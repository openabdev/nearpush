//! Relay puller. Long-polls the event relay (`crates/eventrelay`), feeds each event
//! through the same card pipeline as `POST /push` (`server::accept_event`), persists
//! the cursor, and only then acks the batch to the relay.
//!
//! Ordering rule: an event is processed before the cursor covers it, and the cursor is
//! saved before the relay is acked. A crash in between replays the batch, and the
//! relay-id dedupe (cursor plus the ring buffer's `relay_id`s) stops a second card.
//!
//! The consumer token is held in memory only. Neither the token nor a response body is
//! ever logged; errors are logged and reported by class.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::config::RelayConfig;
use nearpush_common::text::{classifier_tail, classify_turn_end, fmt_duration};
use nearpush_common::time::parse_rfc3339_ms;

use crate::event::{AgentState, NewEvent, TurnClassify};
use crate::server::{self, AppState};
use crate::store::now_ms;
use crate::summarizer;

/// Relay ids remembered in memory for dedupe, on top of the cursor.
pub const SEEN_CAP: usize = 4096;
/// Open Kiro turns remembered for the "took" time. Cleared when full, so a turn that
/// never ends cannot grow the map without bound.
const TURNS_CAP: usize = 4096;
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const BATCH_LIMIT: u64 = 500;
const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(10);
/// Request timeout beyond the long-poll wait.
const HTTP_SLACK: Duration = Duration::from_secs(15);

/// What `/status` reports about the puller. Holds no bodies and no tokens.
#[derive(Default)]
pub struct RelayHealth {
    enabled: AtomicBool,
    connected: AtomicBool,
    last_id: AtomicU64,
    last_ok_ms: AtomicU64,
    last_error_class: Mutex<Option<&'static str>>,
}

/// The `relay` object in `GET /status`.
#[derive(Serialize)]
pub struct RelayView {
    pub enabled: bool,
    pub connected: bool,
    pub last_id: u64,
    pub last_ok_ms: u64,
    pub last_error_class: Option<&'static str>,
}

impl RelayHealth {
    pub fn view(&self) -> RelayView {
        RelayView {
            enabled: self.enabled.load(Ordering::Relaxed),
            connected: self.connected.load(Ordering::Relaxed),
            last_id: self.last_id.load(Ordering::Relaxed),
            last_ok_ms: self.last_ok_ms.load(Ordering::Relaxed),
            last_error_class: *self.last_error_class.lock().unwrap_or_else(|p| p.into_inner()),
        }
    }

    fn set_ok(&self, now: u64) {
        self.connected.store(true, Ordering::Relaxed);
        self.last_ok_ms.store(now, Ordering::Relaxed);
        *self.last_error_class.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    fn set_error(&self, class: &'static str) {
        self.connected.store(false, Ordering::Relaxed);
        *self.last_error_class.lock().unwrap_or_else(|p| p.into_inner()) = Some(class);
    }

    fn disable(&self, class: &'static str) {
        self.enabled.store(false, Ordering::Relaxed);
        self.set_error(class);
    }
}

/// One failed relay call. The class is the only thing that gets logged or reported.
#[derive(Debug)]
enum RelayError {
    Unauthorized,
    Network,
    Status(u16),
    BadResponse,
    /// The local store refused an event.
    Store,
    /// The cursor file could not be written.
    Cursor,
}

impl RelayError {
    fn class(&self) -> &'static str {
        match self {
            RelayError::Unauthorized => "unauthorized",
            RelayError::Network => "network",
            RelayError::Status(c) if *c >= 500 => "http_5xx",
            RelayError::Status(_) => "http_4xx",
            RelayError::BadResponse => "bad_response",
            RelayError::Store => "store_error",
            RelayError::Cursor => "cursor_write",
        }
    }
}

#[derive(Deserialize)]
struct Page {
    events: Vec<RelayEvent>,
}

#[derive(Deserialize)]
struct RelayEvent {
    id: u64,
    source: String,
    body: Value,
}

#[derive(Serialize, Deserialize)]
struct CursorFile {
    last_id: u64,
}

/// Relay HTTP client. Holds the consumer token, so it is never printed.
struct Client {
    http: reqwest::Client,
    events_url: String,
    ack_url: String,
    token: String,
    wait_secs: u64,
}

