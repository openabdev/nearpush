//! HTTP routes: `POST /push`, `GET /status`, `POST /control`, `GET /ws`,
//! `POST /v1/dismiss`, `POST /v1/undismiss`, `GET /icons/<sender>` and the embedded page.
//!
//! Display rules: an event reaches the page only when it is not `muted` (pushed
//! while paused) and is either a generic event (no state) or its state is in the
//! current filter. Hidden events are stored and listed in `/status`, but skipped
//! in both the replay and the live stream, so the main card and the history
//! drawer never show them.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
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
use crate::classifier::{ClassifierConfig, ClassifierRuntime, Provider, VerdictSource, apply_verdict};
use crate::config::{DEFAULT_FILTER_STATES, SummarizerConfig};
use crate::control::Control;
use crate::event::{AgentState, Event, NewEvent, TurnClassify};
use crate::main_card::{Dismissal, MainState, MainView, PendingCard, Subject, Target, UndoError};
use crate::relay::{RelayHealth, RelayView};
use crate::sessions::{SessionUpdate, Sessions};
use crate::store::{Store, now_ms};
use crate::summarizer::{Summarizer, SummarizerHealth};

/// Largest accepted `POST /push` body.
pub const MAX_PUSH_BYTES: usize = 16 * 1024;
pub const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(20);
/// Events listed in `GET /status`.
pub const STATUS_RECENT: usize = 5;

pub const LOGIN_HEADER: &str = "tailscale-user-login";

/// How long a dismissal can be undone. The page shows its Undo toast for 5 s; the extra
/// second covers the round trip.
pub const DEFAULT_UNDO_WINDOW: Duration = Duration::from_secs(6);
/// `POST /v1/dismiss` and `/v1/undismiss` calls allowed per caller per minute.
pub const DISMISS_PER_MINUTE: usize = 10;
const RATE_WINDOW_MS: u64 = 60_000;

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
    /// Classifier provider in use.
    pub classifier: Provider,
    pub classifier_source: ProviderSource,
    /// Where control.json is written; `None` keeps changes in memory only.
    pub state_dir: Option<PathBuf>,
}

impl ControlState {
    fn persisted(&self, m: &MainState) -> Control {
        Control {
            paused: self.paused,
            states: (self.source == FilterSource::Control).then(|| self.states.clone()),
            classifier: (self.classifier_source == ProviderSource::Control).then_some(self.classifier),
            pending: m.cards(),
            hidden_through: m.hidden_through(),
            dismissing: m.dismissals(),
        }
    }
}

