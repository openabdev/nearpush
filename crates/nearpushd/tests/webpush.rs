//! Web Push end to end: a real listener, a local mock push service on 127.0.0.1 that
//! decrypts what it receives, and a fake relay. No real push service, keychain or
//! config directory is touched.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::State as AxState;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64ct::{Base64UrlUnpadded, Encoding};
use nearpushd::classifier::{ClassifierConfig, Provider};
use nearpushd::config::{RelayConfig, WebPushConfig};
use nearpushd::control::Control;
use nearpushd::event::AgentState;
use nearpushd::server::{self, AppState};
use nearpushd::store::{Store, now_ms};
use nearpushd::webpush::{self, EndpointPolicy, WebPush, WriteAtomic};
use p256::ecdsa::signature::Verifier;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use serde_json::{Value, json};
use tokio::net::TcpListener;

const TOKEN: &str = "test-token-0123456789";
const LOGIN: &str = "you@example.com";

// ---- mock push service ----------------------------------------------------------

#[derive(Clone)]
enum Answer {
    Status(u16),
    RetryAfter(u16, &'static str),
    /// Holds the request open, then answers.
    Hold(Duration, u16),
    /// A hostile body that echoes what the daemon sent.
    Echo(u16, &'static str),
}

#[derive(Clone)]
struct Seen {
    headers: HeaderMap,
    body: Vec<u8>,
    at: Instant,
}

#[derive(Default)]
struct MockState {
    scripts: HashMap<String, VecDeque<Answer>>,
    seen: HashMap<String, Vec<Seen>>,
}

#[derive(Clone)]
struct Mock {
    port: u16,
    st: Arc<Mutex<MockState>>,
}

async fn mock_handler(AxState(st): AxState<Arc<Mutex<MockState>>>, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let path = uri.path().to_string();
    let answer = {
        let mut s = st.lock().unwrap();
        s.seen.entry(path.clone()).or_default().push(Seen { headers: headers.clone(), body: body.to_vec(), at: Instant::now() });
        s.scripts.get_mut(&path).and_then(|q| q.pop_front()).unwrap_or(Answer::Status(201))
    };
    let code = |c: u16| StatusCode::from_u16(c).unwrap();
    match answer {
        Answer::Status(c) => code(c).into_response(),
        Answer::RetryAfter(c, v) => (code(c), [("retry-after", v)]).into_response(),
        Answer::Hold(d, c) => {
            tokio::time::sleep(d).await;
            code(c).into_response()
        }
        Answer::Echo(c, marker) => {
            let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
            (code(c), format!("{{\"reason\":\"{marker} {path} {auth}\"}}")).into_response()
        }
    }
}

impl Mock {
    async fn start() -> Mock {
        let st = Arc::new(Mutex::new(MockState::default()));
        let app = Router::new().fallback(mock_handler).with_state(st.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Mock { port, st }
    }

    fn path(name: &str) -> String {
        format!("/push/{name}")
    }

    fn endpoint(&self, name: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.port, Mock::path(name))
    }

    fn script(&self, name: &str, answers: &[Answer]) {
        self.st.lock().unwrap().scripts.entry(Mock::path(name)).or_default().extend(answers.iter().cloned());
    }

    fn seen(&self, name: &str) -> Vec<Seen> {
        self.st.lock().unwrap().seen.get(&Mock::path(name)).cloned().unwrap_or_default()
    }

    fn total(&self) -> usize {
        self.st.lock().unwrap().seen.values().map(Vec::len).sum()
    }
}

/// A browser's subscription keys (test keys only).
struct Ua {
    secret: p256::SecretKey,
    auth: [u8; 16],
}

impl Ua {
    fn new() -> Ua {
        loop {
            let mut b = [0u8; 32];
            getrandom::fill(&mut b).unwrap();
            if let Ok(secret) = p256::SecretKey::from_bytes(&p256::FieldBytes::clone_from_slice(&b)) {
                let mut auth = [0u8; 16];
                getrandom::fill(&mut auth).unwrap();
                return Ua { secret, auth };
            }
        }
    }

    fn p256dh(&self) -> String {
        Base64UrlUnpadded::encode_string(self.secret.public_key().to_encoded_point(false).as_bytes())
    }

    fn auth_b64(&self) -> String {
        Base64UrlUnpadded::encode_string(&self.auth)
    }

    fn decrypt(&self, body: &[u8]) -> Option<Value> {
        let plain =
            web_push_native::decrypt(body.to_vec(), &self.secret, &web_push_native::Auth::clone_from_slice(&self.auth)).ok()?;
        assert_eq!(plain.len(), webpush::PAYLOAD_BYTES, "padded payload");
        serde_json::from_slice(&plain).ok()
    }
}

// ---- daemon fixture -------------------------------------------------------------

struct Opts {
    cfg: WebPushConfig,
    writer: WriteAtomic,
    loopback_policy: bool,
    allow_loopback: bool,
    webpush: bool,
    classifier: Option<(&'static str, bool)>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            cfg: WebPushConfig { enabled: true, subject: Some("mailto:you@example.com".into()), ..WebPushConfig::default() },
            writer: webpush::default_writer(),
            loopback_policy: true,
            allow_loopback: false,
            webpush: true,
            classifier: None,
        }
    }
}

struct Fx {
    _dir: tempfile::TempDir,
    addr: SocketAddr,
    task: tokio::task::JoinHandle<io::Result<()>>,
    state: AppState,
    mock: Mock,
    key: p256::ecdsa::SigningKey,
}

fn fake_security(dir: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("security");
    let script = "#!/bin/sh\n[ \"$1\" = find-generic-password ] || exit 2\n\
                  case \"$3\" in nearpush-kiro-api-key) ;; *) echo 'security: item not found' >&2; exit 44 ;; esac\n\
                  if [ \"$4\" = -w ]; then printf '%s\\n' fixture-key-not-real; fi\nexit 0\n";
    std::fs::write(&p, script).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p.display().to_string()
}

fn build_state(dir: &Path, opts: &Opts, mock: &Mock, key: &p256::ecdsa::SigningKey) -> AppState {
    let store = Arc::new(Store::open(dir, 500, 64).unwrap());
    let mut state = AppState::new(store, TOKEN.into(), vec![LOGIN.into()], opts.allow_loopback);
    if let Some((script, key_present)) = opts.classifier {
        let security = if key_present { fake_security(dir) } else { "/usr/bin/false".into() };
        let cfg = ClassifierConfig {
            security_program: security,
            kiro_command: vec!["/bin/sh".into(), "-c".into(), script.into()],
            ..ClassifierConfig::default()
        };
        state = state.with_classifier_hold(cfg, dir.join("classifier"), Duration::from_secs(5)).with_control(
            vec![AgentState::AllSet, AgentState::YourTurn, AgentState::Allow],
            Provider::Kiro,
            Control::default(),
            dir.to_path_buf(),
        );
    }
    if opts.webpush {
        let mut policy = EndpointPolicy::new(&opts.cfg.endpoint_hosts);
        if opts.loopback_policy {
            policy = policy.with_loopback_http(mock.port);
        }
        let wp = WebPush::start(opts.cfg.clone(), key.clone(), dir, state.control.clone(), policy, opts.writer.clone()).unwrap();
        state = state.with_webpush(wp);
    }
    state
}

async fn serve(state: AppState) -> (SocketAddr, tokio::task::JoinHandle<io::Result<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (addr, tokio::spawn(server::serve(listener, state)))
}

impl Fx {
    async fn start(opts: Opts) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let mock = Mock::start().await;
        let key = webpush::generate_signing_key().unwrap();
        let state = build_state(dir.path(), &opts, &mock, &key);
        let (addr, task) = serve(state.clone()).await;
        Fx { _dir: dir, addr, task, state, mock, key }
    }

