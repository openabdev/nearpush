//! End-to-end tests against a real listener: push auth, persistence and replay.

use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use nearpushd::assets;
use nearpushd::classifier::{ClassifierConfig, Provider};
use nearpushd::config::SummarizerConfig;
use nearpushd::control::Control;
use nearpushd::event::AgentState;
use nearpushd::server::{self, AppState};
use nearpushd::store::Store;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest, http::HeaderValue};
use tower::ServiceExt;

const TOKEN: &str = "test-token-0123456789";
const LOGIN: &str = "you@example.com";

struct Daemon {
    addr: SocketAddr,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Daemon {
    async fn start(state_dir: &Path, allow_loopback: bool) -> Daemon {
        Daemon::start_with(state(state_dir, allow_loopback)).await
    }

    async fn start_with(state: AppState) -> Daemon {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(server::serve(listener, state));
        Daemon { addr, task }
    }

    async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn ws_url(&self, since: u64) -> String {
        format!("ws://{}/ws?since={since}", self.addr)
    }
}

fn state(state_dir: &Path, allow_loopback: bool) -> AppState {
    let store = Arc::new(Store::open(state_dir, 500, 16).unwrap());
    AppState::new(store, TOKEN.into(), vec![LOGIN.into()], allow_loopback)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

async fn push(d: &Daemon, body: Value) -> reqwest::Response {
    client().post(d.url("/push")).bearer_auth(TOKEN).json(&body).send().await.unwrap()
}

async fn push_ok(d: &Daemon, title: &str) -> u64 {
    let r = push(d, json!({ "title": title })).await;
    assert_eq!(r.status(), 200);
    r.json::<Value>().await.unwrap()["id"].as_u64().unwrap()
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn open_ws(url: &str, headers: &[(&'static str, &str)]) -> Result<Ws, StatusCode> {
    let mut req = url.into_client_request().unwrap();
    for (k, v) in headers {
        req.headers_mut().insert(*k, HeaderValue::from_str(v).unwrap());
    }
    match tokio_tungstenite::connect_async(req).await {
        Ok((ws, _)) => Ok(ws),
        Err(tungstenite::Error::Http(resp)) => Err(StatusCode::from_u16(resp.status().as_u16()).unwrap()),
        Err(e) => panic!("websocket connect failed: {e}"),
    }
}

/// Next JSON text frame, skipping protocol pings.
async fn next_frame(ws: &mut Ws) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("frame within 5s")
            .expect("socket open")
            .expect("frame ok");
        if let tungstenite::Message::Text(t) = msg {
            return serde_json::from_str(t.as_str()).unwrap();
        }
    }
}

async fn event_ids_until_synced(ws: &mut Ws) -> Vec<u64> {
    let mut ids = Vec::new();
    loop {
        let f = next_frame(ws).await;
        match f["type"].as_str().unwrap() {
            "event" => ids.push(f["event"]["id"].as_u64().unwrap()),
            "main_card" => {}
            "synced" => return ids,
            other => panic!("unexpected frame {other}"),
        }
    }
}

#[tokio::test]
async fn push_requires_bearer_token() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), false).await;
    let body = json!({ "title": "t" });

    let r = client().post(d.url("/push")).json(&body).send().await.unwrap();
    assert_eq!(r.status(), 401);
    let r = client().post(d.url("/push")).bearer_auth("wrong").json(&body).send().await.unwrap();
    assert_eq!(r.status(), 401);
    assert!(r.json::<Value>().await.unwrap()["error"].is_string());

    assert_eq!(push_ok(&d, "t").await, 1);
    d.stop().await;
}

#[tokio::test]
async fn push_rejects_requests_forwarded_by_tailscale() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), false).await;
    for (name, value) in [
        ("Tailscale-User-Login", LOGIN),
        ("Tailscale-User-Name", "Example"),
        ("Tailscale-Headers-Info", "x"),
        ("X-Forwarded-For", "192.0.2.2"),
    ] {
        let r = client()
            .post(d.url("/push"))
            .bearer_auth(TOKEN)
            .header(name, value)
            .json(&json!({ "title": "t" }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "{name}");
    }
    // Nothing was stored: the first accepted event is still id 1.
    assert_eq!(push_ok(&d, "t").await, 1);
    d.stop().await;
}

#[tokio::test]
async fn non_loopback_peers_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let app = server::router(state(dir.path(), true))
        .layer(MockConnectInfo(SocketAddr::from(([192, 0, 2, 2], 40000))));

    let req = Request::post("/push")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"title":"t"}"#))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let req = Request::get("/").header("tailscale-user-login", LOGIN).body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn push_validates_payload() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), false).await;
    for bad in [
        json!({}),
        json!({ "title": "" }),
        json!({ "title": "t", "level": "fatal" }),
        json!({ "title": "t", "state": "napping" }),
        json!({ "title": "t", "url": "javascript:alert(1)" }),
        json!({ "title": "x".repeat(201) }),
        json!({ "title": "t", "extra": 1 }),
    ] {
        assert_eq!(push(&d, bad.clone()).await.status(), 400, "{bad}");
    }
    let huge = json!({ "title": "t", "body": "x".repeat(20_000) });
    assert_eq!(push(&d, huge).await.status(), 413);

    let full = json!({
        "title": "F38 多工位会话拼单", "state": "your_turn", "subtitle": "part of the source",
        "project": "frontend / 7008", "started_at": 1_700_000_000_000u64, "level": "warn",
        "url": "https://example.com", "tags": ["a"]
    });
    assert_eq!(push(&d, full).await.status(), 200);
    d.stop().await;
}

#[tokio::test]
async fn ids_and_history_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    for i in 1..=3 {
        assert_eq!(push_ok(&d, &format!("e{i}")).await, i);
    }
    d.stop().await;

    let lines = std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
    assert_eq!(lines.lines().count(), 3);

    let d = Daemon::start(dir.path(), true).await;
    assert_eq!(push_ok(&d, "after restart").await, 4);
    let mut ws = open_ws(&d.ws_url(0), &[]).await.unwrap();
    let hello = next_frame(&mut ws).await;
    assert_eq!(hello["last_id"], 4);
    assert_eq!(event_ids_until_synced(&mut ws).await, vec![1, 2, 3, 4]);
    d.stop().await;
}

#[tokio::test]
async fn ws_replays_since_then_streams_live() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), false).await;
    for i in 1..=5 {
        push_ok(&d, &format!("e{i}")).await;
    }

    let mut ws = open_ws(&d.ws_url(2), &[("tailscale-user-login", LOGIN)]).await.unwrap();
    let hello = next_frame(&mut ws).await;
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["version"], assets::ui_version());
    assert_eq!(hello["last_id"], 5);
    assert_eq!(event_ids_until_synced(&mut ws).await, vec![3, 4, 5]);

    let r = push(&d, json!({ "title": "live", "state": "all_set", "duration_ms": 60000 })).await;
    assert_eq!(r.status(), 200);
    let f = next_frame(&mut ws).await;
    assert_eq!(f["type"], "event");
    assert_eq!(f["event"]["id"], 6);
    assert_eq!(f["event"]["state"], "all_set");
    assert_eq!(f["event"]["duration_ms"], 60000);
    d.stop().await;
}

#[tokio::test]
async fn ws_and_page_require_allowed_login() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), false).await;

    assert_eq!(open_ws(&d.ws_url(0), &[]).await.unwrap_err(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        open_ws(&d.ws_url(0), &[("tailscale-user-login", "mallory@example.com")]).await.unwrap_err(),
        StatusCode::FORBIDDEN
    );
    let host = d.addr.to_string();
    assert_eq!(
        open_ws(&d.ws_url(0), &[("tailscale-user-login", LOGIN), ("origin", "https://evil.example")])
            .await
            .unwrap_err(),
        StatusCode::FORBIDDEN
    );
    let origin = format!("https://{host}");
    assert!(open_ws(&d.ws_url(0), &[("tailscale-user-login", "YOU@example.com"), ("origin", &origin)]).await.is_ok());

    assert_eq!(client().get(d.url("/")).send().await.unwrap().status(), 401);
    let r = client().get(d.url("/")).header("tailscale-user-login", LOGIN).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["cache-control"], "no-store");
    let html = r.text().await.unwrap();
    assert!(html.contains(assets::ui_version()));
    assert!(!html.contains(assets::VERSION_PLACEHOLDER));
    assert!(html.contains("apple-mobile-web-app-capable"));

    for path in ["/manifest.webmanifest", "/icon-180.png", "/icon-192.png", "/icon-512.png"] {
        assert_eq!(client().get(d.url(path)).send().await.unwrap().status(), 401, "{path}");
        let r = client().get(d.url(path)).header("tailscale-user-login", LOGIN).send().await.unwrap();
        assert_eq!(r.status(), 200, "{path}");
    }
    d.stop().await;
}

