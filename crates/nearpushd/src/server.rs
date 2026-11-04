//! HTTP routes: `POST /push`, `GET /ws`, `GET /icons/<sender>` and the embedded page.

use std::net::SocketAddr;
use std::path::PathBuf;
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
use crate::config::SummarizerConfig;
use crate::event::{Event, NewEvent};
use crate::sessions::{SessionUpdate, Sessions};
use crate::store::{Store, now_ms};
use crate::summarizer::Summarizer;

/// Largest accepted `POST /push` body.
pub const MAX_PUSH_BYTES: usize = 16 * 1024;
pub const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(20);

pub const LOGIN_HEADER: &str = "tailscale-user-login";

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
        }
    }

    pub fn with_sessions(mut self, sessions: Sessions) -> Self {
        self.sessions = Arc::new(sessions);
        self
    }

    /// Starts the summarizer worker (needs a Tokio runtime). `workdir` is its cwd.
    pub fn with_summarizer(mut self, cfg: SummarizerConfig, workdir: PathBuf) -> Self {
        if cfg.enabled {
            self.summarizer = Some(Summarizer::spawn(cfg, workdir, self.sessions.clone(), self.updates.clone()));
        }
        self
    }

    pub fn with_icons_dir(mut self, dir: PathBuf) -> Self {
        self.icons_dir = Some(dir);
        self
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/push", post(push))
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

/// `POST /push` authorisation: direct loopback peer, no proxy headers, valid token.
pub fn authorize_push(peer: SocketAddr, headers: &HeaderMap, token: &str) -> Result<(), ApiError> {
    if !peer.ip().is_loopback() {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "push is only accepted from loopback"));
    }
    if is_proxied(headers) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "push is not accepted through a proxy"));
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
    // The prompt only feeds the summarizer; it is never stored with the event.
    let prompt = new.prompt.take();
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
    tracing::info!(id = ev.id, title = %ev.title, "event accepted");
    Ok(Json(PushResponse { id: ev.id, ts: ev.ts }))
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

async fn stream(mut socket: WebSocket, state: AppState, since: u64) {
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
    for ev in &replay {
        if socket.send(text(&Frame::Event { event: ev })).await.is_err() {
            return;
        }
    }
    let synced = Frame::Synced { last_id: replay.last().map(|e| e.id).unwrap_or(since.min(store.last_id())) };
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
                for ev in &batch.events {
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