/// Where the effective classifier provider comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderSource {
    /// `[classifier] provider` in config.toml.
    Config,
    /// Set through `POST /control`, kept in control.json.
    Control,
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
    /// `main_card` frames for every connected page (`None` hands the card back to the stream).
    pub main_updates: broadcast::Sender<Option<MainView>>,
    /// Sessions waiting on the user, and the working card; see `main_card`.
    pub main: Arc<Mutex<MainState>>,
    /// `hidden` frames: stored events up to this id are hidden from the main card.
    pub hidden_updates: broadcast::Sender<Option<u64>>,
    /// How long a dismissal can be undone before it becomes final.
    pub undo_window: Duration,
    /// Recent dismiss/undismiss calls per caller, for the rate limit.
    dismiss_rate: Arc<Mutex<HashMap<String, VecDeque<u64>>>>,
    /// `None` when summarizing is disabled.
    pub summarizer: Option<Summarizer>,
    /// `None` when the classifier is disabled.
    pub classifier: Option<Arc<ClassifierRuntime>>,
    /// The `[classifier]` section as loaded, also when it is disabled (for `/status`).
    pub classifier_config: ClassifierConfig,
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
    /// Relay puller state for `/status`.
    pub relay: Arc<RelayHealth>,
    /// Serializes `POST /control` updates.
    control_write: Arc<tokio::sync::Mutex<()>>,
    /// The latest held Stop of each session. A later held Stop of the same session waits
    /// for it, so the cards commit in the order the Stops arrived.
    stop_chain: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Receiver<()>>>>,
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
            main_updates: broadcast::channel(64).0,
            main: Arc::new(Mutex::new(MainState::default())),
            hidden_updates: broadcast::channel(64).0,
            undo_window: DEFAULT_UNDO_WINDOW,
            dismiss_rate: Arc::new(Mutex::new(HashMap::new())),
            summarizer: None,
            classifier: None,
            classifier_config: ClassifierConfig::default(),
            icons_dir: None,
            control: Arc::new(RwLock::new(ControlState {
                paused: false,
                states: DEFAULT_FILTER_STATES.to_vec(),
                source: FilterSource::Config,
                classifier: Provider::default(),
                classifier_source: ProviderSource::Config,
                state_dir: None,
            })),
            viewers: Arc::new(AtomicUsize::new(0)),
            started_at_ms: now_ms(),
            public_url: Arc::new(RwLock::new(None)),
            summarizer_health: Arc::new(SummarizerHealth::default()),
            relay: Arc::new(RelayHealth::default()),
            control_write: Arc::new(tokio::sync::Mutex::new(())),
            stop_chain: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_sessions(mut self, sessions: Sessions) -> Self {
        self.sessions = Arc::new(sessions);
        self
    }

    /// Restores the pending sessions saved in control.json.
    pub fn with_pending(mut self, cards: Vec<PendingCard>) -> Self {
        self.main = Arc::new(Mutex::new(MainState::from_cards(cards)));
        self
    }

    /// Restores the hidden mark and the open dismissals saved in control.json. The
    /// dismissals become final (with their history record) when the server starts.
    pub fn with_dismissals(self, hidden_through: Option<u64>, dismissing: Vec<Dismissal>) -> Self {
        self.main.lock().unwrap_or_else(|p| p.into_inner()).restore_dismissals(hidden_through, dismissing);
        self
    }

    pub fn with_undo_window(mut self, window: Duration) -> Self {
        self.undo_window = window;
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

    /// Starts the classifier with the default 15 s hold, if `cfg.enabled`.
    pub fn with_classifier(self, cfg: ClassifierConfig, workdir: PathBuf) -> Self {
        self.with_classifier_hold(cfg, workdir, crate::classifier::DEFAULT_HOLD)
    }

    /// Like [`AppState::with_classifier`] with an explicit card hold.
    pub fn with_classifier_hold(mut self, cfg: ClassifierConfig, workdir: PathBuf, hold: Duration) -> Self {
        if cfg.enabled {
            let health = Arc::new(SummarizerHealth::default());
            self.classifier = Some(Arc::new(ClassifierRuntime::build(cfg.clone(), workdir, health, hold)));
        }
        self.classifier_config = cfg;
        self
    }

    /// Effective controls: `control` (from control.json) over `config_states`
    /// (`[filter] states`) and `config_classifier` (`[classifier] provider`). Later
    /// `/control` changes are saved in `state_dir`.
    pub fn with_control(
        self,
        config_states: Vec<AgentState>,
        config_classifier: Provider,
        control: Control,
        state_dir: PathBuf,
    ) -> Self {
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
            match control.classifier {
                Some(p) => {
                    c.classifier = p;
                    c.classifier_source = ProviderSource::Control;
                }
                None => {
                    c.classifier = config_classifier;
                    c.classifier_source = ProviderSource::Config;
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
        .route("/v1/dismiss", post(dismiss))
        .route("/v1/undismiss", post(undismiss))
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
    finish_restored_dismissals(&state).await;
    axum::serve(listener, router(state).into_make_service_with_connect_info::<SocketAddr>()).await
}

#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub String);

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
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
    ts: u64,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    deferred: bool,
}

/// Whether a Stop card is classified: a your_turn or all_set state and a reply tail to
/// read.
fn classifiable(new: &NewEvent, classify: &TurnClassify) -> bool {
    matches!(new.state, Some(AgentState::YourTurn | AgentState::AllSet))
        && !classify.reply_tail.trim().is_empty()
}

async fn push(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    authorize_push(peer, &headers, &state.token)?;
    let new: NewEvent = serde_json::from_slice(&body)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, format!("invalid payload: {e}")))?;
    let new = new.validate(now_ms()).map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e))?;
    ingest(state, new).await
}