#[tokio::test]
async fn loopback_flag_allows_headerless_viewer() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    assert_eq!(client().get(d.url("/")).send().await.unwrap().status(), 200);
    assert!(open_ws(&d.ws_url(0), &[]).await.is_ok());
    // The flag never relaxes the allowlist for a login that is present.
    assert_eq!(
        open_ws(&d.ws_url(0), &[("tailscale-user-login", "mallory@example.com")]).await.unwrap_err(),
        StatusCode::FORBIDDEN
    );
    d.stop().await;
}

#[tokio::test]
async fn manifest_is_valid_json() {
    let v: Value = serde_json::from_str(assets::MANIFEST).unwrap();
    assert_eq!(v["display"], "standalone");
    let body = server::router(state(tempfile::tempdir().unwrap().path(), true))
        .layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1))))
        .oneshot(Request::get("/manifest.webmanifest").body(Body::empty()).unwrap())
        .await
        .unwrap()
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), v);
}

#[tokio::test]
async fn ws_signals_events_missed_past_the_ring() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), 3, 16).unwrap());
    let d = Daemon::start_with(AppState::new(store, TOKEN.into(), vec![LOGIN.into()], true)).await;
    for i in 1..=10 {
        push_ok(&d, &format!("e{i}")).await;
    }
    let mut ws = open_ws(&d.ws_url(0), &[]).await.unwrap();
    let hello = next_frame(&mut ws).await;
    assert_eq!((hello["oldest_id"].as_u64(), hello["missed"].as_u64()), (Some(8), Some(7)));
    assert_eq!(event_ids_until_synced(&mut ws).await, vec![8, 9, 10]);

    let mut ws = open_ws(&d.ws_url(8), &[]).await.unwrap();
    assert_eq!(next_frame(&mut ws).await["missed"], 0);
    d.stop().await;
}

#[tokio::test]
async fn sender_and_context_fields_are_stored() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    let r = push(&d, json!({
        "title": "all set.", "state": "all_set", "sender": "kiro", "app": "Kiro", "session": "s-1234",
        "session_label": "修正登入", "host": "laptop", "detail": "took 3m", "project": "nearpush / main"
    })).await;
    assert_eq!(r.status(), 200);
    assert_eq!(push(&d, json!({ "title": "t", "sender": "../etc" })).await.status(), 400);
    let line = std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
    let v: Value = serde_json::from_str(line.lines().next().unwrap()).unwrap();
    assert_eq!(v["sender"], "kiro");
    assert_eq!(v["session"], "s-1234");
    assert_eq!(v["detail"], "took 3m");
    d.stop().await;
}

#[tokio::test]
async fn sender_icons_are_served() {
    let dir = tempfile::tempdir().unwrap();
    let icons = dir.path().join("icons");
    std::fs::create_dir(&icons).unwrap();
    std::fs::write(icons.join("mine.png"), b"\x89PNG").unwrap();
    let st = state(dir.path(), false).with_icons_dir(icons);
    let d = Daemon::start_with(st).await;
    let get = |path: &str| client().get(d.url(path)).header("tailscale-user-login", LOGIN).send();
    assert_eq!(client().get(d.url("/icons/kiro")).send().await.unwrap().status(), 401);
    let r = get("/icons/kiro").await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "image/svg+xml");
    assert!(r.text().await.unwrap().contains("#9046FF"));
    let r = get("/icons/mine").await.unwrap();
    assert_eq!((r.status().as_u16(), r.headers()["content-type"].to_str().unwrap()), (200, "image/png"));
    let r = get("/icons/someone").await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains(">S</text>"));
    for bad in ["/icons/..%2Fsecret", "/icons/KIRO", "/icons/a.b"] {
        assert_eq!(get(bad).await.unwrap().status(), 404, "{bad}");
    }
    d.stop().await;
}

fn summarizer_state(dir: &Path, script: &str, timeout_secs: u64) -> AppState {
    let cfg = SummarizerConfig {
        enabled: true,
        command: vec!["/bin/sh".into(), "-c".into(), script.into()],
        timeout_secs,
        keychain_service: String::new(),
        ..SummarizerConfig::default()
    };
    state(dir, true)
        .with_sessions(nearpushd::sessions::Sessions::load(dir))
        .with_summarizer(cfg, dir.join("summarizer"))
        // These tests watch `working` cards, which the default filter hides.
        .with_control(vec![AgentState::Working, AgentState::AllSet], Provider::Kiro, Control::default(), dir.to_path_buf())
}

/// Next frame that is not a ping, or `None` if nothing arrives within `wait`.
/// Next frame that `keep` accepts, within `wait`.
async fn frame_within_where(ws: &mut Ws, wait: Duration, keep: impl Fn(&Value) -> bool) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let msg = tokio::time::timeout_at(deadline, ws.next()).await.ok()??.ok()?;
        if let tungstenite::Message::Text(t) = msg {
            let v: Value = serde_json::from_str(t.as_str()).unwrap();
            if v["type"] != "ping" && keep(&v) {
                return Some(v);
            }
        }
    }
}

/// Next frame other than `main_card`, within `wait`.
async fn frame_within(ws: &mut Ws, wait: Duration) -> Option<Value> {
    frame_within_where(ws, wait, |v| v["type"] != "main_card").await
}

/// Next `main_card` frame, within `wait`. Its `card` is `null` when the event stream shows the card.
async fn main_within(ws: &mut Ws, wait: Duration) -> Option<Value> {
    frame_within_where(ws, wait, |v| v["type"] == "main_card").await
}

/// Next text frame of any type, within `wait`.
async fn raw_frame_within(ws: &mut Ws, wait: Duration) -> Option<Value> {
    frame_within_where(ws, wait, |_| true).await
}

#[tokio::test]
async fn summarizer_label_is_broadcast_and_reused() {
    let dir = tempfile::tempdir().unwrap();
    let script = r#"input=$(cat); case "$input" in *"login redirect"*) echo "  Fix login redirect." ;; *) echo other ;; esac"#;
    let d = Daemon::start_with(summarizer_state(dir.path(), script, 10)).await;
    let mut ws = open_ws(&d.ws_url(0), &[]).await.unwrap();
    assert_eq!(next_frame(&mut ws).await["type"], "hello");
    assert_eq!(next_frame(&mut ws).await["type"], "synced");

    let r = push(&d, json!({ "title": "working.", "state": "working", "sender": "kiro", "session": "s1",
        "session_label": "please fix the login redi", "prompt": "please fix the login redirect loop" })).await;
    assert_eq!(r.status(), 200);
    let f = frame_within(&mut ws, Duration::from_secs(5)).await.unwrap();
    assert_eq!(f["event"]["session_label"], "please fix the login redi", "provisional label first");
    let f = frame_within(&mut ws, Duration::from_secs(10)).await.expect("session_update");
    assert_eq!(f, json!({ "type": "session_update", "session": "s1", "label": "Fix login redirect" }));

    // Later events of the session carry the summarized label.
    push(&d, json!({ "title": "all set.", "state": "all_set", "session": "s1", "session_label": "prov" })).await;
    let f = frame_within(&mut ws, Duration::from_secs(5)).await.unwrap();
    assert_eq!(f["event"]["session_label"], "Fix login redirect");
    // Prompts never reach the event log; labels survive a restart via the hello frame.
    let log = std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
    assert!(!log.contains("redirect loop"));
    d.stop().await;
    let d = Daemon::start_with(state(dir.path(), true).with_sessions(nearpushd::sessions::Sessions::load(dir.path()))).await;
    let mut ws = open_ws(&d.ws_url(0), &[]).await.unwrap();
    let hello = next_frame(&mut ws).await;
    assert_eq!(hello["labels"], json!([{ "session": "s1", "label": "Fix login redirect" }]));
    d.stop().await;
}

#[tokio::test]
async fn failing_or_slow_summarizer_keeps_provisional_label() {
    for script in ["cat >/dev/null; echo boom >&2; exit 1", "sleep 10", "cat >/dev/null"] {
        let dir = tempfile::tempdir().unwrap();
        let d = Daemon::start_with(summarizer_state(dir.path(), script, 1)).await;
        let mut ws = open_ws(&d.ws_url(0), &[]).await.unwrap();
        next_frame(&mut ws).await;
        next_frame(&mut ws).await;
        push(&d, json!({ "title": "working.", "state": "working", "session": "s1", "session_label": "prov",
            "prompt": "fix the login redirect" })).await;
        assert_eq!(frame_within(&mut ws, Duration::from_secs(5)).await.unwrap()["type"], "event");
        assert!(frame_within(&mut ws, Duration::from_millis(2500)).await.is_none(), "{script}: no session_update");
        push(&d, json!({ "title": "all set.", "state": "all_set", "session": "s1", "session_label": "prov" })).await;
        let f = frame_within(&mut ws, Duration::from_secs(5)).await.unwrap();
        assert_eq!(f["event"]["session_label"], "prov", "{script}");
        d.stop().await;
    }
}

