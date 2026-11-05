//! Relay puller tests against a fake relay on 127.0.0.1. No real network, keychain or
//! config directory is touched: the keychain is a fake `security` script and the cursor
//! lives in a temp dir.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::extract::{Query, State as AxState};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use http_body_util::BodyExt;
use nearpushd::config::RelayConfig;
use nearpushd::event::NewEvent;
use nearpushd::relay::{self, RelayHealth};
use nearpushd::server::{self, AppState};
use nearpushd::store::Store;
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tower::ServiceExt;

/// Bearer token of the local daemon (`/push`, `/status`).
const LOCAL_TOKEN: &str = "local-test-token-0123456789";
/// Consumer token the fake relay expects. Test value only.
const CONSUMER_TOKEN: &str = "consumer-test-token-0123456789";

#[derive(Clone)]
enum Reply {
    Page(Vec<Value>),
    Status(u16),
    Hold(Duration),
}

#[derive(Clone)]
struct Get {
    at: Instant,
    since: u64,
}

#[derive(Clone)]
struct Ack {
    through: u64,
    /// Contents of the cursor file at the moment the ack arrived.
    cursor_file: Option<String>,
}

#[derive(Default)]
struct Script {
    replies: VecDeque<Reply>,
    gets: Vec<Get>,
    acks: Vec<Ack>,
}

#[derive(Clone)]
struct Fake {
    script: Arc<Mutex<Script>>,
    cursor_path: PathBuf,
}

fn bearer_ok(headers: &HeaderMap) -> bool {
    headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {CONSUMER_TOKEN}"))
}

async fn get_events(
    AxState(f): AxState<Fake>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    let since: u64 = q.get("since").and_then(|s| s.parse().ok()).unwrap_or(0);
    let reply = {
        let mut s = f.script.lock().unwrap();
        s.gets.push(Get { at: Instant::now(), since });
        if !bearer_ok(&headers) {
            return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"})));
        }
        s.replies.pop_front()
    };
    match reply {
        Some(Reply::Status(code)) => (StatusCode::from_u16(code).unwrap(), Json(json!({"error": "scripted"}))),
        Some(Reply::Page(events)) => {
            let last = events.iter().filter_map(|e| e["id"].as_u64()).max().unwrap_or(since);
            (StatusCode::OK, Json(json!({"events": events, "last_id": last})))
        }
        Some(Reply::Hold(d)) => {
            tokio::time::sleep(d).await;
            (StatusCode::OK, Json(json!({"events": [], "last_id": since})))
        }
        None => {
            tokio::time::sleep(Duration::from_millis(100)).await;
            (StatusCode::OK, Json(json!({"events": [], "last_id": since})))
        }
    }
}

async fn ack(AxState(f): AxState<Fake>, headers: HeaderMap, Json(body): Json<Value>) -> StatusCode {
    let through = body["through"].as_u64().unwrap_or(0);
    let cursor_file = fs::read_to_string(&f.cursor_path).ok();
    let ok = bearer_ok(&headers);
    f.script.lock().unwrap().acks.push(Ack { through, cursor_file });
    if ok { StatusCode::NO_CONTENT } else { StatusCode::UNAUTHORIZED }
}

struct Relay {
    url: String,
    script: Arc<Mutex<Script>>,
}

impl Relay {
    async fn start(cursor_path: PathBuf) -> Relay {
        let script = Arc::new(Mutex::new(Script::default()));
        let fake = Fake { script: script.clone(), cursor_path };
        let app = Router::new()
            .route("/v1/events", get(get_events))
            .route("/v1/ack", post(ack))
            .with_state(fake);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Relay { url: format!("http://{addr}"), script }
    }

    fn reply(&self, r: Reply) {
        self.script.lock().unwrap().replies.push_back(r);
    }

    fn gets(&self) -> Vec<Get> {
        self.script.lock().unwrap().gets.clone()
    }

    fn acks(&self) -> Vec<Ack> {
        self.script.lock().unwrap().acks.clone()
    }

}