/// The card pipeline behind `POST /push` and the relay puller: session end, turn start,
/// the turn-end classifier, then storage. Both sources take the same path.
async fn ingest(state: AppState, mut new: NewEvent) -> Result<Response, ApiError> {
    let classify = new.classify.take();
    let prompt = new.prompt.take();
    let stop = classify.is_some();

    // SessionEnd: the session no longer waits on the user. Nothing is stored.
    if new.session_end {
        if let Some(session) = new.session.as_deref() {
            resolve_session(&state, session).await;
        }
        return Ok(Json(PushResponse { id: None, ts: now_ms(), deferred: false }).into_response());
    }

    // A UserPromptSubmit of a pending session answers it: the card is not stored or
    // listed in history, and the main card follows the pending list.
    if new.turn_start {
        if let Some(session) = new.session.clone() {
            let started_at = new.started_at.unwrap_or_else(now_ms);
            let answered = state.main.lock().unwrap_or_else(|p| p.into_inner()).answered(&session, Some(started_at));
            if answered {
                note_prompt(&state, &session, prompt, now_ms()).await?;
                persist_main(&state).await;
                main_changed(&state).await;
                return Ok(Json(PushResponse { id: None, ts: now_ms(), deferred: false }).into_response());
            }
        }
    }

    if let Some(runtime) = state.classifier.clone() {
        if let Some(classify) = classify.filter(|c| classifiable(&new, c)) {
            let provider = state.controls().classifier;
            let order = stop_order(&state, new.session.as_deref());
            defer_verdict(state.clone(), runtime, provider, new, classify, prompt, order);
            let body = PushResponse { id: None, ts: now_ms(), deferred: true };
            return Ok((StatusCode::ACCEPTED, Json(body)).into_response());
        }
    }

    if stop {
        // Not sent to a provider: the rules verdict stands.
        record_verdict(&mut new, None, VerdictSource::Rules);
    }
    let ev = commit(&state, new, prompt).await?;
    Ok(Json(PushResponse { id: Some(ev.id), ts: ev.ts, deferred: false }).into_response())
}

/// Stores the turn-end record of a Stop card: the provider that was asked (`None` when
/// none ran), where the verdict came from, and the question line of a `your_turn` card.
fn record_verdict(new: &mut NewEvent, provider: Option<Provider>, source: VerdictSource) {
    new.classify_provider = provider.map(|p| p.as_str().to_string());
    new.verdict_source = Some(source.as_str().to_string());
    let asks = new.state == Some(AgentState::YourTurn);
    new.question_line = if asks { new.detail.clone() } else { None };
}

/// Takes this held Stop's place in its session's commit order. The returned receiver
/// stays pending until the previous held Stop of the session has committed (or failed).
/// The sender is kept by the new Stop, so dropping it marks this Stop as done.
fn stop_order(state: &AppState, session: Option<&str>) -> (Option<tokio::sync::oneshot::Receiver<()>>, tokio::sync::oneshot::Sender<()>) {
    let (done, rx) = tokio::sync::oneshot::channel();
    let prev = session.and_then(|s| {
        state.stop_chain.lock().unwrap_or_else(|p| p.into_inner()).insert(s.to_string(), rx)
    });
    (prev, done)
}