#[tokio::test]
async fn prompts_are_not_kept_when_summarizer_is_off() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start_with(state(dir.path(), true).with_sessions(nearpushd::sessions::Sessions::load(dir.path()))).await;
    let r = push(&d, json!({ "title": "working.", "state": "working", "session": "s1", "session_label": "prov",
        "prompt": "secret prompt text" })).await;
    assert_eq!(r.status(), 200);
    d.stop().await;
    let sessions = std::fs::read_to_string(dir.path().join("sessions.json")).unwrap_or_default();
    assert!(!sessions.contains("secret prompt text"), "{sessions}");
    let log = std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
    assert!(!log.contains("secret prompt text"));
}

#[tokio::test]
async fn failing_summarizer_is_not_rerun_on_every_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let runs = dir.path().join("runs");
    let script = format!("cat >/dev/null; echo run >> '{}'; exit 1", runs.display());
    let d = Daemon::start_with(summarizer_state(dir.path(), &script, 5)).await;
    for i in 0..4 {
        push(&d, json!({ "title": "working.", "state": "working", "session": "s1", "session_label": "prov",
            "prompt": format!("fix the login redirect, try {i}") })).await;
        // Past the worker's debounce, so each prompt would be its own run.
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }
    d.stop().await;
    let n = std::fs::read_to_string(&runs).unwrap_or_default().lines().count();
    assert_eq!(n, 1, "one failed run, then backoff");
}

// ---- /status, /control, pause and the display filter -------------------------

async fn get_status(d: &Daemon) -> Value {
    let r = client().get(d.url("/status")).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

async fn post_control(d: &Daemon, body: Value) -> reqwest::Response {
    client().post(d.url("/control")).bearer_auth(TOKEN).json(&body).send().await.unwrap()
}

/// Opens a socket and reads up to `synced`; returns the socket and the synced cursor.
async fn ws_synced(d: &Daemon, since: u64) -> (Ws, Vec<u64>, u64) {
    let mut ws = open_ws(&d.ws_url(since), &[]).await.unwrap();
    assert_eq!(next_frame(&mut ws).await["type"], "hello");
    let mut ids = Vec::new();
    loop {
        let f = next_frame(&mut ws).await;
        match f["type"].as_str().unwrap() {
            "event" => ids.push(f["event"]["id"].as_u64().unwrap()),
            "main_card" => {}
            "synced" => return (ws, ids, f["last_id"].as_u64().unwrap()),
            other => panic!("unexpected frame {other}"),
        }
    }
}

const PROXY_HEADERS: [(&str, &str); 3] =
    [("Tailscale-User-Login", LOGIN), ("Tailscale-User-Name", "Example"), ("X-Forwarded-For", "192.0.2.2")];

#[tokio::test]
async fn status_requires_token_and_loopback() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    assert_eq!(client().get(d.url("/status")).send().await.unwrap().status(), 401);
    assert_eq!(client().get(d.url("/status")).bearer_auth("wrong").send().await.unwrap().status(), 401);
    for (name, value) in PROXY_HEADERS {
        let r = client().get(d.url("/status")).bearer_auth(TOKEN).header(name, value).send().await.unwrap();
        assert_eq!(r.status(), 403, "{name}");
    }
    let app = server::router(state(dir.path(), true)).layer(MockConnectInfo(SocketAddr::from(([192, 0, 2, 2], 40000))));
    let req = Request::get("/status").header("authorization", format!("Bearer {TOKEN}")).body(Body::empty()).unwrap();
    assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::FORBIDDEN);

    let s = get_status(&d).await;
    assert_eq!((s["last_event_id"].as_u64(), s["viewers"].as_u64()), (Some(0), Some(0)));
    assert_eq!(s["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(s["pid"].as_u64(), Some(std::process::id() as u64));
    assert!(s["exe"].is_string());
    assert_eq!(s["paused"], false);
    assert_eq!(s["filter"], json!({ "states": ["all_set", "your_turn", "allow"], "source": "config" }));
    assert_eq!(s["summarizer"]["enabled"], false);
    assert_eq!(s["summarizer"]["healthy"], false);
    assert!(s["public_url"].is_null());

    push_ok(&d, "first").await;
    push_ok(&d, "second").await;
    let (ws, _, _) = ws_synced(&d, 0).await;
    let s = get_status(&d).await;
    assert_eq!(s["viewers"], 1);
    assert_eq!(s["last_event_id"], 2);
    assert_eq!(s["recent"][0]["title"], "second");
    assert_eq!(s["recent"][0]["shown"], true);
    assert_eq!(s["recent"].as_array().unwrap().len(), 2);
    drop(ws);
    let mut viewers = 1;
    for _ in 0..50 {
        viewers = get_status(&d).await["viewers"].as_u64().unwrap();
        if viewers == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(viewers, 0, "viewer count drops when the socket closes");
    d.stop().await;
}

#[tokio::test]
async fn status_reports_public_url_and_summarizer() {
    let dir = tempfile::tempdir().unwrap();
    let st = summarizer_state(dir.path(), "cat >/dev/null; echo label", 5).with_public_url(Some("https://laptop.example.ts.net/".into()));
    let d = Daemon::start_with(st).await;
    let s = get_status(&d).await;
    assert_eq!(s["public_url"], "https://laptop.example.ts.net/");
    assert_eq!(s["summarizer"]["enabled"], true);
    assert_eq!(s["summarizer"]["healthy"], true, "never failed yet");
    d.stop().await;
}

#[tokio::test]
async fn control_requires_token_and_rejects_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    let body = json!({ "paused": true });
    assert_eq!(client().post(d.url("/control")).json(&body).send().await.unwrap().status(), 401);
    assert_eq!(client().post(d.url("/control")).bearer_auth("wrong").json(&body).send().await.unwrap().status(), 401);
    for (name, value) in PROXY_HEADERS {
        let r = client().post(d.url("/control")).bearer_auth(TOKEN).header(name, value).json(&body).send().await.unwrap();
        assert_eq!(r.status(), 403, "{name}");
    }
    let app = server::router(state(dir.path(), true)).layer(MockConnectInfo(SocketAddr::from(([192, 0, 2, 2], 40000))));
    let req = Request::post("/control")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"paused":true}"#))
        .unwrap();
    assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(post_control(&d, json!({})).await.status(), 400);
    assert_eq!(post_control(&d, json!({ "pausd": true })).await.status(), 400);
    assert_eq!(get_status(&d).await["paused"], false, "rejected requests change nothing");

    let r = post_control(&d, body).await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["paused"], true);
    assert_eq!(v["filter"]["source"], "config");
    assert_eq!(get_status(&d).await["paused"], true);
    d.stop().await;
}

#[tokio::test]
async fn control_states_validated() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    assert_eq!(post_control(&d, json!({ "states": ["sleeping"] })).await.status(), 400);
    assert_eq!(post_control(&d, json!({ "states": "working" })).await.status(), 400);
    assert_eq!(post_control(&d, json!({ "paused": "yes" })).await.status(), 400);
    let v: Value = post_control(&d, json!({ "states": ["working", "working", "allow"] })).await.json().await.unwrap();
    assert_eq!(v["filter"], json!({ "states": ["working", "allow"], "source": "control" }), "duplicates dropped");
    d.stop().await;
}

#[tokio::test]
async fn paused_events_are_stored_not_broadcast() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    assert_eq!(post_control(&d, json!({ "paused": true })).await.status(), 200);
    let muted_id = push_ok(&d, "quiet please").await;
    assert!(frame_within(&mut ws, Duration::from_millis(1500)).await.is_none(), "paused: nothing broadcast");
    let log = std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
    let line = log.lines().find(|l| l.contains("quiet please")).expect("stored");
    assert!(line.contains("\"muted\":true"), "{line}");
    let s = get_status(&d).await;
    assert_eq!((s["last_event_id"].as_u64(), s["recent"][0]["shown"].as_bool()), (Some(muted_id), Some(false)));

    assert_eq!(post_control(&d, json!({ "paused": false })).await.status(), 200);
    let id = push_ok(&d, "back on").await;
    let f = frame_within(&mut ws, Duration::from_secs(5)).await.unwrap();
    assert_eq!((f["type"].as_str(), f["event"]["id"].as_u64()), (Some("event"), Some(id)));
    // A new page replaying from 0 never sees the muted event, even after unpausing.
    let (_ws2, ids, cursor) = ws_synced(&d, 0).await;
    assert_eq!(ids, vec![id]);
    assert_eq!(cursor, id);
    d.stop().await;
}