/// One fixture per test: a state dir (events log and cursor) and a fake relay.
struct Fixture {
    _dir: tempfile::TempDir,
    state_dir: PathBuf,
    cursor: PathBuf,
    relay: Relay,
}

impl Fixture {
    async fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        let cursor = state_dir.join("relay-cursor.json");
        let relay = Relay::start(cursor.clone()).await;
        Fixture { _dir: dir, state_dir, cursor, relay }
    }

    /// A daemon state over the fixture's state dir. Reopening it reads the same log.
    fn app_state(&self) -> AppState {
        let store = Arc::new(Store::open(&self.state_dir, 500, 64).unwrap());
        AppState::new(store, LOCAL_TOKEN.into(), vec![], false)
    }

    fn start(&self, state: &AppState) -> JoinHandle<()> {
        let cfg = RelayConfig {
            enabled: true,
            url: Some(self.relay.url.clone()),
            wait_secs: 1,
            ..RelayConfig::default()
        };
        tokio::spawn(relay::run(state.clone(), cfg, CONSUMER_TOKEN.into(), self.cursor.clone()))
    }
}

fn titled(id: u64) -> Value {
    json!({"id": id, "source": "kiro", "received_ms": 1, "body": {"title": format!("t{id}")}})
}

fn with_body(id: u64, body: Value) -> Value {
    json!({"id": id, "source": "kiro", "received_ms": 1, "body": body})
}

/// Relay ids of the stored events, in store order.
fn relay_ids(state: &AppState) -> Vec<u64> {
    state.store.since(0).iter().filter_map(|e| e.relay_id).collect()
}