/// Holds a classified Stop card while the provider decides, then commits it. The
/// verdict replaces the rules state; a failure, a missing key or an expired hold
/// commits the rules card unchanged. The commit waits for the session's earlier held
/// Stop, so cards of one session commit in arrival order.
fn defer_verdict(
    state: AppState,
    runtime: Arc<ClassifierRuntime>,
    provider: Provider,
    mut new: NewEvent,
    classify: TurnClassify,
    prompt: Option<String>,
    (prev, done): (Option<tokio::sync::oneshot::Receiver<()>>, tokio::sync::oneshot::Sender<()>),
) {
    tokio::spawn(async move {
        let hold = runtime.hold;
        let mut jev_noul = None;
        let source = match tokio::time::timeout(hold, runtime.decide(provider, &classify.reply_tail)).await {
            Ok(Ok(c)) => {
                jev_noul = c.noul;
                apply_verdict(&mut new, c.verdict, &classify)
            }
            Ok(Err(e)) => {
                tracing::warn!(provider = provider.as_str(), error = e.class, "classifier unavailable; using rules verdict");
                tracing::debug!(provider = provider.as_str(), detail = %e.detail, "classifier error detail");
                VerdictSource::Fallback
            }
            Err(_) => {
                runtime.health.err();
                tracing::warn!(provider = provider.as_str(), error = "hold-expired", "classifier unavailable; using rules verdict");
                VerdictSource::Fallback
            }
        };
        record_verdict(&mut new, Some(provider), source);
        new.jev_noul = jev_noul;
        if let Some(prev) = prev {
            // An error means the earlier Stop's sender was dropped, which is also "done".
            let _ = prev.await;
        }
        if let Err(e) = commit(&state, new, prompt).await {
            tracing::error!(error = %e.1, "failed to commit a classified event");
        }
        drop(done);
    });
}

/// Records a prompt for the session summarizer, and queues a run when the record asks.
async fn note_prompt(state: &AppState, session: &str, prompt: Option<String>, ts: u64) -> Result<(), ApiError> {
    let Some(prompt) = prompt.filter(|_| state.summarizer.is_some()) else { return Ok(()) };
    let sessions = state.sessions.clone();
    let key = session.to_string();
    let summarize = tokio::task::spawn_blocking(move || sessions.record_prompt(&key, &prompt, ts))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if let (true, Some(sum)) = (summarize, &state.summarizer) {
        sum.request(session);
    }
    Ok(())
}

/// Stores and publishes an event: muted when paused, with the session label
/// replaced by its summarized one, and queues the summarizer when the prompt asks.
async fn commit(state: &AppState, mut new: NewEvent, prompt: Option<String>) -> Result<Arc<Event>, ApiError> {
    // Paused (DND): stored and marked muted, never broadcast or replayed.
    new.muted = state.control.read().unwrap_or_else(|p| p.into_inner()).paused;
    // The prompt only feeds the summarizer; it is never stored with the event, and
    // it is not recorded at all when the summarizer is off.
    let prompt = prompt.filter(|_| state.summarizer.is_some());
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
    track_main(state, &ev).await;
    if summarize {
        if let (Some(sum), Some(s)) = (&state.summarizer, ev.session.as_deref()) {
            sum.request(s);
        }
    }
    tracing::info!(id = ev.id, title = %ev.title, muted = ev.muted, "event accepted");
    Ok(ev)
}

/// Keeps the pending list in step with a stored Stop. A `your_turn` makes its session
/// pending; an `all_set` resolves it. Muted events count too, so pending stays right while paused.
async fn track_main(state: &AppState, ev: &Event) {
    let Some(session) = ev.session.as_deref() else { return };
    match ev.state {
        Some(AgentState::YourTurn) => {
            let card = PendingCard {
                session: session.to_string(),
                ts: ev.ts,
                question_line: ev.detail.clone(),
                project: ev.project.clone(),
                session_label: ev.session_label.clone(),
                host: ev.host.clone(),
                app: ev.app.clone(),
                sender: ev.sender.clone(),
            };
            state.main.lock().unwrap_or_else(|p| p.into_inner()).your_turn(card);
            persist_main(state).await;
            main_changed(state).await;
        }
        Some(AgentState::AllSet) => resolve_session(state, session).await,
        _ => {}
    }
}

/// The session stopped waiting on the user (SessionEnd, or an all_set Stop).
async fn resolve_session(state: &AppState, session: &str) {
    let changed = state.main.lock().unwrap_or_else(|p| p.into_inner()).resolve(session);
    if changed {
        persist_main(state).await;
        main_changed(state).await;
    }
}