#[tokio::test]
async fn default_filter_hides_working() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    let r = push(&d, json!({ "title": "working.", "state": "working", "session": "s1" })).await;
    assert_eq!(r.status(), 200);
    let working_id = r.json::<Value>().await.unwrap()["id"].as_u64().unwrap();
    assert!(frame_within(&mut ws, Duration::from_millis(1500)).await.is_none(), "working is filtered");
    let r = push(&d, json!({ "title": "all set.", "state": "all_set", "session": "s1" })).await;
    let all_set = r.json::<Value>().await.unwrap()["id"].as_u64().unwrap();
    let f = frame_within(&mut ws, Duration::from_secs(5)).await.unwrap();
    assert_eq!(f["event"]["id"].as_u64(), Some(all_set));
    let r = push(&d, json!({ "title": "build ok", "level": "warn" })).await;
    let generic = r.json::<Value>().await.unwrap()["id"].as_u64().unwrap();
    let f = frame_within(&mut ws, Duration::from_secs(5)).await.unwrap();
    assert_eq!(f["event"]["id"].as_u64(), Some(generic), "generic level events always show");
    let hidden = push_ok_state(&d, "working").await;

    let s = get_status(&d).await;
    let recent = s["recent"].as_array().unwrap();
    assert_eq!(recent[0]["id"].as_u64(), Some(hidden));
    assert_eq!(recent[0]["shown"], false);
    let w = recent.iter().find(|e| e["id"].as_u64() == Some(working_id)).unwrap();
    assert_eq!((w["state"].as_str(), w["shown"].as_bool()), (Some("working"), Some(false)));

    let (_ws2, ids, cursor) = ws_synced(&d, 0).await;
    assert_eq!(ids, vec![all_set, generic], "replay skips filtered events");
    assert_eq!(cursor, hidden, "synced.last_id is the newest stored id, hidden or not");
    d.stop().await;
}

async fn push_ok_state(d: &Daemon, state: &str) -> u64 {
    let r = push(d, json!({ "title": "t", "state": state })).await;
    assert_eq!(r.status(), 200);
    r.json::<Value>().await.unwrap()["id"].as_u64().unwrap()
}

#[tokio::test]
async fn filter_change_applies_live_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let with_control = |dir: &Path| {
        state(dir, true).with_control(vec![AgentState::AllSet], Provider::Kiro, Control::load(dir), dir.to_path_buf())
    };
    let d = Daemon::start_with(with_control(dir.path())).await;
    assert_eq!(get_status(&d).await["filter"], json!({ "states": ["all_set"], "source": "config" }));
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    push_ok_state(&d, "working").await;
    assert!(frame_within(&mut ws, Duration::from_millis(1000)).await.is_none());

    let r = post_control(&d, json!({ "states": ["working", "all_set"] })).await;
    assert_eq!(r.status(), 200);
    let id = push_ok_state(&d, "working").await;
    let f = frame_within(&mut ws, Duration::from_secs(5)).await.expect("applies to the open socket");
    assert_eq!(f["event"]["id"].as_u64(), Some(id));
    assert_eq!(post_control(&d, json!({ "paused": true })).await.status(), 200);
    d.stop().await;

    let saved: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("control.json")).unwrap()).unwrap();
    assert_eq!(saved, json!({ "paused": true, "states": ["working", "all_set"] }));
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(dir.path().join("control.json")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    let d = Daemon::start_with(with_control(dir.path())).await;
    let s = get_status(&d).await;
    assert_eq!(s["filter"], json!({ "states": ["working", "all_set"], "source": "control" }));
    assert_eq!(s["paused"], true);
    d.stop().await;
}

// ---- turn-end classifier -----------------------------------------------------

const KIRO_SERVICE: &str = "nearpush-kiro-api-key";
const FIXTURE_KEY: &str = "fixture-key-not-real";

fn write_script(dir: &Path, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p.display().to_string()
}

/// Fake `security`: `find-generic-password -s <svc> [-w]`. Exits 0 for the services
/// in `present` (printing a fixture key with `-w`), 44 otherwise, as the real tool does.
fn fake_security(dir: &Path, present: &[&str]) -> String {
    let cases: String = present.iter().map(|s| format!("{s}) found=1 ;;\n")).collect();
    let script = format!(
        "[ \"$1\" = find-generic-password ] || exit 2\n\
         found=0\n\
         case \"$3\" in\n{cases}*) ;;\nesac\n\
         if [ \"$found\" != 1 ]; then echo 'security: item not found' >&2; exit 44; fi\n\
         if [ \"$4\" = -w ]; then printf '%s\\n' '{FIXTURE_KEY}'; fi\n\
         exit 0\n"
    );
    write_script(dir, "security", &script)
}

/// Daemon state with the classifier on. The kiro provider runs `kiro_script` through
/// /bin/sh, the keychain is the fake with `present` services, and the card hold is `hold`.
/// Loads control.json from `dir`, so a second call on the same dir is a restart.
fn classifier_state(dir: &Path, kiro_script: &str, present: &[&str], hold: Duration) -> AppState {
    let cfg = ClassifierConfig {
        security_program: fake_security(dir, present),
        kiro_command: vec!["/bin/sh".into(), "-c".into(), kiro_script.into()],
        ..ClassifierConfig::default()
    };
    state(dir, true)
        .with_classifier_hold(cfg, dir.join("classifier"), hold)
        .with_control(
            vec![AgentState::AllSet, AgentState::YourTurn],
            Provider::Kiro,
            Control::load(dir),
            dir.to_path_buf(),
        )
}

fn event_log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("events.jsonl")).unwrap_or_default()
}

