//! HTTP routes: `POST /push`, `GET /status`, `POST /control`, `GET /ws`,
//! `GET /icons/<sender>` and the embedded page.
//!
//! Display rules: an event reaches the page only when it is not `muted` (pushed
//! while paused) and is either a generic event (no state) or its state is in the
//! current filter. Hidden events are stored and listed in `/status`, but skipped
//! in both the replay and the live stream, so the main card and the history
//! drawer never show them.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        ConnectInfo, DefaultBodyLimit, Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade, rejection::WebSocketUpgradeRejection},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{MethodRouter, get, post},
};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tokio::sync::broadcast;

use crate::assets;
use crate::config::{DEFAULT_FILTER_STATES, SummarizerConfig};
use crate::control::Control;
use crate::event::{AgentState, Event, NewEvent};
use crate::sessions::{SessionUpdate, Sessions};
use crate::store::{Store, now_ms};
use crate::summarizer::{Summarizer, SummarizerHealth};

/// Largest accepted `POST /push` body.
pub const MAX_PUSH_BYTES: usize = 16 * 1024;
pub const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(20);
/// Events listed in `GET /status`.
pub const STATUS_RECENT: usize = 5;

pub const LOGIN_HEADER: &str = "tailscale-user-login";

/// Where the effective display filter comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterSource {
    /// `[filter] states` in config.toml (or its default).
    Config,
    /// Set through `POST /control`, kept in control.json.
    Control,
}

/// Pause flag and display filter, changed live by `POST /control`.
#[derive(Debug, Clone)]
pub struct ControlState {
    pub paused: bool,
    pub states: Vec<AgentState>,
    pub source: FilterSource,
    /// Where control.json is written; `None` keeps changes in memory only.
    pub state_dir: Option<PathBuf>,
}

impl ControlState {
    fn persisted(&self) -> Control {
        Control {
            paused: self.paused,
            states: (self.source == FilterSource::Control).then(|| self.states.clone()),
        }
    }
}

/// Shared state behind every route.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub token: Arc<str>,
    /// Logins allowed to view. Filled later by discovery when the config left it empty.
    pub allowed_logins: Arc<RwLock<Vec<String>>>,
    pub allow_loopback_without_login: bool,
    pub ping_interval: Duration,
    /// Prompt history and labels per session.
    pub sessions: Arc<Sessions>,
    /// `session_update` frames for every connected page.
    pub updates: broadcast::Sender<SessionUpdate>,
    /// `None` when summarizing is disabled.
    pub summarizer: Option<Summarizer>,
    /// User icon overrides (`~/.config/nearpush/icons`).
    pub icons_dir: Option<PathBuf>,
    /// Pause flag and display filter.
    pub control: Arc<RwLock<ControlState>>,
    /// Open WebSocket connections.
    pub viewers: Arc<AtomicUsize>,
    pub started_at_ms: u64,
    /// The page's public URL, from config or tailscale discovery.
    pub public_url: Arc<RwLock<Option<String>>>,
    pub summarizer_health: Arc<SummarizerHealth>,
    /// Serializes `POST /control` updates.
    control_write: Arc<tokio::sync::Mutex<()>>,
}