/// Saves the pending sessions in control.json (0600), so they survive a restart.
async fn persist_main(state: &AppState) {
    // Shares the control.json lock with `POST /control`, so neither write drops the other's data.
    let _serial = state.control_write.lock().await;
    let c = state.controls();
    let Some(dir) = c.state_dir.clone() else { return };
    let persisted = c.persisted(&state.main.lock().unwrap_or_else(|p| p.into_inner()));
    match tokio::task::spawn_blocking(move || persisted.save(&dir)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "could not save pending sessions"),
        Err(e) => tracing::warn!(error = %e, "could not save pending sessions"),
    }
}

/// The card to show now, with the session's summarized label.
fn current_main(state: &AppState) -> Option<MainView> {
    view_with_label(&state.main.lock().unwrap_or_else(|p| p.into_inner()), &state.sessions)
}

fn view_with_label(m: &MainState, sessions: &Sessions) -> Option<MainView> {
    let mut v = m.view()?;
    if let Some(label) = sessions.label(&v.session) {
        v.session_label = Some(label);
    }
    Some(v)
}

/// Recomputes the main card and broadcasts it when it changed. While paused nothing is
/// sent; the change goes out on the first refresh after resume.
async fn main_changed(state: &AppState) {
    if state.controls().paused {
        return;
    }
    let mut m = state.main.lock().unwrap_or_else(|p| p.into_inner());
    let view = view_with_label(&m, &state.sessions);
    if m.sent == view {
        return;
    }
    m.sent = view.clone();
    let _ = state.main_updates.send(view);
}

/// Runs a validated relay event through the same card pipeline as `POST /push`, so a
/// relay Stop gets the same turn-end classifier and pending-list handling as a local one.
/// The relay puller calls this; the response is not needed.
pub async fn accept_event(state: &AppState, new: NewEvent) -> Result<(), ApiError> {
    ingest(state.clone(), new).await.map(drop)
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
struct ClassifierView {
    enabled: bool,
    provider: &'static str,
    model: &'static str,
    source: ProviderSource,
    threshold: f64,
    key_service: &'static str,
    /// `None` until the first check of the selected provider's key.
    key_present: Option<bool>,
    healthy: bool,
    last_ok_ms: u64,
    last_error_ms: u64,
}

#[derive(Serialize)]
struct PendingView {
    session: String,
    ts: u64,
}

/// The last four letters and digits of a session id, as the page's session tag shows them.
fn short_session(session: &str) -> String {
    let t: Vec<char> = session.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    t[t.len().saturating_sub(4)..].iter().collect()
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
    #[serde(skip_serializing_if = "Option::is_none")]
    classify_provider: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verdict_source: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    question_line: Option<&'a str>,
    /// Whether the page showed it (false: filtered out, muted, or a dismissal record).
    shown: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    dismissed: bool,
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
    classifier: ClassifierView,
    pending_count: usize,
    pending_your_turn: Vec<PendingView>,
    relay: RelayView,
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
    let cl = state.classifier.as_deref();
    let classifier = ClassifierView {
        enabled: cl.is_some(),
        provider: c.classifier.as_str(),
        model: c.classifier.model(),
        source: c.classifier_source,
        threshold: state.classifier_config.threshold,
        key_service: c.classifier.keychain_service(),
        key_present: cl.and_then(|r| *r.key_present.read().unwrap_or_else(|p| p.into_inner())),
        healthy: cl.is_some_and(|r| r.health.healthy()),
        last_ok_ms: cl.map_or(0, |r| r.health.last_ok_ms.load(Ordering::Relaxed)),
        last_error_ms: cl.map_or(0, |r| r.health.last_err_ms.load(Ordering::Relaxed)),
    };
    let (pending_count, pending_your_turn) = {
        let m = state.main.lock().unwrap_or_else(|p| p.into_inner());
        let entries = m
            .pending_entries()
            .into_iter()
            .map(|(session, ts)| PendingView { session: short_session(&session), ts })
            .collect();
        (m.pending_count(), entries)
    };
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
        classifier,
        pending_count,
        pending_your_turn,
        relay: state.relay.view(),
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
                classify_provider: e.classify_provider.as_deref(),
                verdict_source: e.verdict_source.as_deref(),
                question_line: e.question_line.as_deref(),
                shown: e.visible(&c.states) && !e.dismissed,
                dismissed: e.dismissed,
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
    #[serde(default)]
    classifier: Option<Provider>,
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
    if req.paused.is_none() && req.states.is_none() && req.classifier.is_none() {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "set paused, states and/or classifier"));
    }
    if req.classifier.is_some() && state.classifier.is_none() {
        return Err(ApiError::new(StatusCode::CONFLICT, "classifier is disabled in config.toml"));
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
    if let Some(p) = req.classifier {
        if let Some(rt) = &state.classifier {
            rt.refresh_key(p).await;
        }
        next.classifier = p;
        next.classifier_source = ProviderSource::Control;
        tracing::info!(provider = p.as_str(), "classifier changed");
    }
    if let Some(dir) = next.state_dir.clone() {
        let persisted = next.persisted(&state.main.lock().unwrap_or_else(|p| p.into_inner()));
        tokio::task::spawn_blocking(move || persisted.save(&dir))
            .await
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot save control.json: {e}")))?;
    }
    let view = ControlView::from(&next);
    tracing::info!(paused = next.paused, states = ?next.states, "controls changed");
    *state.control.write().unwrap_or_else(|p| p.into_inner()) = next;
    // A resume sends the card that changed while paused.
    main_changed(&state).await;
    Ok(Json(view))
}