/// Polls events.jsonl until it contains `needle`; panics after 5 s.
async fn wait_for_log(dir: &Path, needle: &str) -> String {
    for _ in 0..50 {
        let log = event_log(dir);
        if log.contains(needle) {
            return log;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{needle} never reached events.jsonl");
}

async fn pending_sessions(d: &Daemon) -> Vec<String> {
    get_status(d).await["pending_your_turn"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["session"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn classified_yes_defers_and_commits_your_turn() {
    let dir = tempfile::tempdir().unwrap();
    let st = classifier_state(dir.path(), "cat >/dev/null; echo YES", &[KIRO_SERVICE], Duration::from_secs(5));
    let d = Daemon::start_with(st).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    let r = push(&d, json!({
        "title": "your turn.", "state": "your_turn", "session": "s1", "detail": "rules line",
        "classify": { "reply_tail": "Two options.\nWhich one should I pick?", "question_line": "rules line" }
    })).await;
    assert_eq!(r.status(), 202);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["deferred"], true);
    assert!(body["id"].is_null(), "{body}");
    let f = frame_within(&mut ws, Duration::from_secs(5)).await.expect("card");
    assert_eq!(f["type"], "event");
    assert_eq!((f["event"]["state"].as_str(), f["event"]["title"].as_str()), (Some("your_turn"), Some("your turn.")));
    assert_eq!(f["event"]["detail"], "Which one should I pick?");
    d.stop().await;
}

#[tokio::test]
async fn classified_no_gives_all_set_with_took() {
    let dir = tempfile::tempdir().unwrap();
    let st = classifier_state(dir.path(), "cat >/dev/null; echo NO", &[KIRO_SERVICE], Duration::from_secs(5));
    let d = Daemon::start_with(st).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    let r = push(&d, json!({
        "title": "all set.", "state": "all_set", "session": "s1", "detail": "took 3m", "duration_ms": 180000,
        "classify": { "reply_tail": "Tests pass and the change is committed.", "took": "took 3m" }
    })).await;
    assert_eq!(r.status(), 202);
    let f = frame_within(&mut ws, Duration::from_secs(5)).await.expect("card");
    assert_eq!((f["event"]["state"].as_str(), f["event"]["detail"].as_str()), (Some("all_set"), Some("took 3m")));
    d.stop().await;
}

#[tokio::test]
async fn held_stops_of_one_session_commit_in_arrival_order() {
    let dir = tempfile::tempdir().unwrap();
    // The first reply's classifier answer takes 2 s; the second answers at once.
    let script = "p=$(cat); case \"$p\" in *SLOWFIRST*) sleep 2 ;; esac; echo YES";
    let st = classifier_state(dir.path(), script, &[KIRO_SERVICE], Duration::from_secs(5));
    let d = Daemon::start_with(st).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    let first = push(&d, json!({
        "title": "your turn.", "state": "your_turn", "session": "s1", "detail": "rules",
        "classify": { "reply_tail": "SLOWFIRST\nPick one?" }
    })).await;
    assert_eq!(first.status(), 202);
    let second = push(&d, json!({
        "title": "your turn.", "state": "your_turn", "session": "s1", "detail": "rules",
        "classify": { "reply_tail": "Pick two?" }
    })).await;
    assert_eq!(second.status(), 202);
    let mut details = Vec::new();
    while details.len() < 2 {
        let f = frame_within(&mut ws, Duration::from_secs(5)).await.expect("card");
        if f["type"] == "event" {
            details.push(f["event"]["detail"].as_str().unwrap_or_default().to_string());
        }
    }
    assert_eq!(details, ["Pick one?", "Pick two?"]);
    d.stop().await;
}

#[tokio::test]
async fn slow_provider_falls_back_to_rules_within_hold() {
    let dir = tempfile::tempdir().unwrap();
    let st = classifier_state(dir.path(), "cat >/dev/null; sleep 5; echo YES", &[KIRO_SERVICE], Duration::from_millis(300));
    let d = Daemon::start_with(st).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    let started = std::time::Instant::now();
    let r = push(&d, json!({
        "title": "your turn.", "state": "your_turn", "session": "s1", "detail": "rules line",
        "classify": { "reply_tail": "Pick one?" }
    })).await;
    assert_eq!(r.status(), 202);
    let f = frame_within(&mut ws, Duration::from_secs(3)).await.expect("rules card");
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!((f["event"]["state"].as_str(), f["event"]["detail"].as_str()), (Some("your_turn"), Some("rules line")));
    d.stop().await;
}

#[tokio::test]
async fn missing_keychain_item_falls_back_to_rules() {
    let dir = tempfile::tempdir().unwrap();
    let st = classifier_state(dir.path(), "cat >/dev/null; echo YES", &[], Duration::from_secs(5));
    let d = Daemon::start_with(st).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    push(&d, json!({
        "title": "your turn.", "state": "your_turn", "session": "s1", "detail": "rules line",
        "classify": { "reply_tail": "Pick one?" }
    })).await;
    let f = frame_within(&mut ws, Duration::from_secs(2)).await.expect("rules card");
    assert_eq!((f["event"]["state"].as_str(), f["event"]["detail"].as_str()), (Some("your_turn"), Some("rules line")));
    let log = event_log(dir.path());
    assert!(!log.contains(FIXTURE_KEY));
    d.stop().await;
}

#[tokio::test]
async fn waiting_workflow_does_not_skip_the_provider_or_show_on_your_turn() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("kiro-ran");
    let script = format!("touch '{}'; cat >/dev/null; echo YES", marker.display());
    let st = classifier_state(dir.path(), &script, &[KIRO_SERVICE], Duration::from_secs(5));
    let d = Daemon::start_with(st).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    let r = push(&d, json!({
        "title": "all set.", "state": "all_set", "session": "s1", "detail": "took 3m",
        "workflows_waiting": 1, "classify": { "reply_tail": "Pick one?" }
    })).await;
    assert_eq!(r.status(), 202, "held for the provider, workflow count or not");
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["deferred"], true);
    let f = frame_within(&mut ws, Duration::from_secs(5)).await.expect("card");
    assert_eq!(f["event"]["state"].as_str(), Some("your_turn"), "the provider verdict drives the card");
    let json = f["event"].to_string();
    assert!(!json.contains("workflows_") && !json.contains("wf waiting"), "your_turn shows no count anywhere: {json}");
    assert!(marker.exists(), "provider ran");
    d.stop().await;
}

#[tokio::test]
async fn model_all_set_with_waiting_workflow_shows_footer() {
    let dir = tempfile::tempdir().unwrap();
    let st = classifier_state(dir.path(), "cat >/dev/null; echo NO", &[KIRO_SERVICE], Duration::from_secs(5));
    let d = Daemon::start_with(st).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    // The rules said your_turn; the model's all_set is the final verdict, so the footer shows.
    let r = push(&d, json!({
        "title": "your turn.", "state": "your_turn", "session": "s1", "detail": "Pick one?", "duration_ms": 180000,
        "workflows_waiting": 1, "classify": { "reply_tail": "Pick one?", "question_line": "Pick one?", "took": "took 3m" }
    })).await;
    assert_eq!(r.status(), 202);
    let f = frame_within(&mut ws, Duration::from_secs(5)).await.expect("card");
    assert_eq!(f["event"]["state"].as_str(), Some("all_set"));
    assert_eq!(f["event"]["workflows_waiting"], 1);
    assert!(!f["event"]["detail"].as_str().unwrap_or_default().contains("wf"), "the count is not in the detail");
    d.stop().await;
}

#[tokio::test]
async fn rules_cards_show_the_footer_on_all_set_only() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    let cases = [
        (json!({ "title": "all set.", "state": "all_set", "session": "s1", "detail": "took 3m", "workflows_waiting": 1 }), Some(1)),
        (json!({ "title": "all set.", "state": "all_set", "session": "s1", "detail": "took 3m", "workflows_waiting": 0 }), None),
        (json!({ "title": "all set.", "state": "all_set", "session": "s1", "detail": "took 3m" }), None),
        (json!({ "title": "your turn.", "state": "your_turn", "session": "s1", "detail": "Pick one?", "workflows_waiting": 3,
                 "classify": { "reply_tail": "Pick one?" } }), None),
    ];
    for (body, want) in cases {
        assert_eq!(push(&d, body.clone()).await.status(), 200);
        let f = loop {
            let f = frame_within(&mut ws, Duration::from_secs(2)).await.expect("card");
            if f["type"] == "event" {
                break f;
            }
        };
        assert_eq!(f["event"]["workflows_waiting"].as_u64(), want, "{body}");
        if want.is_none() {
            assert!(!f["event"].to_string().contains("workflows_"), "{body}");
        }
    }
    d.stop().await;
}

#[tokio::test]
async fn disabled_classifier_commits_at_once_and_rejects_provider_change() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    let r = push(&d, json!({
        "title": "your turn.", "state": "your_turn", "session": "s1", "detail": "rules line",
        "classify": { "reply_tail": "Pick one?" }
    })).await;
    assert_eq!(r.status(), 200);
    assert!(r.json::<Value>().await.unwrap()["id"].is_number());
    let f = frame_within(&mut ws, Duration::from_secs(2)).await.expect("card");
    assert_eq!(f["event"]["detail"], "rules line");

    let s = get_status(&d).await;
    assert_eq!(s["classifier"]["enabled"], false);
    assert_eq!(s["classifier"]["provider"], "kiro");
    assert!(s["classifier"]["key_present"].is_null());
    assert_eq!(post_control(&d, json!({ "classifier": "typesafe" })).await.status(), 409);
    d.stop().await;
}

#[tokio::test]
async fn provider_change_persists_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start_with(classifier_state(dir.path(), "echo YES", &[], Duration::from_secs(5))).await;
    assert_eq!(post_control(&d, json!({ "classifier": "typesafe" })).await.status(), 200);
    let s = get_status(&d).await;
    assert_eq!(
        (s["classifier"]["provider"].as_str(), s["classifier"]["source"].as_str(), s["classifier"]["model"].as_str()),
        (Some("typesafe"), Some("control"), Some("jev-1.13.0"))
    );
    assert_eq!(s["classifier"]["key_service"], "nearpush-typesafe-api-key");
    assert_eq!(s["classifier"]["key_present"], false, "fake keychain has no typesafe item");
    d.stop().await;

    let d = Daemon::start_with(classifier_state(dir.path(), "echo YES", &[], Duration::from_secs(5))).await;
    let s = get_status(&d).await;
    assert_eq!((s["classifier"]["provider"].as_str(), s["classifier"]["source"].as_str()), (Some("typesafe"), Some("control")));
    d.stop().await;
}

#[tokio::test]
async fn unknown_provider_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start_with(classifier_state(dir.path(), "echo YES", &[], Duration::from_secs(5))).await;
    assert_eq!(post_control(&d, json!({ "classifier": "jev" })).await.status(), 400);
    assert_eq!(get_status(&d).await["classifier"]["provider"], "kiro", "rejected request changes nothing");
    d.stop().await;
}

#[tokio::test]
async fn reply_tail_never_reaches_the_event_log() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start_with(classifier_state(dir.path(), "cat >/dev/null; echo YES", &[KIRO_SERVICE], Duration::from_secs(5))).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    push(&d, json!({
        "title": "your turn.", "state": "your_turn", "session": "s1",
        "classify": { "reply_tail": "alpha marker-XYZ line\nWhich one should I pick?" }
    })).await;
    frame_within(&mut ws, Duration::from_secs(5)).await.expect("card");
    let log = event_log(dir.path());
    assert!(log.contains("Which one should I pick?"), "{log}");
    assert!(!log.contains("marker-XYZ"), "{log}");
    d.stop().await;
}