impl AppState {
    pub fn new(store: Arc<Store>, token: String, allowed_logins: Vec<String>, allow_loopback_without_login: bool) -> Self {
        AppState {
            store,
            token: token.into(),
            allowed_logins: Arc::new(RwLock::new(allowed_logins)),
            allow_loopback_without_login,
            ping_interval: DEFAULT_PING_INTERVAL,
            sessions: Arc::new(Sessions::in_memory()),
            updates: broadcast::channel(64).0,
            summarizer: None,
            icons_dir: None,
            control: Arc::new(RwLock::new(ControlState {
                paused: false,
                states: DEFAULT_FILTER_STATES.to_vec(),
                source: FilterSource::Config,
                state_dir: None,
            })),
            viewers: Arc::new(AtomicUsize::new(0)),
            started_at_ms: now_ms(),
            public_url: Arc::new(RwLock::new(None)),
            summarizer_health: Arc::new(SummarizerHealth::default()),
            control_write: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub fn with_sessions(mut self, sessions: Sessions) -> Self {
        self.sessions = Arc::new(sessions);
        self
    }

    /// Starts the summarizer worker (needs a Tokio runtime). `workdir` is its cwd.
    pub fn with_summarizer(mut self, cfg: SummarizerConfig, workdir: PathBuf) -> Self {
        if cfg.enabled {
            self.summarizer = Some(Summarizer::spawn(
                cfg,
                workdir,
                self.sessions.clone(),
                self.updates.clone(),
                self.summarizer_health.clone(),
            ));
        }
        self
    }

    pub fn with_icons_dir(mut self, dir: PathBuf) -> Self {
        self.icons_dir = Some(dir);
        self
    }

    /// Effective controls: `control` (from control.json) over `config_states`
    /// (`[filter] states`). Later `/control` changes are saved in `state_dir`.
    pub fn with_control(self, config_states: Vec<AgentState>, control: Control, state_dir: PathBuf) -> Self {
        {
            let mut c = self.control.write().unwrap_or_else(|p| p.into_inner());
            c.paused = control.paused;
            match control.states {
                Some(states) => {
                    c.states = states;
                    c.source = FilterSource::Control;
                }
                None => {
                    c.states = config_states;
                    c.source = FilterSource::Config;
                }
            }
            c.state_dir = Some(state_dir);
        }
        self
    }

    pub fn with_public_url(self, url: Option<String>) -> Self {
        *self.public_url.write().unwrap_or_else(|p| p.into_inner()) = url;
        self
    }

    fn controls(&self) -> ControlState {
        self.control.read().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/push", post(push))
        .route("/status", get(status))
        .route("/control", post(control))
        .route("/ws", get(ws))
        .route("/", get(index))
        .route("/manifest.webmanifest", get(manifest))
        .route("/icon-180.png", icon(assets::ICON_180))
        .route("/apple-touch-icon.png", icon(assets::ICON_180))
        .route("/icon-192.png", icon(assets::ICON_192))
        .route("/icon-512.png", icon(assets::ICON_512))
        .route("/icons/{sender}", get(sender_icon))
        .layer(DefaultBodyLimit::max(MAX_PUSH_BYTES))
        .with_state(state)
}

/// Serves on `listener` until the task is dropped. Events are persisted before they
/// are acknowledged, so stopping abruptly loses nothing.
pub async fn serve(listener: tokio::net::TcpListener, state: AppState) -> std::io::Result<()> {
    axum::serve(listener, router(state).into_make_service_with_connect_info::<SocketAddr>()).await
}

#[derive(Debug)]
pub struct ApiError(StatusCode, String);

impl ApiError {
    fn new(code: StatusCode, msg: impl Into<String>) -> Self {
        ApiError(code, msg.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

/// Headers that mean the request came through a proxy (tailscale serve sets
/// `Tailscale-User-*` and `X-Forwarded-For`). The CLI never sends them.
fn is_proxied(headers: &HeaderMap) -> bool {
    headers.keys().any(|k| {
        let k = k.as_str();
        k.starts_with("tailscale-") || k == "x-forwarded-for" || k == "x-forwarded-host" || k == "forwarded"
    })
}

/// Authorisation for `/push`, `/status` and `/control`: direct loopback peer, no
/// proxy headers, valid token. tailscale serve always adds `Tailscale-*` /
/// `X-Forwarded-For`, so these routes are unreachable from the tailnet.
pub fn authorize_local(peer: SocketAddr, headers: &HeaderMap, token: &str) -> Result<(), ApiError> {
    if !peer.ip().is_loopback() {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "only accepted from loopback"));
    }
    if is_proxied(headers) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "not accepted through a proxy"));
    }
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    match presented {
        Some(p) if bool::from(p.as_bytes().ct_eq(token.as_bytes())) => Ok(()),
        Some(_) => Err(ApiError::new(StatusCode::UNAUTHORIZED, "invalid token")),
        None => Err(ApiError::new(StatusCode::UNAUTHORIZED, "missing bearer token")),
    }
}

/// `POST /push` authorisation; see [`authorize_local`].
pub fn authorize_push(peer: SocketAddr, headers: &HeaderMap, token: &str) -> Result<(), ApiError> {
    authorize_local(peer, headers, token)
}

/// Page and socket authorisation: loopback peer with an allowlisted
/// `Tailscale-User-Login`, or a header-less loopback request when the local-testing
/// flag is on.
pub fn authorize_viewer(state: &AppState, peer: SocketAddr, headers: &HeaderMap) -> Result<(), ApiError> {
    if !peer.ip().is_loopback() {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "not reachable from this address"));
    }
    let login = headers.get(LOGIN_HEADER).map(|v| v.to_str().unwrap_or(""));
    match login {
        Some(login) => {
            let allowed = state.allowed_logins.read().unwrap_or_else(|p| p.into_inner());
            if allowed.iter().any(|a| a.eq_ignore_ascii_case(login.trim())) {
                Ok(())
            } else {
                tracing::warn!(login, "rejected tailnet login not in allowed_logins");
                Err(ApiError::new(StatusCode::FORBIDDEN, "this tailnet login is not allowed"))
            }
        }
        None if state.allow_loopback_without_login => Ok(()),
        None => Err(ApiError::new(StatusCode::UNAUTHORIZED, "open this page through tailscale serve")),
    }
}