impl Client {
    fn new(cfg: &RelayConfig, token: String) -> Result<Client, String> {
        let base = cfg.url.as_deref().unwrap_or_default().trim_end_matches('/');
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(cfg.wait_secs) + HTTP_SLACK)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Client {
            http,
            events_url: format!("{base}/v1/events"),
            ack_url: format!("{base}/v1/ack"),
            token,
            wait_secs: cfg.wait_secs,
        })
    }

    async fn fetch(&self, since: u64) -> Result<Page, RelayError> {
        let resp = self
            .http
            .get(&self.events_url)
            .bearer_auth(&self.token)
            .query(&[
                ("since", since.to_string()),
                ("wait", self.wait_secs.to_string()),
                ("limit", BATCH_LIMIT.to_string()),
            ])
            .send()
            .await
            .map_err(|_| RelayError::Network)?;
        check(resp.status())?;
        resp.json::<Page>().await.map_err(|_| RelayError::BadResponse)
    }

    async fn ack(&self, through: u64) -> Result<(), RelayError> {
        let resp = self
            .http
            .post(&self.ack_url)
            .bearer_auth(&self.token)
            .json(&json!({ "through": through }))
            .send()
            .await
            .map_err(|_| RelayError::Network)?;
        check(resp.status())
    }
}

fn check(status: reqwest::StatusCode) -> Result<(), RelayError> {
    if status.is_success() {
        Ok(())
    } else if status == reqwest::StatusCode::UNAUTHORIZED {
        Err(RelayError::Unauthorized)
    } else {
        Err(RelayError::Status(status.as_u16()))
    }
}

/// Reads the saved cursor. A missing file means 0; an unreadable one also means 0,
/// and the dedupe in [`run`] then keeps recent events from showing twice.
pub fn load_cursor(path: &Path) -> u64 {
    match fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<CursorFile>(&bytes) {
            Ok(c) => c.last_id,
            Err(_) => {
                tracing::warn!(path = %path.display(), "relay cursor file is corrupt; starting from 0");
                0
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "cannot read relay cursor; starting from 0");
            0
        }
    }
}

/// Writes the cursor through a temp file (created 0600, synced) and renames it into place.
pub fn save_cursor(path: &Path, last_id: u64) -> io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    // A temp file left by a crash would make create_new fail forever.
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    let mut bytes = serde_json::to_vec(&CursorFile { last_id }).map_err(io::Error::other)?;
    bytes.push(b'\n');
    f.write_all(&bytes)?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, path)
}

/// Turns one relay body into the `NewEvent` that `POST /push` would build. The source
/// name becomes `app` when the body names none. A body `session` is prefixed with the
/// source, so two producers cannot share a session. The relay id is recorded for dedupe.
/// Errors are a fixed class, never the body or the serde message.
pub fn to_new_event(source: &str, id: u64, mut body: Value, now_ms: u64) -> Result<NewEvent, &'static str> {
    let obj = body.as_object_mut().ok_or("invalid_event")?;
    if obj.get("app").is_none_or(Value::is_null) {
        obj.insert("app".into(), Value::String(source.to_string()));
    }
    let session = obj.get("session").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    if let Some(s) = session {
        obj.insert("session".into(), Value::String(format!("{source}/{s}")));
    }
    let new: NewEvent = serde_json::from_value(body).map_err(|_| "invalid_event")?;
    let mut new = new.validate(now_ms).map_err(|_| "invalid_event")?;
    new.relay_id = Some(id);
    Ok(new)
}

/// Body the Kiro adapter posts: `{"event", "session_id", "cwd", "reply_tail", "timestamp"}`.
#[derive(Deserialize)]
struct KiroBody {
    event: String,
    session_id: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    reply_tail: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
}