// ---- dismiss ------------------------------------------------------------------

/// Body of `/v1/dismiss` and `/v1/undismiss`: exactly one of `session` (a pending
/// `your_turn` or the `working.` card) or `event_id` (a stored event shown as the card).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DismissRequest {
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    event_id: Option<u64>,
}

fn parse_target(body: &[u8]) -> Result<Target, ApiError> {
    let bad = |m: &str| ApiError::new(StatusCode::BAD_REQUEST, m.to_string());
    let req: DismissRequest = serde_json::from_slice(body)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, format!("invalid payload: {e}")))?;
    match (req.session, req.event_id) {
        (Some(s), None) => {
            let s = s.trim();
            if s.is_empty() || s.chars().count() > crate::event::MAX_SESSION_CHARS || s.chars().any(char::is_control) {
                return Err(bad("session must be 1-128 printable characters"));
            }
            Ok(Target::Session(s.to_string()))
        }
        (None, Some(id)) if id > 0 => Ok(Target::Event(id)),
        (None, Some(_)) => Err(bad("event_id must be positive")),
        _ => Err(bad("send exactly one of session or event_id")),
    }
}

/// Who may dismiss: a tailnet viewer (allowlisted `Tailscale-User-Login`, a same-origin
/// browser `Origin`, a JSON body), or a direct loopback caller with the push token.
/// Returns the rate-limit key.
fn authorize_dismiss(state: &AppState, peer: SocketAddr, headers: &HeaderMap) -> Result<String, ApiError> {
    if let Some(login) = headers.get(LOGIN_HEADER) {
        authorize_viewer(state, peer, headers)?;
        if !headers.contains_key(header::ORIGIN) {
            return Err(ApiError::new(StatusCode::FORBIDDEN, "missing Origin"));
        }
        check_origin(headers).map_err(|_| ApiError::new(StatusCode::FORBIDDEN, "cross-origin request"))?;
        let json = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim().to_ascii_lowercase().starts_with("application/json"));
        if !json {
            return Err(ApiError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, "send application/json"));
        }
        let login = login.to_str().unwrap_or("").trim().to_ascii_lowercase();
        return Ok(format!("login:{login}"));
    }
    authorize_local(peer, headers, &state.token)?;
    check_origin(headers).map_err(|_| ApiError::new(StatusCode::FORBIDDEN, "cross-origin request"))?;
    Ok("local".into())
}