async fn wait_for(mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn loopback() -> SocketAddr {
    "127.0.0.1:5000".parse().unwrap()
}

#[tokio::test]
async fn events_processed_in_id_order() {
    let fx = Fixture::new().await;
    let state = fx.app_state();
    fx.relay.reply(Reply::Page(vec![titled(3), titled(1), titled(2)]));
    let task = fx.start(&state);
    wait_for(|| relay_ids(&state).len() == 3, "three events").await;
    assert_eq!(relay_ids(&state), vec![1, 2, 3]);
    let stored = state.store.since(0);
    assert_eq!(stored[0].title, "t1");
    assert_eq!(stored[0].app.as_deref(), Some("kiro"), "source becomes the app");
    task.abort();
}

#[tokio::test]
async fn cursor_persisted_only_after_processing() {
    let fx = Fixture::new().await;
    let state = fx.app_state();
    fx.relay.reply(Reply::Page(vec![titled(1), titled(2)]));
    let task = fx.start(&state);
    wait_for(|| !fx.relay.acks().is_empty(), "first ack").await;
    let ack = fx.relay.acks()[0].clone();
    assert_eq!(ack.through, 2);
    assert_eq!(relay_ids(&state), vec![1, 2], "both events were processed before the ack");
    let on_disk = ack.cursor_file.expect("cursor file existed when the ack arrived");
    assert_eq!(serde_json::from_str::<Value>(&on_disk).unwrap()["last_id"], 2);
    assert_eq!(fs::metadata(&fx.cursor).unwrap().permissions().mode() & 0o777, 0o600);
    task.abort();
}

#[tokio::test]
async fn restart_resumes_from_cursor() {
    let fx = Fixture::new().await;
    let state = fx.app_state();
    fx.relay.reply(Reply::Page((1..=5).map(titled).collect()));
    let task = fx.start(&state);
    wait_for(|| fx.relay.acks().iter().any(|a| a.through == 5), "ack through 5").await;
    task.abort();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let before = fx.relay.gets().len();
    let restarted = fx.app_state();
    let task = fx.start(&restarted);
    wait_for(|| fx.relay.gets().len() > before, "GET after restart").await;
    let first = fx.relay.gets()[before].since;
    assert_eq!(first, 5, "restart resumes from the persisted cursor, not 0");
    assert_eq!(relay_ids(&restarted), vec![1, 2, 3, 4, 5], "restart does not reprocess");
    task.abort();
}

#[tokio::test]
async fn duplicate_ids_ignored() {
    let fx = Fixture::new().await;
    let state = fx.app_state();
    fx.relay.reply(Reply::Page(vec![titled(4), titled(4)]));
    fx.relay.reply(Reply::Page(vec![titled(4)]));
    let task = fx.start(&state);
    wait_for(|| fx.relay.gets().len() >= 3, "three polls").await;
    assert_eq!(relay_ids(&state), vec![4]);
    task.abort();
}

#[tokio::test]
async fn crash_window_skipped() {
    let fx = Fixture::new().await;
    let state = fx.app_state();
    // Crash after processing relay event 7 but before the cursor moved past 5.
    state.store.push(NewEvent { title: "old".into(), relay_id: Some(7), ..Default::default() }).unwrap();
    relay::save_cursor(&fx.cursor, 5).unwrap();
    fx.relay.reply(Reply::Page(vec![titled(6), titled(7), titled(8)]));
    let task = fx.start(&state);
    wait_for(|| fx.relay.acks().iter().any(|a| a.through == 8), "ack through 8").await;
    let mut ids = relay_ids(&state);
    ids.sort();
    assert_eq!(ids, vec![6, 7, 8], "7 was already shown and is not shown again");
    task.abort();
}

#[tokio::test]
async fn all_seen_batch_still_advances_cursor() {
    // Crash after storing 6 and 7 but before the cursor moved past 5. The relay
    // redelivers only those two ids, so the batch has nothing new to show.
    let fx = Fixture::new().await;
    let state = fx.app_state();
    state.store.push(NewEvent { title: "six".into(), relay_id: Some(6), ..Default::default() }).unwrap();
    state.store.push(NewEvent { title: "seven".into(), relay_id: Some(7), ..Default::default() }).unwrap();
    relay::save_cursor(&fx.cursor, 5).unwrap();
    fx.relay.reply(Reply::Page(vec![titled(6), titled(7)]));
    let task = fx.start(&state);
    wait_for(|| fx.relay.acks().iter().any(|a| a.through == 7), "ack through 7").await;
    assert_eq!(relay::load_cursor(&fx.cursor), 7, "cursor moves past the seen batch");
    assert_eq!(relay_ids(&state), vec![6, 7], "nothing shown twice");
    task.abort();
}

#[tokio::test]
async fn unauthorized_disables_worker() {
    let fx = Fixture::new().await;
    let state = fx.app_state();
    fx.relay.reply(Reply::Status(401));
    let task = fx.start(&state);
    tokio::time::timeout(Duration::from_secs(5), task).await.expect("worker returns").unwrap();
    assert_eq!(fx.relay.gets().len(), 1, "no retry after 401");
    assert!(fx.relay.acks().is_empty());
    let view = state.relay.view();
    assert!(!view.enabled && !view.connected);
    assert_eq!(view.last_error_class, Some("unauthorized"));
}

#[tokio::test]
async fn backoff_on_500() {
    let fx = Fixture::new().await;
    let state = fx.app_state();
    fx.relay.reply(Reply::Status(500));
    fx.relay.reply(Reply::Status(500));
    fx.relay.reply(Reply::Page(vec![titled(1)]));
    let task = fx.start(&state);
    wait_for(|| fx.relay.gets().len() >= 3, "retries").await;
    let gets = fx.relay.gets();
    let gap1 = gets[1].at - gets[0].at;
    let gap2 = gets[2].at - gets[1].at;
    assert!(gap1 >= Duration::from_secs(1), "first retry waits at least 1s: {gap1:?}");
    assert!(gap2 >= Duration::from_secs(2), "second retry waits at least 2s: {gap2:?}");
    wait_for(|| !fx.relay.acks().is_empty(), "ack after recovery").await;
    let view = state.relay.view();
    assert!(view.connected && view.last_error_class.is_none());
    task.abort();
}

#[tokio::test]
async fn push_not_blocked_by_long_poll() {
    let fx = Fixture::new().await;
    let state = fx.app_state();
    fx.relay.reply(Reply::Hold(Duration::from_secs(5)));
    let task = fx.start(&state);
    wait_for(|| !fx.relay.gets().is_empty(), "long poll in flight").await;

    let app = server::router(state.clone()).layer(MockConnectInfo(loopback()));
    let req = Request::builder()
        .method("POST")
        .uri("/push")
        .header("authorization", format!("Bearer {LOCAL_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"title":"local"}"#))
        .unwrap();
    let started = Instant::now();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(started.elapsed() < Duration::from_secs(1), "/push waited on the puller: {:?}", started.elapsed());
    task.abort();
}

#[tokio::test]
async fn bad_event_skipped_and_cursor_advances() {
    let fx = Fixture::new().await;
    let state = fx.app_state();
    let too_long = "x".repeat(201);
    fx.relay.reply(Reply::Page(vec![
        with_body(1, json!("not an object")),
        with_body(2, json!({"title": too_long})),
        titled(3),
    ]));
    let task = fx.start(&state);
    wait_for(|| fx.relay.acks().iter().any(|a| a.through == 3), "ack through 3").await;
    assert_eq!(relay_ids(&state), vec![3], "only the valid event is shown");
    assert_eq!(relay::load_cursor(&fx.cursor), 3, "the bad ids are passed, not retried");
    task.abort();
}

#[tokio::test]
async fn keychain_failure_disables_relay_without_crash() {
    let fx = Fixture::new().await;
    let state = fx.app_state();
    let fake_security = fx.state_dir.parent().unwrap().join("security");
    fs::write(&fake_security, "#!/bin/sh\necho 'not found' >&2\nexit 44\n").unwrap();
    fs::set_permissions(&fake_security, fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = RelayConfig {
        enabled: true,
        url: Some(fx.relay.url.clone()),
        security_program: fake_security.display().to_string(),
        ..RelayConfig::default()
    };
    let task = relay::spawn_from_config(state.clone(), cfg, fx.cursor.clone()).expect("relay enabled");
    tokio::time::timeout(Duration::from_secs(10), task).await.expect("lookup finishes").unwrap();
    assert!(fx.relay.gets().is_empty(), "no relay call without a token");

    // The daemon still answers, and /status says the relay is off.
    let app = server::router(state.clone()).layer(MockConnectInfo(loopback()));
    let req = Request::builder()
        .uri("/status")
        .header("authorization", format!("Bearer {LOCAL_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["relay"]["enabled"], false);
    assert_eq!(body["relay"]["connected"], false);
    assert_eq!(body["relay"]["last_id"], 0);
    assert!(body["relay"]["last_error_class"].is_null());
}

#[tokio::test]
async fn cursor_write_failure_backs_off_exponentially() {
    let fx = Fixture::new().await;
    let state = fx.app_state();
    // A regular file where the cursor directory should be: every cursor save fails.
    let blocker = fx.state_dir.join("blocker");
    fs::write(&blocker, "not a directory").unwrap();
    let cursor = blocker.join("relay-cursor.json");
    for _ in 0..3 {
        fx.relay.reply(Reply::Page(vec![titled(1)]));
    }
    let cfg = RelayConfig {
        enabled: true,
        url: Some(fx.relay.url.clone()),
        wait_secs: 1,
        ..RelayConfig::default()
    };
    let task = tokio::spawn(relay::run(state.clone(), cfg, CONSUMER_TOKEN.into(), cursor));
    wait_for(|| fx.relay.gets().len() >= 3, "retries after cursor failures").await;
    let gets = fx.relay.gets();
    let gap1 = gets[1].at - gets[0].at;
    let gap2 = gets[2].at - gets[1].at;
    assert!(gap1 >= Duration::from_secs(1), "first retry waits at least 1s: {gap1:?}");
    assert!(gap2 >= Duration::from_secs(2), "backoff grows on repeated cursor failures: {gap2:?}");
    assert!(fx.relay.acks().is_empty(), "nothing is acked while the cursor cannot be saved");
    assert_eq!(relay_ids(&state), vec![1], "the event is shown once, not on every retry");
    task.abort();
}

#[tokio::test]
async fn relay_health_starts_disabled() {
    let health = RelayHealth::default();
    let v = health.view();
    assert!(!v.enabled && !v.connected);
    assert_eq!(v.last_id, 0);
    assert!(v.last_error_class.is_none());
}