/// Maps an adapter body onto the event the local hook sends for the same Kiro event.
/// `turn_start` is working and starts the turn clock. `turn_end` runs the turn-end rules
/// on `reply_tail`: `your_turn` with the question line, or `all_set` with "took Xm". The
/// classifier path in `accept_event` may still replace that verdict. `session_end` only
/// clears the session. `turns` maps `source/session` to the open turn's start (unix ms).
/// Errors are a fixed class, never the body.
pub fn kiro_to_new_event(
    source: &str,
    id: u64,
    body: Value,
    turns: &mut HashMap<String, u64>,
    now_ms: u64,
) -> Result<NewEvent, &'static str> {
    let b: KiroBody = serde_json::from_value(body).map_err(|_| "invalid_event")?;
    let session = b.session_id.trim();
    if session.is_empty() {
        return Err("invalid_event");
    }
    let key = format!("{source}/{session}");
    let timestamp = || -> Result<u64, &'static str> {
        let t = b.timestamp.as_deref().ok_or("invalid_event")?;
        parse_rfc3339_ms(t).map_err(|_| "invalid_event")
    };
    let project = b
        .cwd
        .as_deref()
        .map(str::trim)
        .and_then(|c| Path::new(c).file_name())
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .map(str::to_string);
    let mut new = NewEvent {
        sender: Some("kiro".into()),
        app: Some("Kiro".into()),
        host: Some(source.to_string()),
        session: Some(session.to_string()),
        project,
        ..Default::default()
    };
    match b.event.as_str() {
        "turn_start" => {
            let start = timestamp()?;
            if turns.len() >= TURNS_CAP {
                turns.clear();
            }
            turns.insert(key, start);
            new.title = "working.".into();
            new.state = Some(AgentState::Working);
            new.started_at = Some(start);
            new.turn_start = true;
        }
        "turn_end" => {
            let end = timestamp()?;
            let start = turns.remove(&key);
            let took_ms = start.map(|s| end.saturating_sub(s));
            let took = took_ms.map(|d| format!("took {}", fmt_duration(d)));
            let reply = b.reply_tail.unwrap_or_default();
            let question = classify_turn_end(&reply);
            match &question {
                Some(q) => {
                    new.title = "your turn.".into();
                    new.state = Some(AgentState::YourTurn);
                    new.detail = Some(q.clone());
                }
                None => {
                    new.title = "all set.".into();
                    new.state = Some(AgentState::AllSet);
                    new.detail = took.clone();
                }
            }
            new.started_at = start;
            new.duration_ms = took_ms;
            if let Some(reply_tail) = classifier_tail(&reply) {
                new.classify = Some(TurnClassify { reply_tail, question_line: question, took });
            }
        }
        "session_end" => {
            turns.remove(&key);
            new.title = "session ended".into();
            new.session_end = true;
        }
        _ => return Err("invalid_event"),
    }
    let mut new = new.validate(now_ms).map_err(|_| "invalid_event")?;
    new.relay_id = Some(id);
    Ok(new)
}

/// Delay before retry `attempt` (1 for the first failure): 1 s, 2 s, 4 s and so on,
/// capped at 60 s, plus up to half a step of jitter.
pub fn backoff_delay(attempt: u32) -> Duration {
    let step = BACKOFF_BASE.saturating_mul(1u32 << attempt.saturating_sub(1).min(6)).min(BACKOFF_MAX);
    let mut raw = [0u8; 8];
    let jitter_ms = match getrandom::fill(&mut raw) {
        Ok(()) => u64::from_le_bytes(raw) % (step.as_millis() as u64 / 2 + 1),
        Err(_) => 0,
    };
    (step + Duration::from_millis(jitter_ms)).min(BACKOFF_MAX)
}

/// Relay ids already processed, bounded to [`SEEN_CAP`] (oldest forgotten first).
#[derive(Default)]
struct Seen {
    ids: HashSet<u64>,
    order: VecDeque<u64>,
}

impl Seen {
    fn insert(&mut self, id: u64) {
        if self.ids.insert(id) {
            self.order.push_back(id);
            if self.order.len() > SEEN_CAP {
                if let Some(old) = self.order.pop_front() {
                    self.ids.remove(&old);
                }
            }
        }
    }

    fn contains(&self, id: u64) -> bool {
        self.ids.contains(&id)
    }
}

/// Records a failed relay call and sleeps before the next try. Returns `false` when the
/// token was rejected, in which case the worker must stop.
async fn after_error(health: &RelayHealth, err: RelayError, failures: &mut u32) -> bool {
    if let RelayError::Unauthorized = err {
        health.disable(err.class());
        tracing::error!(
            "relay rejected the consumer token (HTTP 401); the relay puller is disabled until the daemon restarts with a valid keychain token"
        );
        return false;
    }
    *failures = failures.saturating_add(1);
    health.set_error(err.class());
    let delay = backoff_delay(*failures);
    tracing::warn!(class = err.class(), retry_in_ms = delay.as_millis() as u64, "relay call failed; retrying");
    tokio::time::sleep(delay).await;
    true
}

