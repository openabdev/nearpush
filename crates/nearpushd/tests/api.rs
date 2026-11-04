//! End-to-end tests against a real listener: push auth, persistence and replay.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use nearpushd::assets;
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
const LOGIN: &str = "pahud@example.com";

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
        ("Tailscale-User-Name", "Pahud"),
        ("Tailscale-Headers-Info", "x"),
        ("X-Forwarded-For", "100.64.0.2"),
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
        .layer(MockConnectInfo(SocketAddr::from(([100, 64, 1, 2], 40000))));

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
    assert!(open_ws(&d.ws_url(0), &[("tailscale-user-login", "PAHUD@example.com"), ("origin", &origin)]).await.is_ok());

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
        .with_control(vec![AgentState::Working, AgentState::AllSet], Control::default(), dir.to_path_buf())
}

/// Next frame that is not a ping, or `None` if nothing arrives within `wait`.
async fn frame_within(ws: &mut Ws, wait: Duration) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let msg = tokio::time::timeout_at(deadline, ws.next()).await.ok()??.ok()?;
        if let tungstenite::Message::Text(t) = msg {
            let v: Value = serde_json::from_str(t.as_str()).unwrap();
            if v["type"] != "ping" {
                return Some(v);
            }
        }
    }
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
            "synced" => return (ws, ids, f["last_id"].as_u64().unwrap()),
            other => panic!("unexpected frame {other}"),
        }
    }
}

const PROXY_HEADERS: [(&str, &str); 3] =
    [("Tailscale-User-Login", LOGIN), ("Tailscale-User-Name", "Pahud"), ("X-Forwarded-For", "100.64.0.2")];

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
    let app = server::router(state(dir.path(), true)).layer(MockConnectInfo(SocketAddr::from(([100, 64, 1, 2], 40000))));
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
    let st = summarizer_state(dir.path(), "cat >/dev/null; echo label", 5).with_public_url(Some("https://lap.ts.net/".into()));
    let d = Daemon::start_with(st).await;
    let s = get_status(&d).await;
    assert_eq!(s["public_url"], "https://lap.ts.net/");
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
    let app = server::router(state(dir.path(), true)).layer(MockConnectInfo(SocketAddr::from(([100, 64, 1, 2], 40000))));
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
        state(dir, true).with_control(vec![AgentState::AllSet], Control::load(dir), dir.to_path_buf())
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