#[tokio::test]
async fn paused_events_are_still_classified_and_stored_muted() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("kiro-ran");
    let script = format!("touch '{}'; cat >/dev/null; echo YES", marker.display());
    let d = Daemon::start_with(classifier_state(dir.path(), &script, &[KIRO_SERVICE], Duration::from_secs(5))).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    assert_eq!(post_control(&d, json!({ "paused": true })).await.status(), 200);
    let r = push(&d, json!({
        "title": "your turn.", "state": "your_turn", "session": "s1",
        "classify": { "reply_tail": "Which one?" }
    })).await;
    assert_eq!(r.status(), 202);
    let log = wait_for_log(dir.path(), "\"muted\":true").await;
    assert!(log.contains("\"detail\":\"Which one?\""), "{log}");
    assert!(marker.exists(), "classifier ran while paused");
    assert!(frame_within(&mut ws, Duration::from_millis(500)).await.is_none(), "paused: nothing broadcast");
    d.stop().await;
}

// ---- turn-end record ------------------------------------------------------------

#[tokio::test]
async fn model_verdict_is_recorded_with_provider_source_and_question() {
    let dir = tempfile::tempdir().unwrap();
    let st = classifier_state(dir.path(), "cat >/dev/null; echo YES", &[KIRO_SERVICE], Duration::from_secs(5));
    let d = Daemon::start_with(st).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    push(&d, json!({
        "title": "your turn.", "state": "your_turn", "session": "s1", "detail": "rules line",
        "classify": { "reply_tail": "Two options.\nWhich one should I pick?", "question_line": "rules line" }
    })).await;
    frame_within(&mut ws, Duration::from_secs(5)).await.expect("card");
    let line: Value = serde_json::from_str(event_log(dir.path()).lines().next().unwrap()).unwrap();
    assert_eq!(
        (line["classify_provider"].as_str(), line["verdict_source"].as_str(), line["question_line"].as_str()),
        (Some("kiro"), Some("model"), Some("Which one should I pick?"))
    );
    let status = get_status(&d).await;
    let recent = &status["recent"][0];
    assert_eq!((recent["classify_provider"].as_str(), recent["verdict_source"].as_str()), (Some("kiro"), Some("model")));
    d.stop().await;
}

#[tokio::test]
async fn timed_out_verdict_is_recorded_as_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let st = classifier_state(dir.path(), "cat >/dev/null; sleep 5; echo YES", &[KIRO_SERVICE], Duration::from_millis(300));
    let d = Daemon::start_with(st).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    push(&d, json!({
        "title": "your turn.", "state": "your_turn", "session": "s1", "detail": "rules line",
        "classify": { "reply_tail": "Pick one?" }
    })).await;
    frame_within(&mut ws, Duration::from_secs(3)).await.expect("rules card");
    let line: Value = serde_json::from_str(event_log(dir.path()).lines().next().unwrap()).unwrap();
    assert_eq!(
        (line["classify_provider"].as_str(), line["verdict_source"].as_str(), line["question_line"].as_str()),
        (Some("kiro"), Some("fallback"), Some("rules line"))
    );
    d.stop().await;
}

// ---- main card: pending your_turn sessions, working card, SessionEnd ----------

fn event_line_count(dir: &Path) -> usize {
    event_log(dir).lines().count()
}

/// A daemon whose pending sessions and controls live in `dir` and survive a restart.
fn persistent_state(dir: &Path) -> AppState {
    let saved = Control::load(dir);
    let pending = saved.pending.clone();
    let (hidden, dismissing) = (saved.hidden_through, saved.dismissing.clone());
    state(dir, true)
        .with_control(vec![AgentState::AllSet, AgentState::YourTurn, AgentState::Allow], Provider::Kiro, saved, dir.to_path_buf())
        .with_pending(pending)
        .with_dismissals(hidden, dismissing)
}

async fn pending_count(d: &Daemon) -> u64 {
    get_status(d).await["pending_count"].as_u64().unwrap()
}

#[tokio::test]
async fn pending_your_turn_then_prompt_shows_working_card() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    push(&d, json!({ "title": "your turn.", "state": "your_turn", "session": "s1", "detail": "Pick?" })).await;
    assert_eq!(pending_sessions(&d).await, vec!["s1"]);
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    let before = event_line_count(dir.path());

    let r = push(&d, json!({ "title": "working.", "state": "working", "session": "s1", "turn_start": true })).await;
    assert_eq!(r.status(), 200);
    assert!(r.json::<Value>().await.unwrap()["id"].is_null());
    let f = main_within(&mut ws, Duration::from_secs(5)).await.expect("main card");
    assert_eq!((f["card"]["state"].as_str(), f["card"]["session"].as_str()), (Some("working"), Some("s1")));
    assert!(f["card"]["started_at"].is_u64());
    assert_eq!(event_line_count(dir.path()), before, "nothing stored");
    assert!(pending_sessions(&d).await.is_empty());
    d.stop().await;
}

#[tokio::test]
async fn prompt_without_pending_is_stored_as_working_event() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    let r = push(&d, json!({ "title": "working.", "state": "working", "session": "s1", "turn_start": true })).await;
    assert!(r.json::<Value>().await.unwrap()["id"].is_number());
    assert_eq!(event_line_count(dir.path()), 1);
    d.stop().await;
}

#[tokio::test]
async fn other_sessions_pending_entry_is_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    push(&d, json!({ "title": "your turn.", "state": "your_turn", "session": "s1" })).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    let r = push(&d, json!({ "title": "working.", "state": "working", "session": "s2", "turn_start": true })).await;
    assert!(r.json::<Value>().await.unwrap()["id"].is_number(), "committed normally");
    assert!(raw_frame_within(&mut ws, Duration::from_millis(800)).await.is_none(), "no frame for s2");
    assert_eq!(pending_sessions(&d).await, vec!["s1"]);
    d.stop().await;
}

#[tokio::test]
async fn stop_after_working_ends_working_card_and_sets_pending_only_for_your_turn() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    push(&d, json!({ "title": "your turn.", "state": "your_turn", "session": "s1" })).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    push(&d, json!({ "title": "working.", "state": "working", "session": "s1", "turn_start": true })).await;
    assert_eq!(main_within(&mut ws, Duration::from_secs(5)).await.unwrap()["card"]["state"], "working");
    assert!(pending_sessions(&d).await.is_empty());

    push(&d, json!({ "title": "all set.", "state": "all_set", "session": "s1" })).await;
    let f = frame_within(&mut ws, Duration::from_secs(5)).await.unwrap();
    assert_eq!((f["type"].as_str(), f["event"]["state"].as_str()), (Some("event"), Some("all_set")));
    // The working card ended with the Stop, so the stream shows the card again.
    assert!(main_within(&mut ws, Duration::from_secs(5)).await.unwrap()["card"].is_null());
    assert!(pending_sessions(&d).await.is_empty());

    push(&d, json!({ "title": "your turn.", "state": "your_turn", "session": "s1" })).await;
    assert_eq!(pending_sessions(&d).await, vec!["s1"]);
    d.stop().await;
}

#[tokio::test]
async fn user_scenario_keeps_your_turn_while_any_session_waits() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;

    push(&d, json!({ "title": "your turn.", "state": "your_turn", "session": "A", "detail": "A asks?" })).await;
    let f = main_within(&mut ws, Duration::from_secs(5)).await.unwrap();
    assert_eq!(
        (f["card"]["state"].as_str(), f["card"]["session"].as_str(), f["card"]["count"].as_u64()),
        (Some("your_turn"), Some("A"), Some(1))
    );

    push(&d, json!({ "title": "your turn.", "state": "your_turn", "session": "B", "detail": "B asks?" })).await;
    let f = main_within(&mut ws, Duration::from_secs(5)).await.unwrap();
    assert_eq!((f["card"]["session"].as_str(), f["card"]["count"].as_u64()), (Some("B"), Some(2)));

    // Client 1 (A) finishes without asking anything. B still waits.
    push(&d, json!({ "title": "all set.", "state": "all_set", "session": "A", "detail": "took 1m" })).await;
    let f = main_within(&mut ws, Duration::from_secs(5)).await.unwrap();
    assert_eq!(
        (f["card"]["state"].as_str(), f["card"]["session"].as_str(), f["card"]["count"].as_u64(), f["card"]["question_line"].as_str()),
        (Some("your_turn"), Some("B"), Some(1), Some("B asks?"))
    );
    assert_eq!(pending_count(&d).await, 1);
    // The all_set still reaches history.
    let recent = get_status(&d).await["recent"].as_array().unwrap().clone();
    assert!(recent.iter().any(|e| e["state"] == "all_set" && e["session"] == "A"));

    // B answers. Nothing waits any more, so the card reads working. for B.
    push(&d, json!({ "title": "working.", "state": "working", "session": "B", "turn_start": true })).await;
    let f = main_within(&mut ws, Duration::from_secs(5)).await.unwrap();
    assert_eq!((f["card"]["state"].as_str(), f["card"]["session"].as_str()), (Some("working"), Some("B")));
    assert_eq!(pending_count(&d).await, 0);
    d.stop().await;
}