/// Rejects cross-site WebSocket opens: a browser `Origin` must name the same host
/// the request was sent to. Clients that send no `Origin` (websocat, tests) pass.
pub fn check_origin(headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(origin) = headers.get(header::ORIGIN) else { return Ok(()) };
    let origin = origin.to_str().unwrap_or("");
    let origin_host = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .unwrap_or("");
    let matches = |name| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|h| !origin_host.is_empty() && h.eq_ignore_ascii_case(origin_host))
    };
    if matches(header::HOST) || matches(header::HeaderName::from_static("x-forwarded-host")) {
        Ok(())
    } else {
        tracing::warn!(origin, "rejected cross-origin websocket");
        Err(ApiError::new(StatusCode::FORBIDDEN, "cross-origin websocket"))
    }
}

#[derive(Serialize)]
struct PushResponse {
    id: u64,
    ts: u64,
}

async fn push(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<PushResponse>, ApiError> {
    authorize_push(peer, &headers, &state.token)?;
    let new: NewEvent = serde_json::from_slice(&body)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, format!("invalid payload: {e}")))?;
    let mut new = new.validate(now_ms()).map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e))?;
    // Paused (DND): stored and marked muted, never broadcast or replayed.
    new.muted = state.control.read().unwrap_or_else(|p| p.into_inner()).paused;
    // The prompt only feeds the summarizer; it is never stored with the event, and
    // it is not recorded at all when the summarizer is off.
    let prompt = new.prompt.take().filter(|_| state.summarizer.is_some());
    let session = new.session.clone();
    if let Some(label) = session.as_deref().and_then(|s| state.sessions.label(s)) {
        // A summarized label replaces the hook's provisional one.
        new.session_label = Some(label);
    }
    let store = state.store.clone();
    let sessions = state.sessions.clone();
    let (ev, summarize) = tokio::task::spawn_blocking(move || {
        let ev = store.push(new)?;
        let summarize = match (session.as_deref(), prompt.as_deref()) {
            (Some(s), Some(p)) => sessions.record_prompt(s, p, ev.ts),
            _ => false,
        };
        Ok::<_, std::io::Error>((ev, summarize))
    })
    .await
    .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| {
        tracing::error!(error = %e, "failed to persist event");
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("failed to persist event: {e}"))
    })?;
    if summarize {
        if let (Some(sum), Some(s)) = (&state.summarizer, ev.session.as_deref()) {
            sum.request(s);
        }
    }
    tracing::info!(id = ev.id, title = %ev.title, muted = ev.muted, "event accepted");
    Ok(Json(PushResponse { id: ev.id, ts: ev.ts }))
}