/// At most [`DISMISS_PER_MINUTE`] calls per caller in any 60 s.
fn rate_limit(state: &AppState, key: String) -> Result<(), ApiError> {
    let now = now_ms();
    let mut m = state.dismiss_rate.lock().unwrap_or_else(|p| p.into_inner());
    m.retain(|_, q| q.back().is_some_and(|t| now.saturating_sub(*t) < RATE_WINDOW_MS));
    let q = m.entry(key).or_default();
    while q.front().is_some_and(|t| now.saturating_sub(*t) >= RATE_WINDOW_MS) {
        q.pop_front();
    }
    if q.len() >= DISMISS_PER_MINUTE {
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "too many dismissals; try again in a minute"));
    }
    q.push_back(now);
    Ok(())
}

/// Sends the hidden mark to every page when it changed.
fn hidden_changed(state: &AppState, before: Option<u64>) {
    let now = state.main.lock().unwrap_or_else(|p| p.into_inner()).hidden_through();
    if now != before {
        let _ = state.hidden_updates.send(now);
    }
}

/// `POST /v1/dismiss`: takes the card off the main view. A pending `your_turn` leaves
/// the pending list; a `working.` card or a stored event is hidden. It never creates,
/// edits or pushes an event and never touches `/control` state; the only write besides
/// the pending list is the history record once the undo window has passed.
async fn dismiss(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let key = authorize_dismiss(&state, peer, &headers)?;
    rate_limit(&state, key)?;
    let target = parse_target(&body)?;
    let not_found = || ApiError::new(StatusCode::NOT_FOUND, "no such card to dismiss");
    let event = match &target {
        Target::Event(id) => Some(state.store.get(*id).filter(|e| !e.dismissed).ok_or_else(not_found)?),
        Target::Session(_) => None,
    };
    let (d, before) = {
        let mut m = state.main.lock().unwrap_or_else(|p| p.into_inner());
        let before = m.hidden_through();
        let d = match (&target, event) {
            (Target::Session(s), _) => m.dismiss_session(s, state.store.last_id()),
            (Target::Event(id), Some(e)) => m.dismiss_event(*id, subject_of(&e)),
            (Target::Event(_), None) => None,
        };
        (d.ok_or_else(not_found)?, before)
    };
    persist_main(&state).await;
    main_changed(&state).await;
    hidden_changed(&state, before);
    let window = state.undo_window;
    let (later, id) = (state.clone(), d.id);
    tokio::spawn(async move {
        tokio::time::sleep(window).await;
        let d = later.main.lock().unwrap_or_else(|p| p.into_inner()).finalize(id);
        if let Some(d) = d {
            record_dismissal(&later, d).await;
            persist_main(&later).await;
        }
    });
    tracing::info!(kind = d.what.kind(), "card dismissed");
    let body = serde_json::json!({ "dismissed": d.what.kind(), "undo_ms": window.as_millis() as u64 });
    Ok(Json(body).into_response())
}

/// `POST /v1/undismiss`: undoes the newest dismissal of the same target while its undo
/// window is open. 404 when there is none, 409 when the card changed since.
async fn undismiss(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let key = authorize_dismiss(&state, peer, &headers)?;
    rate_limit(&state, key)?;
    let target = parse_target(&body)?;
    let (res, before) = {
        let mut m = state.main.lock().unwrap_or_else(|p| p.into_inner());
        let before = m.hidden_through();
        (m.undo(&target), before)
    };
    let d = res.map_err(|e| match e {
        UndoError::NotFound => ApiError::new(StatusCode::NOT_FOUND, "nothing to undo"),
        UndoError::Stale => ApiError::new(StatusCode::CONFLICT, "the card changed since it was dismissed"),
    })?;
    persist_main(&state).await;
    main_changed(&state).await;
    hidden_changed(&state, before);
    tracing::info!(kind = d.what.kind(), "dismissal undone");
    Ok(Json(serde_json::json!({ "restored": d.what.kind() })).into_response())
}

fn subject_of(e: &Event) -> Subject {
    Subject {
        session: e.session.clone(),
        session_label: e.session_label.clone(),
        project: e.project.clone(),
        host: e.host.clone(),
        app: e.app.clone(),
        sender: e.sender.clone(),
        detail: Some(e.title.clone()),
    }
}