#[tokio::test]
async fn session_end_clears_its_pending_entry_without_storing_an_event() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    push(&d, json!({ "title": "your turn.", "state": "your_turn", "session": "s1" })).await;
    assert_eq!(main_within(&mut ws, Duration::from_secs(5)).await.unwrap()["card"]["state"], "your_turn");
    let before = event_line_count(dir.path());

    let r = push(&d, json!({ "title": "session ended", "session": "s1", "session_end": true })).await;
    assert_eq!(r.status(), 200);
    assert!(r.json::<Value>().await.unwrap()["id"].is_null());
    assert!(pending_sessions(&d).await.is_empty());
    assert_eq!(event_line_count(dir.path()), before, "session_end is not stored");
    assert!(main_within(&mut ws, Duration::from_secs(5)).await.unwrap()["card"].is_null());
    d.stop().await;
}

#[tokio::test]
async fn pending_sessions_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let d = Daemon::start_with(persistent_state(dir.path())).await;
        push(&d, json!({ "title": "your turn.", "state": "your_turn", "session": "s1", "detail": "first?", "project": "alpha" })).await;
        push(&d, json!({ "title": "your turn.", "state": "your_turn", "session": "s2", "detail": "second?", "project": "beta" })).await;
        assert_eq!(pending_count(&d).await, 2);
        d.stop().await;
    }
    let path = dir.path().join("control.json");
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);

    let d = Daemon::start_with(persistent_state(dir.path())).await;
    assert_eq!(pending_sessions(&d).await.len(), 2);
    let mut ws = open_ws(&d.ws_url(0), &[]).await.unwrap();
    // The reloaded card is the most recent pending session, and the count covers both.
    let f = main_within(&mut ws, Duration::from_secs(5)).await.unwrap();
    assert_eq!(
        (f["card"]["session"].as_str(), f["card"]["count"].as_u64(), f["card"]["question_line"].as_str()),
        (Some("s2"), Some(2), Some("second?"))
    );

    // A /control change rewrites control.json and must keep the pending cards.
    assert_eq!(post_control(&d, json!({ "states": ["all_set", "your_turn"] })).await.status(), 200);
    d.stop().await;
    let d = Daemon::start_with(persistent_state(dir.path())).await;
    assert_eq!(pending_count(&d).await, 2, "still pending after a control change and restart");
    d.stop().await;
}

#[tokio::test]
async fn paused_display_tracks_pending_but_broadcasts_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    assert_eq!(post_control(&d, json!({ "paused": true })).await.status(), 200);

    push(&d, json!({ "title": "your turn.", "state": "your_turn", "session": "s1" })).await;
    assert_eq!(pending_sessions(&d).await, vec!["s1"], "pending still updates while paused");
    assert!(raw_frame_within(&mut ws, Duration::from_millis(800)).await.is_none(), "nothing broadcast while paused");

    // Resuming sends the card that changed while paused.
    assert_eq!(post_control(&d, json!({ "paused": false })).await.status(), 200);
    let f = main_within(&mut ws, Duration::from_secs(5)).await.expect("card after resume");
    assert_eq!(f["card"]["state"], "your_turn");
    d.stop().await;
}

// ---- swipe-to-dismiss: /v1/dismiss and /v1/undismiss ---------------------------

fn same_origin(d: &Daemon) -> String {
    format!("http://{}", d.addr)
}

/// A tailnet viewer request as tailscale serve forwards it, from the page's own origin.
async fn viewer_post(d: &Daemon, path: &str, body: Value) -> reqwest::Response {
    viewer_post_as(d, path, body, LOGIN, Some(&same_origin(d))).await
}

async fn viewer_post_as(d: &Daemon, path: &str, body: Value, login: &str, origin: Option<&str>) -> reqwest::Response {
    let mut r = client()
        .post(d.url(path))
        .header("Tailscale-User-Login", login)
        .header("X-Forwarded-For", "192.0.2.2")
        .json(&body);
    if let Some(o) = origin {
        r = r.header("Origin", o);
    }
    r.send().await.unwrap()
}

async fn local_post(d: &Daemon, path: &str, body: Value) -> reqwest::Response {
    client().post(d.url(path)).bearer_auth(TOKEN).json(&body).send().await.unwrap()
}

async fn hidden_within(ws: &mut Ws, wait: Duration) -> Option<Value> {
    frame_within_where(ws, wait, |v| v["type"] == "hidden").await
}

#[tokio::test]
async fn dismissing_your_turn_shows_next_pending_then_all_set_and_undo_restores() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    push(&d, json!({ "title": "your turn.", "state": "your_turn", "session": "TEST-A", "detail": "A?" })).await;
    push(&d, json!({ "title": "your turn.", "state": "your_turn", "session": "TEST-B", "detail": "B?" })).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;

    let r = viewer_post(&d, "/v1/dismiss", json!({ "session": "TEST-B" })).await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["dismissed"], "pending");
    assert!(v["undo_ms"].as_u64().unwrap() >= 5000);
    let f = main_within(&mut ws, Duration::from_secs(5)).await.unwrap();
    assert_eq!((f["card"]["session"].as_str(), f["card"]["count"].as_u64()), (Some("TEST-A"), Some(1)));
    assert_eq!(pending_sessions(&d).await, vec!["ESTA"]);

    // The last pending one (dismissed through loopback + token): the stream shows all set.
    assert_eq!(local_post(&d, "/v1/dismiss", json!({ "session": "TEST-A" })).await.status(), 200);
    assert!(main_within(&mut ws, Duration::from_secs(5)).await.unwrap()["card"].is_null());
    assert_eq!(pending_count(&d).await, 0);

    // Undo puts it back, for this viewer and every other one.
    let r = viewer_post(&d, "/v1/undismiss", json!({ "session": "TEST-A" })).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["restored"], "pending");
    let f = main_within(&mut ws, Duration::from_secs(5)).await.unwrap();
    assert_eq!((f["card"]["state"].as_str(), f["card"]["question_line"].as_str()), (Some("your_turn"), Some("A?")));
    assert_eq!(viewer_post(&d, "/v1/undismiss", json!({ "session": "TEST-A" })).await.status(), 404, "only once");

    // The same session asking again later is pending as normal.
    assert_eq!(local_post(&d, "/v1/dismiss", json!({ "session": "TEST-A" })).await.status(), 200);
    push(&d, json!({ "title": "your turn.", "state": "your_turn", "session": "TEST-A", "detail": "again?" })).await;
    assert_eq!(pending_count(&d).await, 1);
    assert_eq!(viewer_post(&d, "/v1/undismiss", json!({ "session": "TEST-A" })).await.status(), 409, "newer card wins");
    d.stop().await;
}

#[tokio::test]
async fn dismiss_requires_an_allowed_login_same_origin_or_the_token() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    push(&d, json!({ "title": "your turn.", "state": "your_turn", "session": "TEST-1" })).await;
    let body = json!({ "session": "TEST-1" });
    let origin = same_origin(&d);

    let r = viewer_post_as(&d, "/v1/dismiss", body.clone(), "mallory@example.com", Some(&origin)).await;
    assert_eq!(r.status(), 403, "login not in allowed_logins");
    let r = viewer_post_as(&d, "/v1/dismiss", body.clone(), LOGIN, Some("https://evil.example")).await;
    assert_eq!(r.status(), 403, "cross-origin");
    let r = viewer_post_as(&d, "/v1/dismiss", body.clone(), LOGIN, Some("null")).await;
    assert_eq!(r.status(), 403, "opaque origin");
    let r = viewer_post_as(&d, "/v1/dismiss", body.clone(), LOGIN, None).await;
    assert_eq!(r.status(), 403, "a viewer must send Origin");
    let r = client()
        .post(d.url("/v1/dismiss"))
        .header("Tailscale-User-Login", LOGIN)
        .header("Origin", &origin)
        .header("Content-Type", "text/plain")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 415, "a form-style body is refused");
    // Proxied without a login (e.g. a tagged node), or loopback without the token.
    let r = client().post(d.url("/v1/dismiss")).header("X-Forwarded-For", "192.0.2.2").bearer_auth(TOKEN).json(&body).send().await.unwrap();
    assert_eq!(r.status(), 403);
    let r = client().post(d.url("/v1/dismiss")).json(&body).send().await.unwrap();
    assert_eq!(r.status(), 401, "the local-testing flag does not open dismiss");
    let r = client().post(d.url("/v1/dismiss")).bearer_auth("wrong").json(&body).send().await.unwrap();
    assert_eq!(r.status(), 401);
    let r = client().post(d.url("/v1/undismiss")).json(&body).send().await.unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(pending_count(&d).await, 1, "nothing refused changed anything");
    d.stop().await;
}