#[derive(Serialize)]
struct FilterView {
    states: Vec<AgentState>,
    source: FilterSource,
}

#[derive(Serialize)]
struct ControlView {
    paused: bool,
    filter: FilterView,
}

impl From<&ControlState> for ControlView {
    fn from(c: &ControlState) -> Self {
        ControlView { paused: c.paused, filter: FilterView { states: c.states.clone(), source: c.source } }
    }
}

#[derive(Serialize)]
struct SummarizerView {
    enabled: bool,
    healthy: bool,
    last_ok_ms: u64,
    last_error_ms: u64,
}

#[derive(Serialize)]
struct RecentView<'a> {
    id: u64,
    ts: u64,
    title: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<AgentState>,
    level: crate::event::Level,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_label: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a str>,
    /// Whether the page showed it (false: filtered out or muted).
    shown: bool,
}

#[derive(Serialize)]
struct StatusView<'a> {
    version: &'static str,
    ui_version: &'static str,
    pid: u32,
    exe: Option<String>,
    started_at_ms: u64,
    uptime_secs: u64,
    viewers: usize,
    last_event_id: u64,
    paused: bool,
    filter: FilterView,
    summarizer: SummarizerView,
    public_url: Option<String>,
    recent: Vec<RecentView<'a>>,
}

/// `GET /status`: loopback + token, for the menu bar app.
async fn status(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    authorize_local(peer, &headers, &state.token)?;
    let c = state.controls();
    let recent = state.store.recent(STATUS_RECENT);
    let now = now_ms();
    let enabled = state.summarizer.is_some();
    let h = &state.summarizer_health;
    let body = StatusView {
        version: env!("CARGO_PKG_VERSION"),
        ui_version: assets::ui_version(),
        pid: std::process::id(),
        exe: std::env::current_exe().ok().map(|p| p.display().to_string()),
        started_at_ms: state.started_at_ms,
        uptime_secs: now.saturating_sub(state.started_at_ms) / 1000,
        viewers: state.viewers.load(Ordering::Relaxed),
        last_event_id: state.store.last_id(),
        paused: c.paused,
        summarizer: SummarizerView {
            enabled,
            healthy: enabled && h.healthy(),
            last_ok_ms: h.last_ok_ms.load(Ordering::Relaxed),
            last_error_ms: h.last_err_ms.load(Ordering::Relaxed),
        },
        public_url: state.public_url.read().unwrap_or_else(|p| p.into_inner()).clone(),
        recent: recent
            .iter()
            .map(|e| RecentView {
                id: e.id,
                ts: e.ts,
                title: &e.title,
                state: e.state,
                level: e.level,
                session: e.session.as_deref(),
                session_label: e.session_label.as_deref(),
                project: e.project.as_deref(),
                detail: e.detail.as_deref(),
                shown: e.visible(&c.states),
            })
            .collect(),
        filter: FilterView { states: c.states, source: c.source },
    };
    Ok(Json(body).into_response())
}

/// Body of `POST /control`; at least one field is required.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlRequest {
    #[serde(default)]
    paused: Option<bool>,
    #[serde(default)]
    states: Option<Vec<AgentState>>,
}