/// Pulls relay events until the token is rejected. Network and server errors back off
/// and retry; they never end the task.
pub async fn run(state: AppState, cfg: RelayConfig, token: String, cursor_path: PathBuf) {
    let health = state.relay.clone();
    let client = match Client::new(&cfg, token) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "relay disabled: cannot build the HTTP client");
            return;
        }
    };
    let mut cursor = load_cursor(&cursor_path);
    health.last_id.store(cursor, Ordering::Relaxed);
    let mut seen = Seen::default();
    let mut turns: HashMap<String, u64> = HashMap::new();
    for ev in state.store.recent(usize::MAX) {
        if let Some(id) = ev.relay_id {
            seen.insert(id);
        }
    }
    health.enabled.store(true, Ordering::Relaxed);
    tracing::info!(url = cfg.url.as_deref().unwrap_or(""), cursor, "relay puller started");

    let mut pending_ack: Option<u64> = None;
    // Set when the pending ack covers a batch that was fully processed. Only that ack
    // ends the backoff, so a partly failed batch keeps the growing delay.
    let mut ack_clears_backoff = false;
    let mut failures: u32 = 0;
    loop {
        if let Some(through) = pending_ack {
            match client.ack(through).await {
                Ok(()) => {
                    pending_ack = None;
                    if ack_clears_backoff {
                        failures = 0;
                        ack_clears_backoff = false;
                    }
                    health.set_ok(now_ms());
                }
                Err(e) => {
                    if !after_error(&health, e, &mut failures).await {
                        return;
                    }
                    continue;
                }
            }
        }

        let page = match client.fetch(cursor).await {
            Ok(p) => p,
            Err(e) => {
                if !after_error(&health, e, &mut failures).await {
                    return;
                }
                continue;
            }
        };
        health.set_ok(now_ms());

        let mut events = page.events;
        events.sort_by_key(|e| e.id);
        let mut done = cursor;
        let mut failure = None;
        for ev in events {
            if ev.id <= done {
                continue;
            }
            if seen.contains(ev.id) {
                done = ev.id;
                continue;
            }
            // A body with an `event` key is the Kiro adapter's shape; everything else is a push body.
            let converted = if ev.body.get("event").is_some() {
                kiro_to_new_event(&ev.source, ev.id, ev.body, &mut turns, now_ms())
            } else {
                to_new_event(&ev.source, ev.id, ev.body, now_ms())
            };
            match converted {
                Ok(new) => match server::accept_event(&state, new).await {
                    Ok(_) => seen.insert(ev.id),
                    Err(e) => {
                        tracing::error!(id = ev.id, status = e.0.as_u16(), "local store refused a relay event");
                        failure = Some(RelayError::Store);
                        break;
                    }
                },
                Err(class) => tracing::warn!(id = ev.id, class, "skipped relay event"),
            }
            done = ev.id;
        }

        if done > cursor {
            match save_cursor(&cursor_path, done) {
                Ok(()) => {
                    cursor = done;
                    health.last_id.store(cursor, Ordering::Relaxed);
                    pending_ack = Some(cursor);
                    ack_clears_backoff = failure.is_none();
                }
                Err(e) => {
                    tracing::error!(error = %e, "cannot save relay cursor; the batch will be fetched again");
                    failure.get_or_insert(RelayError::Cursor);
                }
            }
        }
        match failure {
            Some(e) => {
                if !after_error(&health, e, &mut failures).await {
                    return;
                }
            }
            None if pending_ack.is_none() => failures = 0,
            None => {}
        }
    }
}