#[tokio::test]
async fn dismiss_validates_strictly_and_never_creates_an_event() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    push(&d, json!({ "title": "your turn.", "state": "your_turn", "session": "TEST-1" })).await;
    let before = event_line_count(dir.path());
    let last = get_status(&d).await["last_event_id"].as_u64().unwrap();
    for body in [
        json!({}),
        json!({ "session": "TEST-1", "event_id": 1 }),
        json!({ "session": "TEST-1", "title": "injected", "state": "your_turn" }),
        json!({ "session": "" }),
        json!({ "session": "a\nb" }),
        json!({ "session": "x".repeat(129) }),
        json!({ "event_id": 0 }),
        json!({ "event_id": -1 }),
        json!({ "event_id": "1" }),
        json!([1]),
    ] {
        // Ten bad bodies use up the viewer's budget; the checks below come from loopback.
        let r = viewer_post(&d, "/v1/dismiss", body.clone()).await;
        assert_eq!(r.status(), 400, "{body}");
    }
    assert_eq!(local_post(&d, "/v1/dismiss", json!({ "session": "TEST-unknown" })).await.status(), 404);
    assert_eq!(local_post(&d, "/v1/dismiss", json!({ "event_id": 999 })).await.status(), 404);
    assert_eq!(local_post(&d, "/v1/undismiss", json!({ "session": "TEST-1" })).await.status(), 404, "nothing to undo");
    assert_eq!(event_line_count(dir.path()), before);
    assert_eq!(get_status(&d).await["last_event_id"].as_u64().unwrap(), last);
    assert_eq!(pending_count(&d).await, 1);

    // A real dismissal stores nothing during its undo window either.
    assert_eq!(local_post(&d, "/v1/dismiss", json!({ "session": "TEST-1" })).await.status(), 200);
    assert_eq!(event_line_count(dir.path()), before);
    d.stop().await;
}

#[tokio::test]
async fn viewer_dismiss_cannot_touch_control_state() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    push(&d, json!({ "title": "your turn.", "state": "your_turn", "session": "TEST-1" })).await;
    let snapshot = |s: &Value| (s["paused"].clone(), s["filter"].clone(), s["classifier"].clone());
    let before = snapshot(&get_status(&d).await);

    for body in [json!({ "paused": true }), json!({ "session": "TEST-1", "paused": true }), json!({ "states": ["working"] })] {
        assert_eq!(viewer_post(&d, "/v1/dismiss", body.clone()).await.status(), 400, "{body}");
    }
    let r = viewer_post(&d, "/control", json!({ "paused": true })).await;
    assert_eq!(r.status(), 403, "/control stays loopback + token only");
    assert_eq!(viewer_post(&d, "/v1/dismiss", json!({ "session": "TEST-1" })).await.status(), 200);
    assert_eq!(viewer_post(&d, "/v1/undismiss", json!({ "session": "TEST-1" })).await.status(), 200);
    assert_eq!(snapshot(&get_status(&d).await), before);
    d.stop().await;
}

#[tokio::test]
async fn dismissal_becomes_final_with_a_history_record() {
    let dir = tempfile::tempdir().unwrap();
    let st = state(dir.path(), true).with_undo_window(Duration::from_millis(300));
    let d = Daemon::start_with(st).await;
    push(&d, json!({ "title": "your turn.", "state": "your_turn", "session": "TEST-1", "detail": "Pick?", "project": "np" })).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    assert_eq!(viewer_post(&d, "/v1/dismiss", json!({ "session": "TEST-1" })).await.status(), 200);

    let f = frame_within_where(&mut ws, Duration::from_secs(5), |v| v["type"] == "event").await.unwrap();
    let e = &f["event"];
    assert_eq!((e["dismissed"].as_bool(), e["title"].as_str(), e["session"].as_str()), (Some(true), Some("dismissed"), Some("TEST-1")));
    assert!(e["state"].is_null(), "the record is no card state");
    assert_eq!(e["detail"], "Pick?");
    let log = wait_for_log(dir.path(), "\"dismissed\":true").await;
    assert_eq!(log.lines().count(), 2);
    assert_eq!(viewer_post(&d, "/v1/undismiss", json!({ "session": "TEST-1" })).await.status(), 404, "final");
    assert_eq!(pending_count(&d).await, 0);
    let recent = get_status(&d).await["recent"][0].clone();
    assert_eq!((recent["dismissed"].as_bool(), recent["shown"].as_bool()), (Some(true), Some(false)));
    d.stop().await;
}

#[tokio::test]
async fn dismissing_a_stored_card_hides_it_for_every_viewer() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    let id = push_ok_state(&d, "all_set").await;
    let (mut a, _, _) = ws_synced(&d, 0).await;
    let (mut b, _, _) = ws_synced(&d, 0).await;

    let r = viewer_post(&d, "/v1/dismiss", json!({ "event_id": id })).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["dismissed"], "event");
    for ws in [&mut a, &mut b] {
        assert_eq!(hidden_within(ws, Duration::from_secs(5)).await.unwrap()["through"].as_u64(), Some(id));
    }
    assert_eq!(viewer_post(&d, "/v1/dismiss", json!({ "event_id": id })).await.status(), 404, "already hidden");
    // A page that connects now learns it on connect.
    let mut c = open_ws(&d.ws_url(0), &[]).await.unwrap();
    assert_eq!(hidden_within(&mut c, Duration::from_secs(5)).await.unwrap()["through"].as_u64(), Some(id));

    assert_eq!(viewer_post(&d, "/v1/undismiss", json!({ "event_id": id })).await.status(), 200);
    assert!(hidden_within(&mut a, Duration::from_secs(5)).await.unwrap()["through"].is_null());
    d.stop().await;
}

#[tokio::test]
async fn dismissing_the_working_card_hides_it() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    push(&d, json!({ "title": "your turn.", "state": "your_turn", "session": "TEST-1" })).await;
    push(&d, json!({ "title": "working.", "state": "working", "session": "TEST-1", "turn_start": true })).await;
    let (mut ws, _, _) = ws_synced(&d, 0).await;
    let r = viewer_post(&d, "/v1/dismiss", json!({ "session": "TEST-1" })).await;
    assert_eq!(r.json::<Value>().await.unwrap()["dismissed"], "working");
    assert!(main_within(&mut ws, Duration::from_secs(5)).await.unwrap()["card"].is_null());
    assert!(hidden_within(&mut ws, Duration::from_secs(5)).await.unwrap()["through"].is_u64());
    assert_eq!(viewer_post(&d, "/v1/undismiss", json!({ "session": "TEST-1" })).await.status(), 200);
    assert_eq!(main_within(&mut ws, Duration::from_secs(5)).await.unwrap()["card"]["state"], "working");
    d.stop().await;
}

#[tokio::test]
async fn dismissal_survives_a_restart_and_becomes_final() {
    let dir = tempfile::tempdir().unwrap();
    {
        let d = Daemon::start_with(persistent_state(dir.path()).with_undo_window(Duration::from_secs(60))).await;
        push(&d, json!({ "title": "your turn.", "state": "your_turn", "session": "TEST-1" })).await;
        push(&d, json!({ "title": "your turn.", "state": "your_turn", "session": "TEST-2" })).await;
        assert_eq!(viewer_post(&d, "/v1/dismiss", json!({ "session": "TEST-2" })).await.status(), 200);
        d.stop().await;
    }
    let saved = Control::load(dir.path());
    assert_eq!(saved.pending.len(), 1);
    assert_eq!(saved.dismissing.len(), 1, "the open dismissal is saved");

    let d = Daemon::start_with(persistent_state(dir.path())).await;
    assert_eq!(pending_sessions(&d).await, vec!["EST1"]);
    let log = wait_for_log(dir.path(), "\"dismissed\":true").await;
    assert!(log.contains("TEST-2"));
    assert_eq!(viewer_post(&d, "/v1/undismiss", json!({ "session": "TEST-2" })).await.status(), 404, "a restart makes it final");
    d.stop().await;
    assert!(Control::load(dir.path()).dismissing.is_empty());
}

#[tokio::test]
async fn dismiss_is_rate_limited_per_login() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(dir.path(), true).await;
    for _ in 0..server::DISMISS_PER_MINUTE {
        assert_eq!(viewer_post(&d, "/v1/dismiss", json!({ "session": "TEST-none" })).await.status(), 404);
    }
    assert_eq!(viewer_post(&d, "/v1/dismiss", json!({ "session": "TEST-none" })).await.status(), 429);
    assert_eq!(viewer_post(&d, "/v1/undismiss", json!({ "session": "TEST-none" })).await.status(), 429);
    // Another caller has its own budget.
    assert_eq!(local_post(&d, "/v1/dismiss", json!({ "session": "TEST-none" })).await.status(), 404);
    d.stop().await;
}