/// `POST /control`: loopback + token. Changes apply to open sockets at once and
/// are saved to control.json before they take effect.
async fn control(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<ControlView>, ApiError> {
    authorize_local(peer, &headers, &state.token)?;
    let req: ControlRequest = serde_json::from_slice(&body)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, format!("invalid payload: {e}")))?;
    if req.paused.is_none() && req.states.is_none() {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "set paused and/or states"));
    }
    // One /control at a time, so read-modify-write and the file save cannot interleave.
    let _serial = state.control_write.lock().await;
    let mut next = state.controls();
    if let Some(p) = req.paused {
        next.paused = p;
    }
    if let Some(mut s) = req.states {
        let mut seen = Vec::with_capacity(s.len());
        s.retain(|x| {
            let new = !seen.contains(x);
            seen.push(*x);
            new
        });
        next.states = s;
        next.source = FilterSource::Control;
    }
    if let Some(dir) = next.state_dir.clone() {
        let persisted = next.persisted();
        tokio::task::spawn_blocking(move || persisted.save(&dir))
            .await
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot save control.json: {e}")))?;
    }
    let view = ControlView::from(&next);
    tracing::info!(paused = next.paused, states = ?next.states, "controls changed");
    *state.control.write().unwrap_or_else(|p| p.into_inner()) = next;
    Ok(Json(view))
}

fn page_headers(content_type: &'static str, cache: &'static str) -> [(header::HeaderName, HeaderValue); 5] {
    [
        (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
        (header::CACHE_CONTROL, HeaderValue::from_static(cache)),
        (header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
        (header::REFERRER_POLICY, HeaderValue::from_static("no-referrer")),
        (
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
                 connect-src 'self' wss: ws:; img-src 'self'; manifest-src 'self'; \
                 base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
            ),
        ),
    ]
}

async fn index(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    authorize_viewer(&state, peer, &headers)?;
    // no-store so a stale page cannot keep reloading against a newer daemon.
    Ok((page_headers("text/html; charset=utf-8", "no-store"), assets::index_html()).into_response())
}

async fn manifest(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    authorize_viewer(&state, peer, &headers)?;
    Ok((page_headers("application/manifest+json", "no-cache"), assets::MANIFEST).into_response())
}

fn icon(bytes: &'static [u8]) -> MethodRouter<AppState> {
    get(
        move |State(state): State<AppState>, ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap| async move {
            authorize_viewer(&state, peer, &headers)?;
            Ok::<_, ApiError>((page_headers("image/png", "public, max-age=86400"), bytes).into_response())
        },
    )
}

async fn sender_icon(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(sender): Path<String>,
) -> Result<Response, ApiError> {
    authorize_viewer(&state, peer, &headers)?;
    let sender = sender.strip_suffix(".svg").unwrap_or(&sender);
    let icon = assets::resolve_icon(state.icons_dir.as_deref(), sender)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "unknown sender"))?;
    Ok((page_headers(icon.content_type, "public, max-age=3600"), icon.bytes.into_owned()).into_response())
}

#[derive(Deserialize)]
struct WsParams {
    #[serde(default)]
    since: u64,
}

/// Frames sent to the page.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Frame<'a> {
    /// `missed`: events after the client's `since` that are no longer buffered.
    /// `labels`: known session labels, so a reloaded page shows summarized labels.
    Hello { version: &'a str, last_id: u64, oldest_id: u64, missed: u64, server_time: u64, labels: &'a [SessionUpdate] },
    Event { event: &'a Event },
    /// The connection lagged past the ring buffer; this many events were skipped.
    Missed { missed: u64 },
    /// A session's label changed; update cards of that session in place.
    SessionUpdate { session: &'a str, label: &'a str },
    /// Replay finished; everything after this is live.
    Synced { last_id: u64 },
    /// Application-level heartbeat so the page can detect a dead socket.
    Ping { server_time: u64 },
}

fn text(frame: &Frame<'_>) -> Message {
    Message::Text(serde_json::to_string(frame).expect("frame serialises").into())
}

async fn ws(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(params): Query<WsParams>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, ApiError> {
    authorize_viewer(&state, peer, &headers)?;
    check_origin(&headers)?;
    let upgrade = upgrade.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))?;
    Ok(upgrade.on_upgrade(move |socket| stream(socket, state, params.since)))
}

/// Counts an open socket for `/status` while alive.
struct ViewerGuard(Arc<AtomicUsize>);