/// Starts the puller in the background when `[relay] enabled` is set. The consumer
/// token is read from the keychain first. A failed lookup logs one warning and the
/// relay stays disabled; startup does not wait for it.
pub fn spawn_from_config(state: AppState, cfg: RelayConfig, cursor_path: PathBuf) -> Option<JoinHandle<()>> {
    if !cfg.enabled {
        return None;
    }
    Some(tokio::spawn(async move {
        match summarizer::keychain_key(&cfg.security_program, &cfg.consumer_token_keychain_service, KEYCHAIN_TIMEOUT).await {
            Ok(token) => run(state, cfg, token, cursor_path).await,
            Err(e) => tracing::warn!(error = %e, "relay disabled: consumer token unavailable"),
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RELAY_CURSOR_FILE;

    #[test]
    fn backoff_grows_then_caps_at_sixty_seconds() {
        for _ in 0..50 {
            let d1 = backoff_delay(1);
            assert!(d1 >= Duration::from_secs(1) && d1 <= Duration::from_millis(1500), "{d1:?}");
            let d2 = backoff_delay(2);
            assert!(d2 >= Duration::from_secs(2) && d2 <= Duration::from_secs(3), "{d2:?}");
            for n in [7, 20, u32::MAX] {
                let d = backoff_delay(n);
                assert!(d >= BACKOFF_MAX && d <= BACKOFF_MAX, "{d:?}");
            }
        }
    }

    #[test]
    fn cursor_round_trip_private_and_corrupt_reads_as_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RELAY_CURSOR_FILE);
        assert_eq!(load_cursor(&path), 0);
        save_cursor(&path, 42).unwrap();
        assert_eq!(load_cursor(&path), 42);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        save_cursor(&path, 43).unwrap();
        assert_eq!(load_cursor(&path), 43);
        fs::write(&path, "{not json").unwrap();
        assert_eq!(load_cursor(&path), 0);
    }

    #[test]
    fn relay_body_maps_to_push_shape() {
        let body = json!({"title": "done", "session": "s1", "state": "all_set"});
        let new = to_new_event("kiro", 9, body, 1).unwrap();
        assert_eq!(new.app.as_deref(), Some("kiro"));
        assert_eq!(new.session.as_deref(), Some("kiro/s1"));
        assert_eq!(new.relay_id, Some(9));

        let new = to_new_event("kiro", 10, json!({"title": "x", "app": "Kiro CLI"}), 1).unwrap();
        assert_eq!(new.app.as_deref(), Some("Kiro CLI"));
        assert_eq!(new.session, None);

        assert_eq!(to_new_event("kiro", 1, json!("not an object"), 1).unwrap_err(), "invalid_event");
        assert_eq!(to_new_event("kiro", 1, json!({"title": "t", "muted": true}), 1).unwrap_err(), "invalid_event");
        assert_eq!(to_new_event("kiro", 1, json!({"title": "t", "relay_id": 5}), 1).unwrap_err(), "invalid_event");
        assert_eq!(to_new_event("kiro", 1, json!({"title": ""}), 1).unwrap_err(), "invalid_event");
    }

    const TURN_START: &str = "2026-01-01T00:00:00Z";
    const TURN_END_3M: &str = "2026-01-01T00:03:00Z";
    const START_MS: u64 = 1_767_225_600_000;
    const NOW_MS: u64 = 1_800_000_000_000;

    fn adapter(event: &str, session: &str, timestamp: Option<&str>, reply: Option<&str>) -> Value {
        let mut body = json!({ "event": event, "session_id": session, "cwd": "/work/demo-app" });
        if let Some(t) = timestamp {
            body["timestamp"] = json!(t);
        }
        if let Some(r) = reply {
            body["reply_tail"] = json!(r);
        }
        body
    }

    #[test]
    fn kiro_turn_start_is_working_from_timestamp() {
        let mut turns = HashMap::new();
        let body = adapter("turn_start", "fake-session-1", Some(TURN_START), None);
        let new = kiro_to_new_event("host-a", 11, body, &mut turns, NOW_MS).unwrap();
        assert_eq!(new.state, Some(AgentState::Working));
        assert_eq!(new.title, "working.");
        assert!(new.turn_start);
        assert_eq!(new.started_at, Some(START_MS));
        assert_eq!(new.project.as_deref(), Some("demo-app"));
        assert_eq!(new.session.as_deref(), Some("fake-session-1"), "session is the adapter's session_id");
        assert_eq!(new.app.as_deref(), Some("Kiro"));
        assert_eq!(new.sender.as_deref(), Some("kiro"));
        assert_eq!(new.host.as_deref(), Some("host-a"));
        assert_eq!(new.relay_id, Some(11));
        assert_eq!(turns.get("host-a/fake-session-1"), Some(&START_MS));
    }

    #[test]
    fn kiro_turn_end_with_question_is_your_turn() {
        let mut turns = HashMap::from([("host-b/fake-session-2".to_string(), START_MS)]);
        let reply = "Fake step finished.\nWhich fake option should I pick?";
        let body = adapter("turn_end", "fake-session-2", Some(TURN_END_3M), Some(reply));
        let new = kiro_to_new_event("host-b", 12, body, &mut turns, NOW_MS).unwrap();
        assert_eq!(new.state, Some(AgentState::YourTurn));
        assert_eq!(new.title, "your turn.");
        assert_eq!(new.detail.as_deref(), Some("Which fake option should I pick?"));
        assert_eq!(new.started_at, Some(START_MS));
        assert_eq!(new.duration_ms, Some(180_000));
        let c = new.classify.as_ref().expect("classify material for the classifier path");
        assert_eq!(c.question_line.as_deref(), Some("Which fake option should I pick?"));
        assert!(c.reply_tail.ends_with("Which fake option should I pick?"));
        assert_eq!(c.took.as_deref(), Some("took 3m"));
        assert!(turns.is_empty(), "the turn is closed");
        assert_eq!(new.host.as_deref(), Some("host-b"));
    }

    #[test]
    fn kiro_turn_end_without_question_is_all_set_with_took() {
        let mut turns = HashMap::from([("host-a/fake-session-3".to_string(), START_MS)]);
        let body = adapter("turn_end", "fake-session-3", Some(TURN_END_3M), Some("Fake work is finished."));
        let new = kiro_to_new_event("host-a", 13, body, &mut turns, NOW_MS).unwrap();
        assert_eq!(new.state, Some(AgentState::AllSet));
        assert_eq!(new.title, "all set.");
        assert_eq!(new.detail.as_deref(), Some("took 3m"));
        assert_eq!(new.duration_ms, Some(180_000));
        let c = new.classify.as_ref().expect("classify");
        assert!(c.question_line.is_none());

        // No matching turn_start: the card has no "took" text.
        let body = adapter("turn_end", "fake-session-4", Some(TURN_END_3M), Some("Fake work is finished."));
        let new = kiro_to_new_event("host-a", 14, body, &mut turns, NOW_MS).unwrap();
        assert_eq!(new.state, Some(AgentState::AllSet));
        assert_eq!(new.detail, None);
        assert_eq!(new.started_at, None);
    }

    #[test]
    fn kiro_session_end_clears_turn_and_is_not_a_card() {
        let mut turns = HashMap::from([("host-a/fake-session-5".to_string(), START_MS)]);
        let body = adapter("session_end", "fake-session-5", None, None);
        let new = kiro_to_new_event("host-a", 15, body, &mut turns, NOW_MS).unwrap();
        assert!(new.session_end, "the daemon clears the pending state and stores nothing");
        assert_eq!(new.state, None);
        assert_eq!(new.session.as_deref(), Some("fake-session-5"));
        assert!(turns.is_empty());
    }

    #[test]
    fn kiro_malformed_body_is_invalid_event() {
        let mut turns = HashMap::new();
        let bad = [
            json!("not an object"),
            json!({ "event": "turn_start", "timestamp": TURN_START }),
            json!({ "event": "turn_start", "session_id": "  ", "timestamp": TURN_START }),
            json!({ "event": "turn_start", "session_id": "fake-session-6" }),
            json!({ "event": "turn_end", "session_id": "fake-session-6", "timestamp": "not-a-time" }),
            json!({ "event": 7, "session_id": "fake-session-6" }),
            json!({ "event": "turn_end", "session_id": "fake-session-6", "timestamp": TURN_END_3M, "reply_tail": 5 }),
        ];
        for body in bad {
            assert_eq!(kiro_to_new_event("host-a", 16, body.clone(), &mut turns, NOW_MS).unwrap_err(), "invalid_event", "{body}");
        }
        assert!(turns.is_empty(), "a rejected turn_start leaves no open turn");
    }

    #[test]
    fn kiro_unknown_event_name_is_invalid_event() {
        let mut turns = HashMap::new();
        let body = adapter("turn_pause", "fake-session-7", Some(TURN_START), None);
        assert_eq!(kiro_to_new_event("host-a", 17, body, &mut turns, NOW_MS).unwrap_err(), "invalid_event");
        let body = adapter("Turn_End", "fake-session-7", Some(TURN_END_3M), Some("x"));
        assert_eq!(kiro_to_new_event("host-a", 18, body, &mut turns, NOW_MS).unwrap_err(), "invalid_event");
    }

    #[test]
    fn seen_set_is_bounded() {
        let mut s = Seen::default();
        for id in 0..(SEEN_CAP as u64 + 10) {
            s.insert(id);
        }
        assert_eq!(s.ids.len(), SEEN_CAP);
        assert!(!s.contains(0));
        assert!(s.contains(SEEN_CAP as u64 + 9));
    }
}