/// Writes the history record of a final dismissal: a fixed `dismissed` entry that is
/// never a notification (no state, so it touches neither pending nor the classifier).
async fn record_dismissal(state: &AppState, d: Dismissal) {
    let s = d.subject;
    let label = s.session.as_deref().and_then(|x| state.sessions.label(x)).or(s.session_label);
    let new = NewEvent {
        title: "dismissed".into(),
        session: s.session,
        session_label: label,
        project: s.project,
        host: s.host,
        app: s.app,
        sender: s.sender,
        detail: s.detail,
        dismissed: true,
        muted: state.controls().paused,
        ..NewEvent::default()
    };
    let store = state.store.clone();
    match tokio::task::spawn_blocking(move || store.push(new)).await {
        Ok(Ok(ev)) => tracing::info!(id = ev.id, "dismissal recorded"),
        Ok(Err(e)) => tracing::warn!(error = %e, "could not record a dismissal"),
        Err(e) => tracing::warn!(error = %e, "could not record a dismissal"),
    }
}

/// Dismissals still open when the daemon stopped become final now.
pub async fn finish_restored_dismissals(state: &AppState) {
    let open = state.main.lock().unwrap_or_else(|p| p.into_inner()).finalize_all();
    if open.is_empty() {
        return;
    }
    for d in open {
        record_dismissal(state, d).await;
    }
    persist_main(state).await;
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
    /// The main card: the pending `your_turn` or the `working.` card. `null` hands the
    /// card back to the event stream. Not stored, not in history.
    MainCard { card: Option<&'a MainView> },
    /// Stored events up to `through` are hidden from the main card (a dismissed card);
    /// `null` clears it. Sent on connect only when set.
    Hidden { through: Option<u64> },
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
    let mut main = state.main_updates.subscribe();
    let mut hidden = state.hidden_updates.subscribe();
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
    // Only a card that is shown is sent here; the page clears its card on hello.
    if let (false, Some(card)) = (state.controls().paused, current_main(&state)) {
        if socket.send(text(&Frame::MainCard { card: Some(&card) })).await.is_err() {
            return;
        }
    }
    let through = state.main.lock().unwrap_or_else(|p| p.into_inner()).hidden_through();
    if through.is_some() && socket.send(text(&Frame::Hidden { through })).await.is_err() {
        return;
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
            update = main.recv() => {
                let card = match update {
                    Ok(card) => card,
                    Err(broadcast::error::RecvError::Lagged(_)) => current_main(&state),
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if socket.send(text(&Frame::MainCard { card: card.as_ref() })).await.is_err() {
                    return;
                }
            }
            update = hidden.recv() => {
                let through = match update {
                    Ok(t) => t,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        state.main.lock().unwrap_or_else(|p| p.into_inner()).hidden_through()
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if socket.send(text(&Frame::Hidden { through })).await.is_err() {
                    return;
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
        assert!(check_origin(&h(&[("host", "laptop.example.ts.net")])).is_ok());
        assert!(check_origin(&h(&[("host", "laptop.example.ts.net"), ("origin", "https://laptop.example.ts.net")])).is_ok());
        assert!(check_origin(&h(&[("host", "laptop.example.ts.net"), ("origin", "https://evil.example")])).is_err());
        assert!(check_origin(&h(&[("host", "laptop.example.ts.net"), ("origin", "null")])).is_err());
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
        let remote: SocketAddr = "192.0.2.9:5000".parse().unwrap();
        assert_eq!(authorize_push(remote, &ok, "t").unwrap_err().0, StatusCode::FORBIDDEN);
        assert_eq!(authorize_push(lo, &h(&[("authorization", "Bearer x")]), "t").unwrap_err().0, StatusCode::UNAUTHORIZED);
        assert_eq!(authorize_push(lo, &HeaderMap::new(), "t").unwrap_err().0, StatusCode::UNAUTHORIZED);
    }
}