impl ViewerGuard {
    fn new(n: &Arc<AtomicUsize>) -> Self {
        n.fetch_add(1, Ordering::Relaxed);
        ViewerGuard(n.clone())
    }
}

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Whether the page gets `ev` under the current controls (read per event, so a
/// `/control` change applies to open sockets at once).
fn shown(state: &AppState, ev: &Event) -> bool {
    ev.visible(&state.control.read().unwrap_or_else(|p| p.into_inner()).states)
}

async fn stream(mut socket: WebSocket, state: AppState, since: u64) {
    let _viewer = ViewerGuard::new(&state.viewers);
    let store = state.store.clone();
    let mut updates = state.updates.subscribe();
    let (replay, missed, mut sub) = store.subscribe(since);
    let labels = state.sessions.labels();
    let hello = Frame::Hello {
        version: assets::ui_version(),
        last_id: store.last_id(),
        oldest_id: store.oldest_id(),
        missed,
        server_time: now_ms(),
        labels: &labels,
    };
    if socket.send(text(&hello)).await.is_err() {
        return;
    }
    for ev in replay.iter().filter(|ev| shown(&state, ev)) {
        if socket.send(text(&Frame::Event { event: ev })).await.is_err() {
            return;
        }
    }
    // The newest stored id, shown or not, so hidden ids never count as missed.
    let synced = Frame::Synced { last_id: sub.cursor() };
    if socket.send(text(&synced)).await.is_err() {
        return;
    }

    let mut ticker = tokio::time::interval(state.ping_interval);
    ticker.tick().await;
    loop {
        tokio::select! {
            batch = sub.recv(&store) => {
                let Some(batch) = batch else { break };
                if batch.missed > 0 && socket.send(text(&Frame::Missed { missed: batch.missed })).await.is_err() {
                    return;
                }
                for ev in batch.events.iter().filter(|ev| shown(&state, ev)) {
                    if socket.send(text(&Frame::Event { event: ev })).await.is_err() {
                        return;
                    }
                }
            }
            update = updates.recv() => {
                match update {
                    Ok(u) => {
                        let f = Frame::SessionUpdate { session: &u.session, label: &u.label };
                        if socket.send(text(&f)).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {}
                }
            }
            _ = ticker.tick() => {
                if socket.send(Message::Ping(Bytes::from_static(b"np"))).await.is_err()
                    || socket.send(text(&Frame::Ping { server_time: now_ms() })).await.is_err()
                {
                    return;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(*k, HeaderValue::from_static(v));
        }
        m
    }

    #[test]
    fn origin_must_match_host() {
        assert!(check_origin(&h(&[("host", "lap.ts.net")])).is_ok());
        assert!(check_origin(&h(&[("host", "lap.ts.net"), ("origin", "https://lap.ts.net")])).is_ok());
        assert!(check_origin(&h(&[("host", "lap.ts.net"), ("origin", "https://evil.example")])).is_err());
        assert!(check_origin(&h(&[("host", "lap.ts.net"), ("origin", "null")])).is_err());
    }

    #[test]
    fn push_rejects_proxy_markers() {
        let lo: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let ok = h(&[("authorization", "Bearer t")]);
        assert!(authorize_push(lo, &ok, "t").is_ok());
        for marker in ["tailscale-user-login", "tailscale-user-name", "x-forwarded-for", "forwarded"] {
            let mut hm = ok.clone();
            hm.insert(header::HeaderName::from_static(marker), HeaderValue::from_static("x"));
            assert_eq!(authorize_push(lo, &hm, "t").unwrap_err().0, StatusCode::FORBIDDEN, "{marker}");
        }
        let remote: SocketAddr = "100.64.0.9:5000".parse().unwrap();
        assert_eq!(authorize_push(remote, &ok, "t").unwrap_err().0, StatusCode::FORBIDDEN);
        assert_eq!(authorize_push(lo, &h(&[("authorization", "Bearer x")]), "t").unwrap_err().0, StatusCode::UNAUTHORIZED);
        assert_eq!(authorize_push(lo, &HeaderMap::new(), "t").unwrap_err().0, StatusCode::UNAUTHORIZED);
    }
}