    async fn default() -> Fx {
        Fx::start(Opts::default()).await
    }

    /// Stops the daemon and starts a new one on the same dirs, key and mock.
    async fn restart(&mut self, opts: Opts) {
        self.task.abort();
        let _ = (&mut self.task).await;
        self.state.webpush = None;
        let state = build_state(self._dir.path(), &opts, &self.mock, &self.key);
        let (addr, task) = serve(state.clone()).await;
        (self.addr, self.task, self.state) = (addr, task, state);
    }

    fn dir(&self) -> &Path {
        self._dir.path()
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn wp(&self) -> Arc<WebPush> {
        self.state.webpush.clone().unwrap()
    }

    /// A same-origin page request from the tailnet viewer.
    fn page(&self, path: &str) -> reqwest::RequestBuilder {
        self.page_typed(path, "application/json")
    }

    fn page_typed(&self, path: &str, content_type: &str) -> reqwest::RequestBuilder {
        client()
            .post(self.url(path))
            .header("tailscale-user-login", LOGIN)
            .header("origin", format!("http://{}", self.addr))
            .header("content-type", content_type)
    }

    async fn subscribe_as(&self, ua: &Ua, name: &str, label: &str) -> (u16, Value) {
        let body = json!({
            "subscription": { "endpoint": self.mock.endpoint(name), "expirationTime": null,
                              "keys": { "p256dh": ua.p256dh(), "auth": ua.auth_b64() } },
            "label": label,
        });
        let r = self.page("/webpush/subscribe").json(&body).send().await.unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }

    async fn subscribe(&self, ua: &Ua, name: &str) -> String {
        let (code, body) = self.subscribe_as(ua, name, "phone").await;
        assert_eq!(code, 200, "{body}");
        body["id"].as_str().unwrap().to_string()
    }

    async fn push(&self, body: Value) -> reqwest::Response {
        client().post(self.url("/push")).bearer_auth(TOKEN).json(&body).send().await.unwrap()
    }

    async fn control(&self, body: Value) {
        let r = client().post(self.url("/control")).bearer_auth(TOKEN).json(&body).send().await.unwrap();
        assert_eq!(r.status(), 200);
    }

    async fn local(&self, path: &str, body: Value) -> (u16, Value) {
        let r = client().post(self.url(path)).bearer_auth(TOKEN).json(&body).send().await.unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }

    async fn status(&self) -> Value {
        client().get(self.url("/status")).bearer_auth(TOKEN).send().await.unwrap().json().await.unwrap()
    }

    fn webpush_file(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.dir().join(webpush::WEBPUSH_FILE)).unwrap()).unwrap()
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

async fn wait_for(mut cond: impl FnMut() -> bool, secs: u64, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn your_turn(session: &str) -> Value {
    json!({ "title": "your turn.", "state": "your_turn", "session": session, "detail": "Which one?" })
}

/// A failing-on-demand writer.
fn faulty_writer() -> (WriteAtomic, Arc<AtomicBool>) {
    let fail = Arc::new(AtomicBool::new(false));
    let f = fail.clone();
    let w: WriteAtomic = Arc::new(move |p: &Path, b: &[u8]| {
        if f.load(Ordering::SeqCst) {
            return Err(io::Error::other("injected fault"));
        }
        webpush::write_atomic_file(p, b)
    });
    (w, fail)
}

// ---- routes ---------------------------------------------------------------------

#[tokio::test]
async fn subscribe_auth_and_validation_matrix() {
    let fx = Fx::default().await;
    let ua = Ua::new();
    let body = json!({ "subscription": { "endpoint": fx.mock.endpoint("a"), "keys": { "p256dh": ua.p256dh(), "auth": ua.auth_b64() } } });
    let url = fx.url("/webpush/subscribe");
    let origin = format!("http://{}", fx.addr);

    let r = client().post(&url).header("origin", &origin).json(&body).send().await.unwrap();
    assert_eq!(r.status(), 401, "no login");
    let r = client().post(&url).header("tailscale-user-login", "mallory@example.com").header("origin", &origin).json(&body).send().await.unwrap();
    assert_eq!(r.status(), 403, "wrong login");
    let r = client().post(&url).header("tailscale-user-login", LOGIN).json(&body).send().await.unwrap();
    assert_eq!(r.status(), 403, "missing Origin");
    assert_eq!(r.json::<Value>().await.unwrap()["error"], "missing Origin");
    let r = client().post(&url).header("tailscale-user-login", LOGIN).header("origin", "https://evil.example").json(&body).send().await.unwrap();
    assert_eq!(r.status(), 403, "cross Origin");
    let r = fx.page_typed("/webpush/subscribe", "text/plain").body(body.to_string()).send().await.unwrap();
    assert_eq!(r.status(), 415);

    let mut bad = body.clone();
    bad["subscription"]["endpoint"] = json!("https://evil.example/push");
    let r = fx.page("/webpush/subscribe").json(&bad).send().await.unwrap();
    assert_eq!(r.status(), 400);
    let text = r.text().await.unwrap();
    assert!(text.contains("endpoint not allowed") && !text.contains("evil"), "{text}");
    let mut extra = body.clone();
    extra["bogus"] = json!(1);
    assert_eq!(fx.page("/webpush/subscribe").json(&extra).send().await.unwrap().status(), 400, "unknown top-level field");
    let mut inner = body.clone();
    inner["subscription"]["somethingNew"] = json!(1);
    assert_eq!(fx.page("/webpush/subscribe").json(&inner).send().await.unwrap().status(), 200, "unknown inner field ignored");
    let big = json!({ "subscription": body["subscription"], "label": "x".repeat(5000) });
    assert_eq!(fx.page("/webpush/subscribe").json(&big).send().await.unwrap().status(), 413);

    let (code, first) = fx.subscribe_as(&ua, "a", "phone").await;
    assert_eq!((code, &first["created"]), (200, &json!(false)), "same endpoint upserts: {first}");
    for n in 1..8 {
        fx.subscribe(&Ua::new(), &format!("n{n}")).await;
    }
    let (code, body9) = fx.subscribe_as(&Ua::new(), "ninth", "x").await;
    assert_eq!((code, body9["error"].as_str()), (409, Some("too many subscriptions")));
    let st = fx.status().await;
    assert_eq!(st["webpush"]["subscriptions"].as_array().unwrap().len(), 8);
    assert_eq!(st["webpush"]["subscriptions"][0]["login"], LOGIN);
}

#[tokio::test]
async fn unsubscribe_and_remove_routes() {
    let fx = Fx::default().await;
    let id = fx.subscribe(&Ua::new(), "a").await;
    let ep = fx.mock.endpoint("a");
    let url = fx.url("/webpush/unsubscribe");

    let r = client().post(&url).header("tailscale-user-login", LOGIN).json(&json!({"endpoint": ep})).send().await.unwrap();
    assert_eq!((r.status().as_u16(), r.json::<Value>().await.unwrap()["error"].clone()), (403, json!("missing Origin")));
    let r = client().post(&url).header("tailscale-user-login", LOGIN).header("origin", "https://evil.example").json(&json!({"endpoint": ep})).send().await.unwrap();
    assert_eq!(r.status(), 403);
    let r = fx.page_typed("/webpush/unsubscribe", "text/plain").body(json!({"endpoint": ep}).to_string()).send().await.unwrap();
    assert_eq!(r.status(), 415);
    let r = client().post(&url).header("origin", format!("http://{}", fx.addr)).json(&json!({"endpoint": ep})).send().await.unwrap();
    assert_eq!(r.status(), 401, "no login");

    let r = fx.page("/webpush/unsubscribe").json(&json!({"endpoint": "http://127.0.0.1:1/none"})).send().await.unwrap();
    assert_eq!((r.status().as_u16(), r.json::<Value>().await.unwrap()), (200, json!({"removed": false})));
    let r = fx.page("/webpush/unsubscribe").json(&json!({"endpoint": ep})).send().await.unwrap();
    assert_eq!((r.status().as_u16(), r.json::<Value>().await.unwrap()), (200, json!({"removed": true})));
    assert_eq!(fx.webpush_file()["subscriptions"], json!([]));

    // remove: local caller only.
    let id2 = fx.subscribe(&Ua::new(), "b").await;
    assert_ne!(id2, id, "ids are never reused");
    let r = fx.page("/webpush/remove").json(&json!({"id": id2})).send().await.unwrap();
    assert_eq!(r.status(), 403, "a tailnet viewer gets the local-auth rejection");
    assert_eq!(fx.local("/webpush/remove", json!({"id": "XYZ"})).await.0, 400);
    assert_eq!(fx.local("/webpush/remove", json!({"id": id2, "x": 1})).await.0, 400);
    assert_eq!(fx.local("/webpush/remove", json!({"id": "0123456789abcdef"})).await, (404, json!({"error": "no such subscription"})));
    assert_eq!(fx.local("/webpush/remove", json!({"id": id2})).await, (200, json!({"removed": true})));
    assert!(fx.status().await["webpush"]["subscriptions"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn disabled_routes_answer_409_and_key_says_disabled() {
    let fx = Fx::start(Opts { webpush: false, ..Opts::default() }).await;
    let key: Value = client().get(fx.url("/webpush/key")).header("tailscale-user-login", LOGIN).send().await.unwrap().json().await.unwrap();
    assert_eq!(key, json!({"enabled": false, "key": null, "key_id": null}));
    let (code, _) = fx.subscribe_as(&Ua::new(), "a", "x").await;
    assert_eq!(code, 409);
    let r = fx.page("/webpush/unsubscribe").json(&json!({"endpoint": "x"})).send().await.unwrap();
    assert_eq!(r.status(), 409);
    assert_eq!(fx.local("/webpush/test", json!({})).await, (409, json!({"error": "web push is disabled"})));
    assert_eq!(fx.local("/webpush/remove", json!({"id": "0123456789abcdef"})).await.0, 409);
    assert_eq!(fx.status().await["webpush"]["enabled"], false);
    let r = client().get(fx.url("/sw.js")).header("tailscale-user-login", LOGIN).send().await.unwrap();
    assert_eq!(r.status(), 200, "the worker is served while disabled");
}

#[tokio::test]
async fn sw_js_headers_and_csp() {
    let fx = Fx::default().await;
    let r = client().get(fx.url("/sw.js")).header("tailscale-user-login", LOGIN).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let h = r.headers();
    assert_eq!(h["content-type"], "text/javascript");
    assert_eq!(h["cache-control"], "no-cache");
    let csp = h["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("img-src 'self'") && csp.contains("worker-src 'self'"), "{csp}");
    assert!(r.text().await.unwrap().contains("showNotification"));
    let r = client().get(fx.url("/sw.js")).send().await.unwrap();
    assert_eq!(r.status(), 401, "viewer auth");
    let page = client().get(fx.url("/")).header("tailscale-user-login", LOGIN).send().await.unwrap();
    assert!(page.headers()["content-security-policy"].to_str().unwrap().contains("worker-src 'self'"));
}

// ---- sending --------------------------------------------------------------------

#[tokio::test]
async fn direct_push_reaches_the_mock_encrypted_and_signed() {
    let fx = Fx::default().await;
    let ua = Ua::new();
    let id = fx.subscribe(&ua, "a").await;
    let key: Value = client().get(fx.url("/webpush/key")).header("tailscale-user-login", LOGIN).send().await.unwrap().json().await.unwrap();
    assert_eq!(key["enabled"], true);
    assert_eq!(fx.push(your_turn("s1")).await.status(), 200);
    wait_for(|| fx.mock.seen("a").len() == 1, 5, "push").await;
    let seen = &fx.mock.seen("a")[0];
    let v = ua.decrypt(&seen.body).expect("decrypts with the subscription keys");
    assert_eq!(v["title"], "✋ your turn");
    assert_eq!(v["state"], "your_turn");
    assert_eq!(v["body"], "", "default content is state");
    assert_eq!(v["url"], "/");
    assert!(v["tag"].as_str().unwrap().starts_with("np-"));
    assert_eq!(seen.headers["ttl"], "300");
    assert_eq!(seen.headers["urgency"], "high");
    assert_eq!(seen.headers["content-encoding"], "aes128gcm");
    // vapid t=<jwt>, k=<key>
    let auth = seen.headers["authorization"].to_str().unwrap().to_string();
    let rest = auth.strip_prefix("vapid t=").unwrap();
    let (jwt, k) = rest.split_once(", k=").unwrap();
    assert_eq!(k, key["key"].as_str().unwrap());
    let parts: Vec<&str> = jwt.split('.').collect();
    let claims: Value = serde_json::from_slice(&Base64UrlUnpadded::decode_vec(parts[1]).unwrap()).unwrap();
    assert_eq!(claims["aud"], format!("http://127.0.0.1:{}", fx.mock.port), "aud includes the port");
    assert_eq!(claims["sub"], "mailto:you@example.com");
    let exp = claims["exp"].as_u64().unwrap();
    let now = now_ms() / 1000;
    assert!(exp > now + 11 * 3600 && exp <= now + 12 * 3600 + 5, "{exp}");
    let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(&Base64UrlUnpadded::decode_vec(k).unwrap()).unwrap();
    let sig = p256::ecdsa::Signature::from_slice(&Base64UrlUnpadded::decode_vec(parts[2]).unwrap()).unwrap();
    vk.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).unwrap();

    wait_for(|| fx.wp().view().sent == 1, 5, "sent counter").await;
    let st = fx.status().await;
    let sub = &st["webpush"]["subscriptions"][0];
    assert_eq!(sub["id"], id.as_str());
    assert_eq!(sub["endpoint_host"], "127.0.0.1");
    let text = st.to_string();
    assert!(!text.contains("/push/a") && !text.contains(&ua.p256dh()) && !text.contains(&ua.auth_b64()), "{text}");

    // 410: the subscription is removed from webpush.json.
    fx.mock.script("a", &[Answer::Status(410)]);
    fx.push(your_turn("s2")).await;
    wait_for(|| fx.webpush_file()["subscriptions"] == json!([]), 5, "removal saved").await;
    assert!(fx.wp().view().subscriptions.is_empty());
}

#[tokio::test]
async fn states_that_never_push_and_pause_and_filter() {
    let fx = Fx::default().await;
    fx.subscribe(&Ua::new(), "a").await;
    fx.push(json!({"title": "working.", "state": "working", "session": "s1"})).await;
    fx.push(json!({"title": "generic"})).await;
    fx.control(json!({"states": ["your_turn"]})).await;
    fx.push(json!({"title": "all set.", "state": "all_set", "session": "s2"})).await;
    fx.control(json!({"paused": true})).await;
    fx.push(your_turn("s3")).await;
    fx.control(json!({"paused": false})).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(fx.mock.total(), 0, "working, generic, filtered and paused events never push");
    fx.push(your_turn("s4")).await;
    wait_for(|| fx.mock.seen("a").len() == 1, 5, "a pushable event after unpause").await;
}

#[tokio::test]
async fn control_epoch_bump_rules() {
    let fx = Fx::default().await;
    let epoch = || fx.state.control.read().unwrap().epoch;
    assert_eq!(epoch(), 0);
    fx.control(json!({"paused": false})).await;
    assert_eq!(epoch(), 0, "paused unchanged");
    fx.control(json!({"paused": true})).await;
    assert_eq!(epoch(), 1);
    fx.control(json!({"states": ["allow", "all_set", "your_turn"]})).await;
    assert_eq!(epoch(), 1, "same set in another order");
    fx.control(json!({"states": ["your_turn", "allow", "all_set", "allow"]})).await;
    assert_eq!(epoch(), 1, "same set with a duplicate");
    fx.control(json!({"states": ["your_turn"]})).await;
    assert_eq!(epoch(), 2);
}

#[tokio::test]
async fn a_slow_endpoint_does_not_delay_another() {
    let fx = Fx::default().await;
    fx.subscribe(&Ua::new(), "slow").await;
    fx.subscribe(&Ua::new(), "fast").await;
    fx.mock.script("slow", &[Answer::Hold(Duration::from_secs(30), 201), Answer::Hold(Duration::from_secs(30), 201)]);
    let t0 = Instant::now();
    fx.push(your_turn("s1")).await;
    wait_for(|| fx.mock.seen("fast").len() == 1, 5, "fast push").await;
    assert!(fx.mock.seen("fast")[0].at - t0 < Duration::from_secs(2));
    tokio::time::sleep(Duration::from_secs(1)).await;
    let t1 = Instant::now();
    fx.push(your_turn("s2")).await;
    wait_for(|| fx.mock.seen("fast").len() == 2, 5, "second fast push").await;
    assert!(fx.mock.seen("fast")[1].at - t1 < Duration::from_secs(2));
}

#[tokio::test]
async fn pause_during_the_retry_cancels_it_even_when_undone() {
    for (first, second) in [
        (json!({"paused": true}), None),
        (json!({"paused": true}), Some(json!({"paused": false}))),
        (json!({"states": ["all_set"]}), None),
        (json!({"states": ["all_set"]}), Some(json!({"states": ["all_set", "your_turn", "allow"]}))),
    ] {
        let fx = Fx::default().await;
        fx.subscribe(&Ua::new(), "a").await;
        fx.mock.script("a", &[Answer::Status(503)]);
        fx.push(your_turn("s1")).await;
        wait_for(|| fx.mock.seen("a").len() == 1, 5, "first attempt").await;
        fx.control(first.clone()).await;
        if let Some(s) = &second {
            fx.control(s.clone()).await;
        }
        tokio::time::sleep(Duration::from_millis(2600)).await;
        assert_eq!(fx.mock.seen("a").len(), 1, "no retry after {first} then {second:?}");
        assert_eq!(fx.wp().view().dropped_paused, 1);
        if second.is_some() {
            fx.push(your_turn("s2")).await;
            wait_for(|| fx.mock.seen("a").len() == 2, 5, "a new event after the undo").await;
        }
    }
}

#[tokio::test]
async fn a_job_held_behind_a_send_is_cancelled_by_pause_and_unpause() {
    let fx = Fx::default().await;
    fx.subscribe(&Ua::new(), "a").await;
    fx.mock.script("a", &[Answer::Hold(Duration::from_secs(3), 201)]);
    fx.push(your_turn("s1")).await;
    wait_for(|| fx.mock.seen("a").len() == 1, 5, "first push").await;
    fx.push(your_turn("s2")).await;
    tokio::time::sleep(Duration::from_millis(800)).await; // the dispatcher hands it to the lane
    fx.control(json!({"paused": true})).await;
    fx.control(json!({"paused": false})).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(fx.mock.seen("a").len(), 1, "the held job was never sent");
}

#[tokio::test]
async fn resubscribe_with_new_keys_and_the_old_send_410_race() {
    let fx = Fx::default().await;
    let (k1, k2) = (Ua::new(), Ua::new());
    let id = fx.subscribe(&k1, "e").await;
    fx.push(your_turn("s1")).await;
    wait_for(|| fx.mock.seen("e").len() == 1, 5, "k1 push").await;
    assert!(k1.decrypt(&fx.mock.seen("e")[0].body).is_some());

    let (code, body) = fx.subscribe_as(&k2, "e", "new label").await;
    assert_eq!((code, body["id"].as_str(), body["created"].as_bool()), (200, Some(id.as_str()), Some(false)));
    fx.push(your_turn("s2")).await;
    wait_for(|| fx.mock.seen("e").len() == 2, 5, "k2 push").await;
    let b = &fx.mock.seen("e")[1].body;
    assert!(k2.decrypt(b).is_some() && web_push_native::decrypt(b.clone(), &k1.secret, &web_push_native::Auth::clone_from_slice(&k1.auth)).is_err());

    // Race: the k2 send is held open, the test re-subscribes with k3, then the mock answers 410.
    let k3 = Ua::new();
    fx.mock.script("e", &[Answer::Hold(Duration::from_secs(2), 410)]);
    fx.push(your_turn("s3")).await;
    wait_for(|| fx.mock.seen("e").len() == 3, 5, "held push").await;
    fx.subscribe_as(&k3, "e", "k3").await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(fx.wp().view().subscriptions.len(), 1, "the old send's 410 did not remove the newer keys");
    fx.push(your_turn("s4")).await;
    wait_for(|| fx.mock.seen("e").len() == 4, 5, "k3 push").await;
    assert!(k3.decrypt(&fx.mock.seen("e")[3].body).is_some());
}

#[tokio::test]
async fn unsubscribe_during_a_send() {
    let fx = Fx::default().await;
    fx.subscribe(&Ua::new(), "a").await;
    fx.mock.script("a", &[Answer::Hold(Duration::from_secs(2), 201)]);
    fx.push(your_turn("s1")).await;
    wait_for(|| fx.mock.seen("a").len() == 1, 5, "held push").await;
    let r = fx.page("/webpush/unsubscribe").json(&json!({"endpoint": fx.mock.endpoint("a")})).send().await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap(), json!({"removed": true}));
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(fx.webpush_file()["subscriptions"], json!([]));
    fx.push(your_turn("s2")).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(fx.mock.seen("a").len(), 1);
}

#[tokio::test]
async fn webpush_test_route_reports_per_subscription() {
    let fx = Fx::default().await;
    assert_eq!(fx.local("/webpush/test", json!({})).await, (200, json!({"results": []})));
    let ok = fx.subscribe(&Ua::new(), "ok").await;
    let gone = fx.subscribe(&Ua::new(), "gone").await;
    fx.mock.script("gone", &[Answer::Status(410)]);
    // A coalescible burst first; the test does not pass through the dispatcher.
    fx.control(json!({"paused": true})).await;
    for n in 0..3 {
        fx.push(your_turn(&format!("s{n}"))).await;
    }
    let (code, body) = fx.local("/webpush/test", json!({})).await;
    assert_eq!(code, 200);
    assert_eq!(
        body["results"],
        json!([
            {"id": ok, "label": "phone", "status": 201, "class": "ok"},
            {"id": gone, "label": "phone", "status": 410, "class": "gone"},
        ]),
        "a test while paused still reaches the mock"
    );
    assert!(!body.to_string().contains("127.0.0.1"));
    assert_eq!(fx.wp().view().subscriptions.len(), 1, "the 410 one was removed");
    let page = fx.page("/webpush/test").send().await.unwrap();
    assert_eq!(page.status(), 403, "local only");
}

#[tokio::test]
async fn webpush_test_lane_full_and_rate_limit_hold() {
    let fx = Fx::default().await;
    let busy = fx.subscribe(&Ua::new(), "busy").await;
    let free = fx.subscribe(&Ua::new(), "free").await;
    fx.mock.script("busy", &[Answer::Hold(Duration::from_secs(4), 201)]);
    fx.mock.script("free", &[Answer::RetryAfter(429, "30")]);
    // Fill the busy lane: one send held open, four jobs waiting.
    let wp = fx.wp();
    let first = tokio::spawn({
        let wp = wp.clone();
        async move { wp.test_all().await }
    });
    wait_for(|| fx.mock.seen("busy").len() == 1, 5, "busy send open").await;
    for _ in 0..4 {
        let wp = wp.clone();
        tokio::spawn(async move { wp.test_all().await });
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let res = wp.test_all().await;
    let class = |id: &str| res.iter().find(|e| e.id == id).unwrap().class;
    assert_eq!(class(&busy), "lane_full");
    assert_eq!(class(&free), "rate_limited", "the free lane is in its Retry-After hold");
    let first = first.await.unwrap();
    assert_eq!(first.iter().find(|e| e.id == free).unwrap().status, Some(429));
}

#[tokio::test]
async fn endpoint_policy_is_one_for_route_and_lane() {
    let fx = Fx::start(Opts { loopback_policy: false, ..Opts::default() }).await;
    let (code, _) = fx.subscribe_as(&Ua::new(), "a", "x").await;
    assert_eq!(code, 400, "the production policy rejects a loopback endpoint");

    // A record hand-edited into webpush.json with a host outside the policy.
    let ua = Ua::new();
    let file = json!({"v": 1, "key_id": fx.wp().key_id(), "subscriptions": [{
        "id": "00000000000000aa", "endpoint": fx.mock.endpoint("edited"), "p256dh": ua.p256dh(), "auth": ua.auth_b64(),
        "login": LOGIN, "label": "edited", "created_ms": 1, "updated_ms": 1 }]});
    std::fs::write(fx.dir().join(webpush::WEBPUSH_FILE), file.to_string()).unwrap();
    let mut fx = fx;
    fx.restart(Opts { loopback_policy: false, ..Opts::default() }).await;
    let (_, body) = fx.local("/webpush/test", json!({})).await;
    assert_eq!(body["results"][0]["class"], "rejected");
    fx.push(your_turn("s1")).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(fx.mock.total(), 0, "never posted to");
    assert_eq!(fx.wp().view().subscriptions.len(), 1, "record kept");
}

#[tokio::test]
async fn log_hygiene_with_a_hostile_push_service() {
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let b = buf.clone();
    let sub = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("nearpushd=trace"))
        .with_ansi(false)
        .with_writer(move || Writer(b.clone()))
        .finish();
    // Global, so lanes and every callsite see it; other tests' lines land here too,
    // which is harmless because the markers below are unique to this test.
    tracing::subscriber::set_global_default(sub).unwrap();

    let fx = Fx::default().await;
    fx.subscribe(&Ua::new(), "h400").await;
    fx.subscribe(&Ua::new(), "h500").await;
    const MARK: &str = "HOSTILE-MARKER";
    fx.mock.script("h400", &[Answer::Echo(400, MARK), Answer::Echo(400, MARK)]);
    fx.mock.script("h500", &[Answer::Echo(500, MARK), Answer::Echo(500, MARK), Answer::Echo(500, MARK), Answer::Echo(500, MARK)]);
    fx.push(your_turn("s1")).await;
    wait_for(|| fx.mock.seen("h500").len() == 2, 10, "send and retry").await;
    let (_, test) = fx.local("/webpush/test", json!({})).await;
    let status = fx.status().await;
    let jwt = fx.mock.seen("h400")[0].headers["authorization"].to_str().unwrap().to_string();
    let jwt = jwt.strip_prefix("vapid t=").unwrap().split(',').next().unwrap().to_string();
    let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("web push failed"), "the failure was logged: {logs}");
    for (what, text) in [("logs", &logs), ("status", &status.to_string()), ("test", &test.to_string())] {
        for secret in [MARK, "/push/h400", "/push/h500", &jwt] {
            assert!(!text.contains(secret), "{what} contains {secret}");
        }
    }
}

struct Writer(Arc<Mutex<Vec<u8>>>);

impl io::Write for Writer {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ---- store paths ----------------------------------------------------------------

#[tokio::test]
async fn classifier_deferred_stop_pushes_once_with_the_post_verdict_state() {
    for (key_present, want_state, want_title) in [(true, "all_set", "✅ all set"), (false, "your_turn", "✋ your turn")] {
        let fx = Fx::start(Opts { classifier: Some(("cat >/dev/null; echo NO", key_present)), ..Opts::default() }).await;
        let ua = Ua::new();
        fx.subscribe(&ua, "a").await;
        let r = fx
            .push(json!({
                "title": "your turn.", "state": "your_turn", "session": "s1", "detail": "rules line",
                "classify": { "reply_tail": "Two options.\nWhich one should I pick?", "question_line": "rules line" }
            }))
            .await;
        assert_eq!(r.status(), 202);
        assert_eq!(r.json::<Value>().await.unwrap()["deferred"], true);
        wait_for(|| fx.mock.seen("a").len() == 1, 10, "deferred push").await;
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(fx.mock.seen("a").len(), 1, "exactly one push");
        let v = ua.decrypt(&fx.mock.seen("a")[0].body).unwrap();
        assert_eq!((v["state"].as_str(), v["title"].as_str()), (Some(want_state), Some(want_title)), "key present: {key_present}");
    }
}

/// A fake relay that serves `events` once, then empty pages.
async fn fake_relay(events: Value) -> String {
    let once = Arc::new(Mutex::new(Some(events)));
    let app = Router::new()
        .route(
            "/v1/events",
            get(move || {
                let once = once.clone();
                async move {
                    let page = once.lock().unwrap().take();
                    if page.is_none() {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                    Json(json!({ "events": page.unwrap_or(json!([])), "last_id": 0 }))
                }
            }),
        )
        .route("/v1/ack", post(|| async { StatusCode::NO_CONTENT }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn relay_events_push_only_when_fresh_for_both_body_shapes() {
    let push_body = json!({"title": "all set.", "state": "all_set", "session": "s1"});
    let kiro_body = json!({"event": "turn_end", "session_id": "k1", "cwd": "/w/demo", "reply_tail": "All done.",
                           "timestamp": "2026-01-01T00:00:00Z"});
    for (shape, body) in [("push", push_body), ("kiro", kiro_body)] {
        for (age_ms, pushed) in [(3_600_000u64, false), (10_000, true)] {
            let fx = Fx::default().await;
            let ua = Ua::new();
            fx.subscribe(&ua, "a").await;
            let url = fake_relay(json!([{ "id": 1, "source": "host-a", "received_ms": now_ms() - age_ms, "body": body }])).await;
            let cfg = RelayConfig { enabled: true, url: Some(url), wait_secs: 1, ..RelayConfig::default() };
            let task = tokio::spawn(nearpushd::relay::run(fx.state.clone(), cfg, "t".into(), fx.dir().join("relay-cursor.json")));
            wait_for(|| fx.state.store.since(0).iter().any(|e| e.relay_id == Some(1)), 5, "relay card stored").await;
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let n = fx.mock.seen("a").len();
            assert_eq!(n, usize::from(pushed), "{shape} body, age {age_ms} ms");
            if pushed {
                assert_eq!(ua.decrypt(&fx.mock.seen("a")[0].body).unwrap()["title"], "✅ all set");
            } else {
                assert_eq!(fx.wp().view().dropped_stale, 0, "stale relay events never reach the queue");
            }
            task.abort();
        }
    }
}

// ---- save failures --------------------------------------------------------------

#[tokio::test]
async fn save_failures_keep_the_change_and_report_500() {
    let (writer, fail) = faulty_writer();
    let mut fx = Fx::start(Opts { writer: writer.clone(), ..Opts::default() }).await;
    let keep = fx.subscribe(&Ua::new(), "keep").await;
    let gone = fx.subscribe(&Ua::new(), "gone").await;
    fail.store(true, Ordering::SeqCst);

    let (code, body) = fx.subscribe_as(&Ua::new(), "new", "x").await;
    assert_eq!((code, body.clone()), (500, json!({"error": "cannot save webpush.json", "applied": true})));
    let r = fx.page("/webpush/unsubscribe").json(&json!({"endpoint": fx.mock.endpoint("keep")})).send().await.unwrap();
    assert_eq!((r.status().as_u16(), r.json::<Value>().await.unwrap()), (500, body.clone()));
    assert_eq!(fx.local("/webpush/remove", json!({"id": gone})).await, (500, body.clone()));
    let st = fx.status().await;
    assert_eq!(st["webpush"]["dirty"], true);
    let labels: Vec<&str> = st["webpush"]["subscriptions"].as_array().unwrap().iter().map(|s| s["label"].as_str().unwrap()).collect();
    assert_eq!(labels, ["x"], "the in-memory set reflects all three changes");
    // Nothing to change: no save attempted, so no 500.
    let r = fx.page("/webpush/unsubscribe").json(&json!({"endpoint": "http://127.0.0.1:1/none"})).send().await.unwrap();
    assert_eq!((r.status().as_u16(), r.json::<Value>().await.unwrap()), (200, json!({"removed": false})));
    assert_eq!(fx.local("/webpush/remove", json!({"id": "0123456789abcdef"})).await.0, 404);

    // Restart before the fault clears: all three changes are lost.
    fx.restart(Opts { writer: writer.clone(), ..Opts::default() }).await;
    let ids: Vec<String> = fx.wp().view().subscriptions.into_iter().map(|s| s.id).collect();
    assert_eq!(ids, [keep.clone(), gone.clone()], "the unsubscribed and removed records are back, the new one is absent");
    // The resurrected unsubscribed record is removed by its next 410.
    fail.store(false, Ordering::SeqCst);
    fx.mock.script("keep", &[Answer::Status(410)]);
    fx.push(your_turn("s1")).await;
    wait_for(|| fx.wp().view().subscriptions.len() == 1, 5, "410 removal").await;
    // A repeated remove after the fault clears removes for good.
    assert_eq!(fx.local("/webpush/remove", json!({"id": gone})).await.0, 200);
    fx.restart(Opts { writer: writer.clone(), ..Opts::default() }).await;
    assert!(fx.wp().view().subscriptions.is_empty());

    // The flush task writes within 30 s once the fault clears.
    fail.store(true, Ordering::SeqCst);
    assert_eq!(fx.subscribe_as(&Ua::new(), "late", "late").await.0, 500);
    fail.store(false, Ordering::SeqCst);
    let wp = fx.wp();
    wait_for(|| !wp.is_dirty(), 35, "flush").await;
    assert_eq!(fx.webpush_file()["subscriptions"][0]["label"], "late");
}
