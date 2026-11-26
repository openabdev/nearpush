//! Web Push sender: VAPID key, subscription store, payload, dispatcher and one lane per
//! subscription. Design of record: `.agents/tasks/apple-watch-webpush/design.md`.
//!
//! `server::commit()` calls [`WebPush::notify`] for every stored event; `notify` only
//! does a `try_send`. A dispatcher coalesces bursts and fans each job out to one lane
//! per subscription, so a slow endpoint never delays another.
//!
//! Lock order (see the design's "Lock order"): `save_lock` is only taken with no
//! `SubSet` guard held, and is the only lock held across an `.await`. A `SubSet` guard
//! lives only inside small synchronous functions over `&mut SubSet`; `ControlState` is
//! read, copied out and released before `SubSet` is taken.
//!
//! Logs never contain the endpoint path, the keys, the JWT, the payload or any part of
//! a push service response.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, Weak};
use std::time::Duration;

use base64ct::{Base64UrlUnpadded, Encoding};
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use web_push_native::{Auth, WebPushBuilder};

use crate::config::{PushContent, WebPushConfig};
use crate::event::{AgentState, Event};
use crate::server::ControlState;
use crate::store::{ensure_private_dir, now_ms};

/// VAPID private key file, in the config dir.
pub const VAPID_FILE: &str = "webpush-vapid.pem";
/// Subscriptions file, in the state dir.
pub const WEBPUSH_FILE: &str = "webpush.json";
pub const MAX_SUBSCRIPTIONS: usize = 8;
/// Every payload is padded to exactly this many bytes.
pub const PAYLOAD_BYTES: usize = 1024;
/// Largest payload JSON before padding.
pub const MAX_CONTENT_BYTES: usize = 960;
const QUEUE_SLOTS: usize = 64;
const LANE_SLOTS: usize = 4;
const COALESCE_WINDOW: Duration = Duration::from_millis(500);
const RATE_MAX: usize = 20;
const RATE_WINDOW_MS: u64 = 60_000;
const RETRY_DELAY: Duration = Duration::from_secs(2);
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const TEST_WAIT: Duration = Duration::from_secs(25);
const FLUSH_EVERY: Duration = Duration::from_secs(30);
const JWT_LIFETIME_SECS: u64 = 12 * 3600;
const JWT_RENEW_SECS: u64 = 3600;
/// A send result rewrites the file only when `last_ok_ms` moved more than this.
const OK_PERSIST_MS: u64 = 10 * 60 * 1000;
const RETRY_AFTER_DEFAULT_SECS: u64 = 60;
const RETRY_AFTER_MAX_SECS: u64 = 600;
const LOG_EVERY_MS: u64 = 60_000;
const SUMMARY_MAX_CHARS: usize = 120;
const DETAIL_MAX_CHARS: usize = 160;

/// The notification title of each state that can push. `sw.js` shows it as is.
pub const TITLES: &[(AgentState, &str)] = &[
    (AgentState::YourTurn, "✋ your turn"),
    (AgentState::AllSet, "✅ all set"),
    (AgentState::Allow, "❓ allow?"),
];
pub const TEST_TITLE: &str = "nearpush test";

pub fn title(state: AgentState) -> &'static str {
    TITLES.iter().find(|(s, _)| *s == state).map_or("nearpush", |(_, t)| *t)
}

fn state_name(s: AgentState) -> &'static str {
    match s {
        AgentState::Allow => "allow",
        AgentState::YourTurn => "your_turn",
        AgentState::AllSet => "all_set",
        AgentState::Working => "working",
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    Base64UrlUnpadded::decode_vec(s.trim().trim_end_matches('=')).ok()
}

// ---- VAPID key --------------------------------------------------------------------

/// A signing key from 32 random bytes; an invalid scalar (zero or ≥ n) is retried.
fn key_from_random(mut fill: impl FnMut(&mut [u8; 32]) -> io::Result<()>) -> io::Result<SigningKey> {
    loop {
        let mut bytes = [0u8; 32];
        fill(&mut bytes)?;
        if let Ok(k) = SigningKey::from_bytes(&p256::FieldBytes::clone_from_slice(&bytes)) {
            return Ok(k);
        }
    }
}

pub fn generate_signing_key() -> io::Result<SigningKey> {
    key_from_random(|b| getrandom::fill(b).map_err(|e| io::Error::other(e.to_string())))
}

fn to_pem(key: &SigningKey) -> io::Result<String> {
    let secret = p256::SecretKey::from_bytes(&key.to_bytes()).map_err(|_| io::Error::other("invalid VAPID key"))?;
    let pem = secret.to_pkcs8_pem(LineEnding::LF).map_err(|_| io::Error::other("cannot encode the VAPID key"))?;
    Ok(pem.to_string())
}

fn from_pem(text: &str) -> Option<SigningKey> {
    p256::SecretKey::from_pkcs8_pem(text).ok().map(|s| SigningKey::from(&s))
}

/// Reads the VAPID key (PKCS#8 PEM, P-256), creating it (0600, parent 0700) when the
/// file is missing. A file readable by group or others is chmod'ed to 0600. The key
/// content is never printed.
pub fn load_or_create_vapid(path: &Path) -> io::Result<SigningKey> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() && !dir.exists() {
            fs::create_dir_all(dir)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
    }
    match fs::read_to_string(path) {
        Ok(text) => {
            let mode = fs::metadata(path)?.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
                tracing::warn!(path = %path.display(), mode = format!("{mode:o}"), "VAPID key file was readable by others; set it to 0600");
            }
            from_pem(&text).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, format!("{}: not a PKCS#8 PEM P-256 private key", path.display()))
            })
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let key = generate_signing_key()?;
            let pem = to_pem(&key)?;
            let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
            f.write_all(pem.as_bytes())?;
            f.sync_all()?;
            tracing::info!(path = %path.display(), "created VAPID key");
            Ok(key)
        }
        Err(e) => Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
    }
}

/// The VAPID signing key with its public half and key id.
pub struct Vapid {
    key: SigningKey,
    public: Vec<u8>,
    key_id: String,
}

impl Vapid {
    pub fn new(key: SigningKey) -> Vapid {
        let public = key.verifying_key().to_encoded_point(false).as_bytes().to_vec();
        let key_id = hex(&Sha256::digest(&public))[..16].to_string();
        Vapid { key, public, key_id }
    }

    /// Uncompressed public key, base64url without padding (the page's `applicationServerKey`).
    pub fn public_b64(&self) -> String {
        Base64UrlUnpadded::encode_string(&self.public)
    }

    /// First 16 hex characters of SHA-256 over the uncompressed public key.
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// An ES256 VAPID JWT (RFC 8292) for `aud`, the endpoint origin.
    pub fn jwt(&self, aud: &str, sub: &str, exp: u64) -> String {
        let header = Base64UrlUnpadded::encode_string(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = serde_json::json!({ "aud": aud, "exp": exp, "sub": sub });
        let claims = Base64UrlUnpadded::encode_string(claims.to_string().as_bytes());
        let input = format!("{header}.{claims}");
        let sig: Signature = self.key.sign(input.as_bytes());
        format!("{input}.{}", Base64UrlUnpadded::encode_string(&sig.to_bytes()))
    }

    pub fn verifying_key(&self) -> p256::ecdsa::VerifyingKey {
        *self.key.verifying_key()
    }
}

/// Scheme, host and non-default port, no path: the VAPID `aud`.
pub fn endpoint_origin(url: &Url) -> String {
    let host = url.host_str().unwrap_or("");
    match url.port() {
        Some(p) => format!("{}://{host}:{p}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    }
}

// ---- endpoint policy --------------------------------------------------------------

/// The one endpoint rule, used by the subscribe route and by every lane before a send.
#[derive(Debug, Clone)]
pub struct EndpointPolicy {
    hosts: Vec<String>,
    loopback_http_port: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointRejected;

impl EndpointPolicy {
    /// https only, port 443 only, host exactly in `hosts` (case-insensitive).
    pub fn new(hosts: &[String]) -> EndpointPolicy {
        EndpointPolicy { hosts: hosts.iter().map(|h| h.to_ascii_lowercase()).collect(), loopback_http_port: None }
    }

    /// Also admits `http://127.0.0.1:<port>`, for tests against a local mock push
    /// service. Reachable from Rust code only; config cannot name it.
    pub fn with_loopback_http(mut self, port: u16) -> EndpointPolicy {
        self.loopback_http_port = Some(port);
        self
    }

    pub fn check(&self, url: &Url) -> Result<(), EndpointRejected> {
        if !url.username().is_empty() || url.password().is_some() {
            return Err(EndpointRejected);
        }
        let host = url.host_str().unwrap_or("");
        if let Some(p) = self.loopback_http_port {
            if url.scheme() == "http" && host == "127.0.0.1" && url.port() == Some(p) {
                return Ok(());
            }
        }
        if url.scheme() != "https" || url.port().is_some_and(|p| p != 443) {
            return Err(EndpointRejected);
        }
        if self.hosts.iter().any(|h| h.eq_ignore_ascii_case(host)) { Ok(()) } else { Err(EndpointRejected) }
    }
}

/// A subscription that passed validation; `login` comes from the request, not the body.
#[derive(Debug, Clone)]
pub struct NewSub {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub label: String,
    pub login: String,
}

/// Validates a subscribe body's fields. Errors never contain the submitted endpoint.
pub fn validate_subscription(
    policy: &EndpointPolicy,
    endpoint: &str,
    p256dh: &str,
    auth: &str,
    label: Option<&str>,
) -> Result<NewSub, &'static str> {
    if endpoint.len() > 1024 {
        return Err("endpoint not allowed");
    }
    let url = Url::parse(endpoint).map_err(|_| "endpoint not allowed")?;
    policy.check(&url).map_err(|_| "endpoint not allowed")?;
    let key = b64url_decode(p256dh).filter(|k| k.len() == 65).ok_or("keys.p256dh must be a 65-byte P-256 key")?;
    p256::PublicKey::from_sec1_bytes(&key).map_err(|_| "keys.p256dh must be a 65-byte P-256 key")?;
    let secret = b64url_decode(auth).filter(|a| a.len() == 16).ok_or("keys.auth must be 16 bytes")?;
    let label = match label.map(str::trim) {
        None | Some("") => "device".to_string(),
        Some(l) => {
            if l.chars().count() > 32 || l.chars().any(char::is_control) {
                return Err("label must be at most 32 printable characters");
            }
            l.to_string()
        }
    };
    Ok(NewSub {
        endpoint: endpoint.to_string(),
        p256dh: Base64UrlUnpadded::encode_string(&key),
        auth: Base64UrlUnpadded::encode_string(&secret),
        label,
        login: String::new(),
    })
}

// ---- which events push, payload ---------------------------------------------------

/// Transient values `commit()` passes along; none of them is stored with the event.
#[derive(Debug, Clone, Default)]
pub struct PushCtx {
    /// When the event first reached nearpush: the relay's `received_ms`, else the event `ts`.
    pub origin_ms: u64,
    /// The summarized session label `commit()` looked up, if any.
    pub summary: Option<String>,
    /// The control epoch read together with `paused` at commit.
    pub control_epoch: u64,
    /// State of the session's previous stateful event in the store buffer, if any.
    pub prior_state: Option<AgentState>,
}

/// Whether a stored event is pushed: not muted, has a state that `[webpush] states`
/// and the display filter both list, and is at most `ttl_secs` old. An `all_set`
/// whose session's previous stateful event (`prior`) was also `all_set` is not pushed
/// again; the event itself is still stored and shown.
pub fn should_push(ev: &Event, prior: Option<AgentState>, origin_ms: u64, now_ms: u64, filter: &[AgentState], cfg: &WebPushConfig) -> bool {
    if ev.muted {
        return false;
    }
    let Some(s) = ev.state else { return false };
    if s == AgentState::AllSet && prior == Some(AgentState::AllSet) {
        return false;
    }
    s != AgentState::Working
        && cfg.states.contains(&s)
        && filter.contains(&s)
        && now_ms.saturating_sub(origin_ms) <= cfg.ttl_secs * 1000
}

/// `np-` + 16 hex characters of SHA-256 over the session id, or `np-<state>`.
pub fn notification_tag(ev: &Event) -> String {
    match ev.session.as_deref() {
        Some(s) => format!("np-{}", &hex(&Sha256::digest(s.as_bytes()))[..16]),
        None => format!("np-{}", ev.state.map_or("event", state_name)),
    }
}

/// The page's session tag: `#` + the last four letters and digits of the session.
fn session_tag(session: Option<&str>) -> Option<String> {
    let t: Vec<char> = session?.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let t: String = t[t.len().saturating_sub(4)..].iter().collect();
    (!t.is_empty()).then(|| format!("#{t}"))
}

/// At most `max` characters, the last one an ellipsis when cut.
fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// The notification body for `content`, before the size fit.
fn body_text(ev: &Event, summary: Option<&str>, content: PushContent) -> (String, usize) {
    let tag = || session_tag(ev.session.as_deref());
    let nonempty = |s: Option<&str>| s.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let (who, max) = match content {
        PushContent::State => return (String::new(), 0),
        PushContent::Summary => (nonempty(summary).or_else(tag), SUMMARY_MAX_CHARS),
        PushContent::Label => (nonempty(ev.session_label.as_deref()).or_else(tag), SUMMARY_MAX_CHARS),
        PushContent::Detail => (nonempty(ev.session_label.as_deref()).or_else(tag), DETAIL_MAX_CHARS),
    };
    let mut parts: Vec<String> = who.into_iter().collect();
    parts.extend(nonempty(ev.project.as_deref()));
    if content == PushContent::Detail {
        parts.extend(nonempty(ev.detail.as_deref()));
    }
    (parts.join(" · "), max)
}

#[derive(Serialize)]
struct Payload<'a> {
    v: u8,
    id: u64,
    state: &'a str,
    title: &'a str,
    body: String,
    tag: String,
    url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    p: Option<String>,
}

/// Fits the body so the JSON is at most [`MAX_CONTENT_BYTES`], then pads it with a
/// `"p"` field of ASCII zeros to exactly [`PAYLOAD_BYTES`].
fn padded(mut p: Payload<'_>, full_body: &str, max_chars: usize) -> Vec<u8> {
    let mut limit = max_chars;
    let mut json;
    loop {
        p.body = cut(full_body, limit);
        json = serde_json::to_vec(&p).expect("payload serialises");
        if json.len() <= MAX_CONTENT_BYTES || limit == 0 {
            break;
        }
        limit -= 1;
    }
    // `,"p":""` adds 7 bytes around the filler.
    let fill = PAYLOAD_BYTES.saturating_sub(json.len() + 7);
    p.p = Some("0".repeat(fill));
    let out = serde_json::to_vec(&p).expect("payload serialises");
    debug_assert_eq!(out.len(), PAYLOAD_BYTES);
    out
}

/// The push payload for a stored event. Never carries the event `title`, `body`,
/// `prompt`, `tags`, `host`, `sender`, `url` or `subtitle`; under the default
/// `content = "state"` it carries no event text field at all.
pub fn payload(ev: &Event, summary: Option<&str>, content: PushContent) -> Vec<u8> {
    let state = ev.state.unwrap_or(AgentState::Working);
    let (body, max) = body_text(ev, summary, content);
    let p = Payload {
        v: 1,
        id: ev.id,
        state: state_name(state),
        title: title(state),
        body: String::new(),
        tag: notification_tag(ev),
        url: "/",
        p: None,
    };
    padded(p, &body, max)
}

/// The `/webpush/test` payload: fixed title, empty body.
pub fn test_payload() -> Vec<u8> {
    let p = Payload {
        v: 1,
        id: 0,
        state: "test",
        title: TEST_TITLE,
        body: String::new(),
        tag: "np-test".into(),
        url: "/",
        p: None,
    };
    padded(p, "", 0)
}

// ---- subscriptions ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubRecord {
    pub id: String,
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub login: String,
    pub label: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    #[serde(default)]
    pub last_ok_ms: u64,
    #[serde(default)]
    pub last_error_class: Option<String>,
    #[serde(default)]
    pub last_error_ms: u64,
    /// Bumped when an upsert changes the keys. In memory only.
    #[serde(skip)]
    pub rev: u64,
}

#[derive(Serialize, Deserialize)]
struct SubFile {
    v: u32,
    key_id: String,
    subscriptions: Vec<SubRecord>,
}

enum LaneJob {
    Event { state: AgentState, origin_ms: u64, control_epoch: u64, payload: Arc<[u8]> },
    Test { payload: Arc<[u8]>, reply: oneshot::Sender<LaneReply> },
}

struct LaneReply {
    status: Option<u16>,
    class: &'static str,
}

/// Subscription records and their lane senders, changed together under one lock.
#[derive(Default)]
pub struct SubSet {
    subs: Vec<SubRecord>,
    lanes: HashMap<String, mpsc::Sender<LaneJob>>,
}

impl SubSet {
    fn get(&self, id: &str) -> Option<&SubRecord> {
        self.subs.iter().find(|r| r.id == id)
    }
}

/// Inserts a record and starts its lane, in one critical section.
fn insert_with_lane(set: &mut SubSet, me: &Weak<WebPush>, rec: SubRecord) {
    let (tx, rx) = mpsc::channel(LANE_SLOTS);
    set.lanes.insert(rec.id.clone(), tx);
    tokio::spawn(lane(me.clone(), rec.id.clone(), rx));
    set.subs.push(rec);
}

fn new_id(set: &SubSet) -> Option<String> {
    loop {
        let mut raw = [0u8; 8];
        getrandom::fill(&mut raw).ok()?;
        let id = hex(&raw);
        if set.get(&id).is_none() {
            return Some(id);
        }
    }
}

pub struct UpsertResult {
    pub id: String,
    pub created: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AddError {
    TooMany,
    Internal,
}

/// Upsert keyed by endpoint: an existing endpoint keeps its id and lane.
fn upsert(set: &mut SubSet, me: &Weak<WebPush>, s: NewSub, now: u64) -> Result<UpsertResult, AddError> {
    if let Some(r) = set.subs.iter_mut().find(|r| r.endpoint == s.endpoint) {
        if r.p256dh != s.p256dh || r.auth != s.auth {
            r.rev += 1;
        }
        r.p256dh = s.p256dh;
        r.auth = s.auth;
        r.label = s.label;
        r.login = s.login;
        r.updated_ms = now;
        return Ok(UpsertResult { id: r.id.clone(), created: false });
    }
    if set.subs.len() >= MAX_SUBSCRIPTIONS {
        return Err(AddError::TooMany);
    }
    let id = new_id(set).ok_or(AddError::Internal)?;
    let rec = SubRecord {
        id: id.clone(),
        endpoint: s.endpoint,
        p256dh: s.p256dh,
        auth: s.auth,
        login: s.login,
        label: s.label,
        created_ms: now,
        updated_ms: now,
        last_ok_ms: 0,
        last_error_class: None,
        last_error_ms: 0,
        rev: 0,
    };
    insert_with_lane(set, me, rec);
    Ok(UpsertResult { id, created: true })
}

/// Which record `remove_sub` targets.
#[derive(Debug, Clone)]
pub enum Selector {
    Id(String),
    Endpoint(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersistError;

#[derive(Debug, PartialEq, Eq)]
pub enum RemoveOutcome {
    /// No such record: nothing removed, no save attempted.
    Absent,
    /// `expect_rev` differs from the record's rev: nothing removed, no save.
    RevMismatch,
    /// Record and lane sender removed, then saved.
    Removed { persisted: Result<(), PersistError> },
}

enum Take {
    Absent,
    RevMismatch,
    Removed,
}

fn take_record(set: &mut SubSet, sel: &Selector, expect_rev: Option<u64>) -> Take {
    let pos = set.subs.iter().position(|r| match sel {
        Selector::Id(id) => &r.id == id,
        Selector::Endpoint(e) => &r.endpoint == e,
    });
    let Some(i) = pos else { return Take::Absent };
    if expect_rev.is_some_and(|rev| rev != set.subs[i].rev) {
        return Take::RevMismatch;
    }
    let rec = set.subs.remove(i);
    set.lanes.remove(&rec.id);
    Take::Removed
}

fn snapshot_bytes(set: &SubSet, key_id: &str) -> Vec<u8> {
    let file = SubFile { v: 1, key_id: key_id.to_string(), subscriptions: set.subs.clone() };
    let mut b = serde_json::to_vec(&file).expect("webpush.json serialises");
    b.push(b'\n');
    b
}

/// Loads `webpush.json`. Returns the records and whether the file must be rewritten
/// (its subscriptions belonged to another VAPID key).
fn load_subs(path: &Path, key_id: &str) -> (Vec<SubRecord>, bool) {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (Vec::new(), false),
        Err(e) => {
            tracing::error!(path = %path.display(), error = %e, "cannot read webpush.json; starting with no subscriptions");
            return (Vec::new(), false);
        }
    };
    match serde_json::from_slice::<SubFile>(&bytes) {
        Ok(f) if f.key_id != key_id => {
            tracing::warn!("webpush.json belongs to another VAPID key; its subscriptions were discarded");
            (Vec::new(), true)
        }
        Ok(f) => (f.subscriptions, false),
        Err(_) => {
            let bad = path.with_file_name(format!("{WEBPUSH_FILE}.bad-{}", now_ms()));
            let moved = fs::rename(path, &bad);
            tracing::error!(moved_to = %bad.display(), moved = moved.is_ok(), "webpush.json is unreadable; starting with no subscriptions");
            (Vec::new(), false)
        }
    }
}

/// Writes `bytes` to `path` through a 0600 temp file, fsync and rename.
pub fn write_atomic_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        ensure_private_dir(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// The function every save goes through; tests inject one that fails on demand.
pub type WriteAtomic = Arc<dyn Fn(&Path, &[u8]) -> io::Result<()> + Send + Sync>;

pub fn default_writer() -> WriteAtomic {
    Arc::new(write_atomic_file)
}

// ---- send results -----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum SendOutcome {
    Ok(u16),
    Network,
    Http5xx(u16),
    Gone(u16),
    Rejected(Option<u16>),
    RateLimited { status: u16, retry_after_secs: u64 },
    PayloadTooLarge(u16),
    Encryption,
}

impl SendOutcome {
    fn class(&self) -> &'static str {
        match self {
            SendOutcome::Ok(_) => "ok",
            SendOutcome::Network => "network",
            SendOutcome::Http5xx(_) => "http_5xx",
            SendOutcome::Gone(_) => "gone",
            SendOutcome::Rejected(_) => "rejected",
            SendOutcome::RateLimited { .. } => "rate_limited",
            SendOutcome::PayloadTooLarge(_) => "payload_too_large",
            SendOutcome::Encryption => "encryption",
        }
    }

    fn status(&self) -> Option<u16> {
        match self {
            SendOutcome::Ok(s) | SendOutcome::Http5xx(s) | SendOutcome::Gone(s) | SendOutcome::PayloadTooLarge(s) => Some(*s),
            SendOutcome::RateLimited { status, .. } => Some(*status),
            SendOutcome::Rejected(s) => *s,
            SendOutcome::Network | SendOutcome::Encryption => None,
        }
    }

    fn retryable(&self) -> bool {
        matches!(self, SendOutcome::Network | SendOutcome::Http5xx(_))
    }
}

/// `Retry-After` in seconds; 60 when absent or unparseable, capped at 600.
fn retry_after_secs(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(RETRY_AFTER_DEFAULT_SECS)
        .min(RETRY_AFTER_MAX_SECS)
}

fn classify(status: u16, retry_after: Option<&str>) -> SendOutcome {
    match status {
        200..=299 => SendOutcome::Ok(status),
        404 | 410 => SendOutcome::Gone(status),
        413 => SendOutcome::PayloadTooLarge(status),
        429 => SendOutcome::RateLimited { status, retry_after_secs: retry_after_secs(retry_after) },
        500..=599 => SendOutcome::Http5xx(status),
        _ => SendOutcome::Rejected(Some(status)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultAction {
    None,
    Persist,
    Remove(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Applied {
    action: ResultAction,
    /// The keys changed during the send; the result was discarded.
    superseded: bool,
}

/// Phase A of a lane's result step, under the `SubSet` lock: applied only when the id
/// still exists and its `rev` is the one the send used.
fn apply_result(set: &mut SubSet, id: &str, rev: u64, outcome: &SendOutcome, now: u64) -> Applied {
    let Some(r) = set.subs.iter_mut().find(|r| r.id == id) else {
        return Applied { action: ResultAction::None, superseded: false };
    };
    if r.rev != rev {
        return Applied { action: ResultAction::None, superseded: true };
    }
    if matches!(outcome, SendOutcome::Gone(_) | SendOutcome::Encryption) {
        return Applied { action: ResultAction::Remove(rev), superseded: false };
    }
    let (before_ok, before_class) = (r.last_ok_ms, r.last_error_class.clone());
    match outcome {
        SendOutcome::Ok(_) => {
            r.last_ok_ms = now;
            r.last_error_class = None;
        }
        other => {
            r.last_error_class = Some(other.class().to_string());
            r.last_error_ms = now;
        }
    }
    let persist = r.last_error_class != before_class || r.last_ok_ms.saturating_sub(before_ok) > OK_PERSIST_MS;
    Applied { action: if persist { ResultAction::Persist } else { ResultAction::None }, superseded: false }
}

// ---- the sender -------------------------------------------------------------------

struct Job {
    coalesce_key: String,
    state: AgentState,
    origin_ms: u64,
    control_epoch: u64,
    payload: Arc<[u8]>,
}

/// Keeps the newest job per coalescing key.
fn coalesce(batch: Vec<Job>) -> Vec<Job> {
    let mut out: Vec<Job> = Vec::with_capacity(batch.len());
    for j in batch {
        if let Some(i) = out.iter().position(|o| o.coalesce_key == j.coalesce_key) {
            out.remove(i);
        }
        out.push(j);
    }
    out
}

/// At most one log line per minute for a repeating condition.
#[derive(Default)]
struct LogEvery(AtomicU64);

impl LogEvery {
    fn due(&self) -> bool {
        let now = now_ms();
        let last = self.0.load(Ordering::Relaxed);
        (last == 0 || now.saturating_sub(last) >= LOG_EVERY_MS)
            && self.0.compare_exchange(last, now.max(1), Ordering::Relaxed, Ordering::Relaxed).is_ok()
    }
}

#[derive(Default)]
struct Counters {
    sent: AtomicU64,
    failed: AtomicU64,
    dropped_rate: AtomicU64,
    dropped_queue: AtomicU64,
    dropped_stale: AtomicU64,
    dropped_paused: AtomicU64,
}

#[derive(Default)]
struct Logs {
    queue_full: LogEvery,
    lane_full: LogEvery,
    rate: LogEvery,
    persist: LogEvery,
}

/// Per-lane state: rate-limit window and `Retry-After` hold.
#[derive(Default)]
struct LaneState {
    hold_until_ms: u64,
    sent_at: VecDeque<u64>,
}

impl LaneState {
    /// Counts a push in the rolling 60 s window; false when the window is full.
    fn rate_ok(&mut self, now: u64) -> bool {
        while self.sent_at.front().is_some_and(|t| now.saturating_sub(*t) >= RATE_WINDOW_MS) {
            self.sent_at.pop_front();
        }
        if self.sent_at.len() >= RATE_MAX {
            return false;
        }
        self.sent_at.push_back(now);
        true
    }
}

/// One `/webpush/test` result. Never carries the endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct TestEntry {
    pub id: String,
    pub label: String,
    pub status: Option<u16>,
    pub class: &'static str,
}

/// The `webpush` object in `GET /status`: no endpoint path, no keys.
#[derive(Debug, Serialize)]
pub struct WebPushView {
    pub enabled: bool,
    pub key_id: Option<String>,
    pub dirty: bool,
    pub sent: u64,
    pub failed: u64,
    pub dropped_rate: u64,
    pub dropped_queue: u64,
    pub dropped_stale: u64,
    pub dropped_paused: u64,
    pub last_ok_ms: u64,
    pub last_error_class: Option<&'static str>,
    pub subscriptions: Vec<SubView>,
}

#[derive(Debug, Serialize)]
pub struct SubView {
    pub id: String,
    pub label: String,
    pub login: String,
    pub endpoint_host: Option<String>,
    pub created_ms: u64,
    pub last_ok_ms: u64,
    pub last_error_class: Option<String>,
}

impl WebPushView {
    pub fn disabled() -> WebPushView {
        WebPushView {
            enabled: false,
            key_id: None,
            dirty: false,
            sent: 0,
            failed: 0,
            dropped_rate: 0,
            dropped_queue: 0,
            dropped_stale: 0,
            dropped_paused: 0,
            last_ok_ms: 0,
            last_error_class: None,
            subscriptions: Vec::new(),
        }
    }
}

/// The Web Push sender. Lives in `AppState` as `Option<Arc<WebPush>>`.
pub struct WebPush {
    me: Weak<WebPush>,
    cfg: WebPushConfig,
    subject: String,
    vapid: Vapid,
    control: Arc<RwLock<ControlState>>,
    policy: EndpointPolicy,
    path: PathBuf,
    writer: WriteAtomic,
    subs: Mutex<SubSet>,
    save_lock: tokio::sync::Mutex<()>,
    dirty: AtomicBool,
    queue: mpsc::Sender<Job>,
    http: reqwest::Client,
    jwts: Mutex<HashMap<String, (String, u64)>>,
    counters: Counters,
    last_ok_ms: AtomicU64,
    last_error_class: Mutex<Option<&'static str>>,
    logs: Logs,
}

impl WebPush {
    /// Loads `<state_dir>/webpush.json`, starts the dispatcher, the flush task and one
    /// lane per stored subscription. Needs a Tokio runtime.
    pub fn start(
        cfg: WebPushConfig,
        key: SigningKey,
        state_dir: &Path,
        control: Arc<RwLock<ControlState>>,
        policy: EndpointPolicy,
        writer: WriteAtomic,
    ) -> io::Result<Arc<WebPush>> {
        let vapid = Vapid::new(key);
        let path = state_dir.join(WEBPUSH_FILE);
        let (records, rewrite) = load_subs(&path, vapid.key_id());
        let http = reqwest::Client::builder()
            .timeout(SEND_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|e| io::Error::other(format!("cannot build the web push client: {e}")))?;
        let (queue, rx) = mpsc::channel(QUEUE_SLOTS);
        let subject = cfg.subject.clone().unwrap_or_default();
        let wp = Arc::new_cyclic(|me| WebPush {
            me: me.clone(),
            cfg,
            subject,
            vapid,
            control,
            policy,
            path,
            writer,
            subs: Mutex::new(SubSet::default()),
            save_lock: tokio::sync::Mutex::new(()),
            dirty: AtomicBool::new(false),
            queue,
            http,
            jwts: Mutex::new(HashMap::new()),
            counters: Counters::default(),
            last_ok_ms: AtomicU64::new(0),
            last_error_class: Mutex::new(None),
            logs: Logs::default(),
        });
        {
            let mut set = wp.lock_subs();
            for r in records {
                insert_with_lane(&mut set, &wp.me, r);
            }
        }
        tokio::spawn(dispatcher(Arc::downgrade(&wp), rx));
        tokio::spawn(flush_loop(Arc::downgrade(&wp)));
        if rewrite {
            let w = wp.clone();
            tokio::spawn(async move {
                let _ = w.persist().await;
            });
        }
        Ok(wp)
    }

    fn lock_subs(&self) -> MutexGuard<'_, SubSet> {
        self.subs.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn config(&self) -> &WebPushConfig {
        &self.cfg
    }

    pub fn endpoint_policy(&self) -> &EndpointPolicy {
        &self.policy
    }

    pub fn public_key_b64(&self) -> String {
        self.vapid.public_b64()
    }

    pub fn key_id(&self) -> &str {
        self.vapid.key_id()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    /// Called by `server::commit()` for every stored event. Never blocks, never fails.
    pub fn notify(&self, ev: &Event, ctx: PushCtx) {
        let filter = self.control.read().unwrap_or_else(|p| p.into_inner()).states.clone();
        if !should_push(ev, ctx.prior_state, ctx.origin_ms, now_ms(), &filter, &self.cfg) {
            return;
        }
        let Some(state) = ev.state else { return };
        let coalesce_key = match ev.session.as_deref() {
            Some(s) => format!("session:{s}"),
            None => format!("state:{}", state_name(state)),
        };
        let job = Job {
            coalesce_key,
            state,
            origin_ms: ctx.origin_ms,
            control_epoch: ctx.control_epoch,
            payload: payload(ev, ctx.summary.as_deref(), self.cfg.content).into(),
        };
        if let Err(mpsc::error::TrySendError::Full(_)) = self.queue.try_send(job) {
            self.counters.dropped_queue.fetch_add(1, Ordering::Relaxed);
            if self.logs.queue_full.due() {
                tracing::warn!("web push queue full; push dropped");
            }
        }
    }

    fn stale(&self, origin_ms: u64, now: u64) -> bool {
        now.saturating_sub(origin_ms) > self.cfg.ttl_secs * 1000
    }

    /// Live pause, filter and epoch check; the lock is released before returning.
    fn control_allows(&self, state: AgentState, epoch: u64) -> bool {
        let c = self.control.read().unwrap_or_else(|p| p.into_inner());
        !c.paused && c.states.contains(&state) && c.epoch == epoch
    }

    /// Fans coalesced jobs out to every lane; never waits on the network.
    fn dispatch(&self, jobs: Vec<Job>) {
        let now = now_ms();
        let jobs: Vec<Job> = jobs
            .into_iter()
            .filter(|j| {
                if self.stale(j.origin_ms, now) {
                    self.counters.dropped_stale.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!("stale web push dropped");
                    false
                } else if !self.control_allows(j.state, j.control_epoch) {
                    self.counters.dropped_paused.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!("web push dropped by pause or filter");
                    false
                } else {
                    true
                }
            })
            .collect();
        if jobs.is_empty() {
            return;
        }
        let lanes: Vec<(String, mpsc::Sender<LaneJob>)> =
            self.lock_subs().lanes.iter().map(|(id, tx)| (id.clone(), tx.clone())).collect();
        for j in &jobs {
            for (id, tx) in &lanes {
                let job = LaneJob::Event {
                    state: j.state,
                    origin_ms: j.origin_ms,
                    control_epoch: j.control_epoch,
                    payload: j.payload.clone(),
                };
                if let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send(job) {
                    self.counters.dropped_queue.fetch_add(1, Ordering::Relaxed);
                    if self.logs.lane_full.due() {
                        tracing::warn!(id = %id, "web push lane full; push dropped for this subscription");
                    }
                }
            }
        }
    }

    /// Upserts a subscription and saves. Returns the upsert and its own save result.
    pub async fn add_sub(&self, s: NewSub) -> Result<(UpsertResult, Result<(), PersistError>), AddError> {
        let res = {
            let mut set = self.lock_subs();
            upsert(&mut set, &self.me, s, now_ms())?
        };
        Ok((res, self.persist().await))
    }

    /// The only code that removes a record and its lane sender. Call with no `SubSet`
    /// guard held.
    pub async fn remove_sub(&self, sel: Selector, expect_rev: Option<u64>) -> RemoveOutcome {
        let taken = {
            let mut set = self.lock_subs();
            take_record(&mut set, &sel, expect_rev)
        };
        match taken {
            Take::Absent => RemoveOutcome::Absent,
            Take::RevMismatch => RemoveOutcome::RevMismatch,
            Take::Removed => RemoveOutcome::Removed { persisted: self.persist().await },
        }
    }

    /// The one writer of `webpush.json`. A failure keeps the in-memory change, sets
    /// `dirty` and leaves the retry to the flush task.
    pub async fn persist(&self) -> Result<(), PersistError> {
        let _serial = self.save_lock.lock().await;
        let bytes = {
            let set = self.lock_subs();
            snapshot_bytes(&set, self.vapid.key_id())
        };
        let (writer, path) = (self.writer.clone(), self.path.clone());
        let result = tokio::task::spawn_blocking(move || writer(&path, &bytes))
            .await
            .unwrap_or_else(|e| Err(io::Error::other(e.to_string())));
        match result {
            Ok(()) => {
                self.dirty.store(false, Ordering::Relaxed);
                Ok(())
            }
            Err(e) => {
                self.dirty.store(true, Ordering::Relaxed);
                if self.logs.persist.due() {
                    tracing::error!(error = %e, "cannot save webpush.json; retrying every 30 s");
                }
                Err(PersistError)
            }
        }
    }

    /// Sends a test push to every subscription through its lane, bypassing the
    /// dispatcher, pause, filter and freshness. Lists exactly the subscriptions in the
    /// snapshot, in `created_ms` order.
    pub async fn test_all(&self) -> Vec<TestEntry> {
        let snapshot: Vec<(String, String, mpsc::Sender<LaneJob>)> = {
            let set = self.lock_subs();
            let mut subs: Vec<&SubRecord> = set.subs.iter().collect();
            subs.sort_by_key(|r| r.created_ms);
            subs.into_iter()
                .filter_map(|r| set.lanes.get(&r.id).map(|tx| (r.id.clone(), r.label.clone(), tx.clone())))
                .collect()
        };
        let payload: Arc<[u8]> = test_payload().into();
        let mut waits = Vec::with_capacity(snapshot.len());
        for (id, label, tx) in snapshot {
            let (reply, rx) = oneshot::channel();
            let wait = match tx.try_send(LaneJob::Test { payload: payload.clone(), reply }) {
                Ok(()) => Ok(rx),
                Err(mpsc::error::TrySendError::Full(_)) => Err("lane_full"),
                Err(mpsc::error::TrySendError::Closed(_)) => Err("removed"),
            };
            waits.push((id, label, wait));
        }
        let deadline = tokio::time::Instant::now() + TEST_WAIT;
        let mut out = Vec::with_capacity(waits.len());
        for (id, label, wait) in waits {
            let (status, class) = match wait {
                Err(class) => (None, class),
                Ok(rx) => match tokio::time::timeout_at(deadline, rx).await {
                    Ok(Ok(r)) => (r.status, r.class),
                    Ok(Err(_)) => (None, "removed"),
                    Err(_) => (None, "pending"),
                },
            };
            out.push(TestEntry { id, label, status, class });
        }
        out
    }

    pub fn view(&self) -> WebPushView {
        let c = &self.counters;
        let subscriptions = self
            .lock_subs()
            .subs
            .iter()
            .map(|r| SubView {
                id: r.id.clone(),
                label: r.label.clone(),
                login: r.login.clone(),
                endpoint_host: Url::parse(&r.endpoint).ok().and_then(|u| u.host_str().map(str::to_string)),
                created_ms: r.created_ms,
                last_ok_ms: r.last_ok_ms,
                last_error_class: r.last_error_class.clone(),
            })
            .collect();
        WebPushView {
            enabled: true,
            key_id: Some(self.key_id().to_string()),
            dirty: self.is_dirty(),
            sent: c.sent.load(Ordering::Relaxed),
            failed: c.failed.load(Ordering::Relaxed),
            dropped_rate: c.dropped_rate.load(Ordering::Relaxed),
            dropped_queue: c.dropped_queue.load(Ordering::Relaxed),
            dropped_stale: c.dropped_stale.load(Ordering::Relaxed),
            dropped_paused: c.dropped_paused.load(Ordering::Relaxed),
            last_ok_ms: self.last_ok_ms.load(Ordering::Relaxed),
            last_error_class: *self.last_error_class.lock().unwrap_or_else(|p| p.into_inner()),
            subscriptions,
        }
    }

    /// The cached JWT for an endpoint origin, renewed when less than 1 h is left.
    fn jwt_for(&self, origin: &str) -> String {
        let now = now_ms() / 1000;
        let mut cache = self.jwts.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((t, exp)) = cache.get(origin) {
            if exp.saturating_sub(now) > JWT_RENEW_SECS {
                return t.clone();
            }
        }
        let exp = now + JWT_LIFETIME_SECS;
        let t = self.vapid.jwt(origin, &self.subject, exp);
        cache.insert(origin.to_string(), (t.clone(), exp));
        t
    }

    /// Lane steps 3 to 6: encrypt, re-check the endpoint, sign, send, classify.
    async fn send_once(&self, id: &str, endpoint: &str, p256dh: &str, auth: &str, payload: &[u8]) -> SendOutcome {
        let keys = b64url_decode(p256dh)
            .and_then(|k| p256::PublicKey::from_sec1_bytes(&k).ok())
            .zip(b64url_decode(auth).filter(|a| a.len() == 16));
        let Some((ua_public, ua_auth)) = keys else {
            tracing::error!(id, "stored subscription keys are unusable");
            return SendOutcome::Encryption;
        };
        let (Ok(uri), Ok(url)) = (endpoint.parse::<http::Uri>(), Url::parse(endpoint)) else {
            tracing::error!(id, "stored endpoint is not a URL; not sending");
            return SendOutcome::Rejected(None);
        };
        let built = WebPushBuilder::new(uri, ua_public, Auth::clone_from_slice(&ua_auth))
            .with_valid_duration(Duration::from_secs(self.cfg.ttl_secs))
            .build(payload.to_vec());
        let Ok(mut req) = built else {
            tracing::error!(id, "web push encryption failed");
            return SendOutcome::Encryption;
        };
        let host = url.host_str().unwrap_or("").to_string();
        if self.policy.check(&url).is_err() {
            tracing::error!(id, host = %host, "stored endpoint is outside the endpoint policy; not sending");
            return SendOutcome::Rejected(None);
        }
        let auth_header = format!("vapid t={}, k={}", self.jwt_for(&endpoint_origin(&url)), self.vapid.public_b64());
        let headers = req.headers_mut();
        match http::HeaderValue::from_str(&auth_header) {
            Ok(v) => {
                headers.insert(http::header::AUTHORIZATION, v);
            }
            Err(_) => return SendOutcome::Rejected(None),
        }
        headers.insert("urgency", http::HeaderValue::from_static("high"));
        let Ok(req) = reqwest::Request::try_from(req) else {
            return SendOutcome::Rejected(None);
        };
        match self.http.execute(req).await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let retry_after = resp.headers().get(http::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).map(str::to_string);
                drop(resp); // the body is never read
                let outcome = classify(status, retry_after.as_deref());
                match &outcome {
                    SendOutcome::Ok(_) => tracing::debug!(id, status, "web push sent"),
                    SendOutcome::Gone(_) => tracing::info!(id, status, "web push subscription is gone; removing it"),
                    SendOutcome::PayloadTooLarge(_) => tracing::error!(id, status, "web push payload too large"),
                    SendOutcome::Http5xx(_) | SendOutcome::RateLimited { .. } | SendOutcome::Rejected(_) => {
                        tracing::warn!(id, host = %host, status, "web push failed")
                    }
                    SendOutcome::Network | SendOutcome::Encryption => {}
                }
                outcome
            }
            Err(e) => {
                let class = if e.is_timeout() { "timeout" } else { "network" };
                tracing::warn!(id, host = %host, error = class, "web push failed");
                SendOutcome::Network
            }
        }
    }

    fn count(&self, outcome: &SendOutcome) {
        if let SendOutcome::Ok(_) = outcome {
            self.counters.sent.fetch_add(1, Ordering::Relaxed);
            self.last_ok_ms.store(now_ms(), Ordering::Relaxed);
        } else {
            self.counters.failed.fetch_add(1, Ordering::Relaxed);
            *self.last_error_class.lock().unwrap_or_else(|p| p.into_inner()) = Some(outcome.class());
        }
    }

    /// One lane job. Returns true when the lane must exit.
    async fn run_job(&self, id: &str, ls: &mut LaneState, job: LaneJob) -> bool {
        let (event, payload, mut reply) = match job {
            LaneJob::Event { state, origin_ms, control_epoch, payload } => (Some((state, origin_ms, control_epoch)), payload, None),
            LaneJob::Test { payload, reply } => (None, payload, Some(reply)),
        };
        let mut answer = |status: Option<u16>, class: &'static str| {
            if let Some(r) = reply.take() {
                let _ = r.send(LaneReply { status, class });
            }
        };
        let mut first = true;
        loop {
            // 1. The current record, by id.
            let rec = {
                let set = self.lock_subs();
                set.get(id).map(|r| (r.endpoint.clone(), r.p256dh.clone(), r.auth.clone(), r.rev))
            };
            let Some((endpoint, p256dh, auth, rev)) = rec else {
                answer(None, "removed");
                return true;
            };
            // 2. Age, live controls, Retry-After hold, rate limit.
            let now = now_ms();
            if let Some((state, origin_ms, epoch)) = event {
                if self.stale(origin_ms, now) {
                    self.counters.dropped_stale.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(id, "stale web push dropped");
                    return false;
                }
                if !self.control_allows(state, epoch) {
                    self.counters.dropped_paused.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(id, "web push dropped by pause or filter");
                    return false;
                }
            }
            if now < ls.hold_until_ms || (first && !ls.rate_ok(now)) {
                if event.is_some() {
                    self.counters.dropped_rate.fetch_add(1, Ordering::Relaxed);
                    if self.logs.rate.due() {
                        tracing::warn!(id, "web push rate limited; push dropped");
                    }
                }
                answer(None, "rate_limited");
                return false;
            }
            // 3-6. Build, check, sign, send.
            let outcome = self.send_once(id, &endpoint, &p256dh, &auth, &payload).await;
            self.count(&outcome);
            // 7. One retry on a network error or 5xx.
            if first && outcome.retryable() {
                first = false;
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
            if let SendOutcome::RateLimited { retry_after_secs, .. } = outcome {
                ls.hold_until_ms = now_ms() + retry_after_secs * 1000;
            }
            // 8. Phase A under the lock, phase B with no lock held.
            let applied = {
                let mut set = self.lock_subs();
                apply_result(&mut set, id, rev, &outcome, now_ms())
            };
            let (exit, class) = match applied.action {
                ResultAction::None => (false, if applied.superseded { "superseded" } else { outcome.class() }),
                ResultAction::Persist => {
                    let _ = self.persist().await;
                    (false, outcome.class())
                }
                ResultAction::Remove(rev) => match self.remove_sub(Selector::Id(id.to_string()), Some(rev)).await {
                    RemoveOutcome::Removed { .. } | RemoveOutcome::Absent => (true, outcome.class()),
                    RemoveOutcome::RevMismatch => (false, "superseded"),
                },
            };
            answer(outcome.status(), class);
            return exit;
        }
    }
}

async fn dispatcher(weak: Weak<WebPush>, mut rx: mpsc::Receiver<Job>) {
    while let Some(first) = rx.recv().await {
        tokio::time::sleep(COALESCE_WINDOW).await;
        let mut batch = vec![first];
        while let Ok(j) = rx.try_recv() {
            batch.push(j);
        }
        let Some(wp) = weak.upgrade() else { return };
        wp.dispatch(coalesce(batch));
    }
}

async fn lane(weak: Weak<WebPush>, id: String, mut rx: mpsc::Receiver<LaneJob>) {
    let mut ls = LaneState::default();
    while let Some(job) = rx.recv().await {
        let Some(wp) = weak.upgrade() else { return };
        if wp.run_job(&id, &mut ls, job).await {
            return;
        }
    }
}

/// Retries a failed save every 30 s while `dirty` is set.
async fn flush_loop(weak: Weak<WebPush>) {
    loop {
        tokio::time::sleep(FLUSH_EVERY).await;
        let Some(wp) = weak.upgrade() else { return };
        if wp.is_dirty() {
            let _ = wp.persist().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PushContent;

    fn ev(state: Option<AgentState>) -> Event {
        let mut e = crate::event::NewEvent { title: "t".into(), state, ..Default::default() }.into_event(7, 1_000);
        e.session = Some("kiro/sess-ab12".into());
        e
    }

    fn cfg() -> WebPushConfig {
        WebPushConfig { enabled: true, subject: Some("mailto:you@example.com".into()), ..WebPushConfig::default() }
    }

    const ALL: &[AgentState] = &[AgentState::YourTurn, AgentState::Allow, AgentState::AllSet, AgentState::Working];

    #[test]
    fn should_push_truth_table() {
        let c = cfg();
        let now = 1_000_000;
        for s in [AgentState::YourTurn, AgentState::Allow, AgentState::AllSet] {
            assert!(should_push(&ev(Some(s)), None, now, now, ALL, &c), "{s:?}");
        }
        assert!(!should_push(&ev(Some(AgentState::Working)), None, now, now, ALL, &c), "working");
        assert!(!should_push(&ev(None), None, now, now, ALL, &c), "no state");
        let mut muted = ev(Some(AgentState::AllSet));
        muted.muted = true;
        assert!(!should_push(&muted, None, now, now, ALL, &c), "muted");
        assert!(!should_push(&ev(Some(AgentState::AllSet)), None, now, now, &[AgentState::YourTurn], &c), "filter excludes");
        let only = WebPushConfig { states: vec![AgentState::YourTurn], ..cfg() };
        assert!(!should_push(&ev(Some(AgentState::AllSet)), None, now, now, ALL, &only), "[webpush] states excludes");
        let e = ev(Some(AgentState::YourTurn));
        assert!(should_push(&e, None, now - 300_000, now, ALL, &c), "age = ttl pushes");
        assert!(!should_push(&e, None, now - 301_000, now, ALL, &c), "age = ttl + 1 s does not");
        assert!(should_push(&e, None, now + 60_000, now, ALL, &c), "origin in the future is age 0");
    }

    #[test]
    fn repeated_all_set_is_not_pushed() {
        use AgentState::*;
        let c = cfg();
        let now = 1_000_000;
        let push = |s, prior| should_push(&ev(Some(s)), prior, now, now, ALL, &c);
        assert!(!push(AllSet, Some(AllSet)), "consecutive all_set suppressed");
        assert!(push(AllSet, Some(YourTurn)), "your_turn in between allows the next all_set");
        assert!(push(AllSet, Some(Working)), "working counts as a turn boundary");
        assert!(push(AllSet, Some(Allow)), "allow in between allows the next all_set");
        assert!(push(AllSet, None), "no prior event pushes");
        assert!(push(YourTurn, Some(YourTurn)) && push(YourTurn, Some(AllSet)), "your_turn unaffected");
        assert!(push(Allow, Some(Allow)) && push(Allow, Some(AllSet)), "allow unaffected");
    }

    fn json(bytes: &[u8]) -> serde_json::Value {
        serde_json::from_slice(bytes).unwrap()
    }

    #[test]
    fn titles_are_fixed_per_state_under_every_content_level() {
        let want = [
            (AgentState::YourTurn, "✋ your turn"),
            (AgentState::AllSet, "✅ all set"),
            (AgentState::Allow, "❓ allow?"),
        ];
        for content in [PushContent::State, PushContent::Summary, PushContent::Label, PushContent::Detail] {
            for (s, t) in want {
                let mut e = ev(Some(s));
                e.title = "MARK title".into();
                e.session_label = Some("MARK label".into());
                let p = payload(&e, Some("MARK summary"), content);
                assert_eq!(p.len(), PAYLOAD_BYTES);
                let v = json(&p);
                assert_eq!(v["title"], t, "{content:?}");
                assert!(!v["title"].as_str().unwrap().contains("MARK"));
            }
        }
    }

    #[test]
    fn default_payload_carries_no_event_text() {
        let mut e = ev(Some(AgentState::Allow));
        for f in [&mut e.session_label, &mut e.detail, &mut e.project, &mut e.body, &mut e.url, &mut e.subtitle, &mut e.host] {
            *f = Some("MARKER".into());
        }
        e.title = "MARKER".into();
        e.tags = vec!["MARKER".into()];
        let p = payload(&e, Some("MARKER"), WebPushConfig::default().content);
        let text = String::from_utf8(p.clone()).unwrap();
        assert!(!text.contains("MARKER"), "{text}");
        let v = json(&p);
        assert_eq!(v["body"], "");
        assert_eq!(v["url"], "/");
        assert_eq!(v["state"], "allow");
        assert_eq!(v["id"], 7);
        for absent in ["prompt", "host", "tags", "sender", "subtitle"] {
            assert!(v.get(absent).is_none(), "{absent}");
        }
    }

    #[test]
    fn payload_content_levels() {
        let mut e = ev(Some(AgentState::YourTurn));
        e.session_label = Some("raw first prompt".into());
        e.project = Some("frontend".into());
        e.detail = Some("execute_bash: ls".into());
        let body = |summary: Option<&str>, c| json(&payload(&e, summary, c))["body"].as_str().unwrap().to_string();
        assert_eq!(body(Some("Fix login"), PushContent::State), "");
        assert_eq!(body(Some("Fix login"), PushContent::Summary), "Fix login · frontend");
        assert_eq!(body(None, PushContent::Summary), "#ab12 · frontend", "falls back to the tag, never the raw label");
        assert_eq!(body(None, PushContent::Label), "raw first prompt · frontend");
        assert_eq!(body(None, PushContent::Detail), "raw first prompt · frontend · execute_bash: ls");
        for c in [PushContent::Summary, PushContent::Label, PushContent::Detail] {
            assert!(!json(&payload(&e, None, c)).to_string().contains("\"prompt\""));
        }
    }

    #[test]
    fn payload_truncates_cjk_with_ellipsis_and_is_always_1024_bytes() {
        let mut e = ev(Some(AgentState::AllSet));
        e.project = Some("修".repeat(100));
        e.session_label = Some("正".repeat(100));
        e.detail = Some("登".repeat(300));
        for c in [PushContent::Summary, PushContent::Label, PushContent::Detail] {
            let p = payload(&e, Some(&"摘".repeat(100)), c);
            assert_eq!(p.len(), PAYLOAD_BYTES, "{c:?}");
            let b = json(&p)["body"].as_str().unwrap().to_string();
            let max = if c == PushContent::Detail { 160 } else { 120 };
            assert_eq!(b.chars().count(), max, "{c:?}");
            assert!(b.ends_with('…'));
        }
        // Control characters escape to six bytes each; the fit still holds.
        e.session_label = Some("\u{1}".repeat(100));
        e.detail = Some("\u{1}".repeat(300));
        assert_eq!(payload(&e, None, PushContent::Detail).len(), PAYLOAD_BYTES);
        assert_eq!(payload(&ev(Some(AgentState::AllSet)), None, PushContent::State).len(), PAYLOAD_BYTES);
        assert_eq!(test_payload().len(), PAYLOAD_BYTES);
        assert_eq!(json(&test_payload())["title"], TEST_TITLE);
    }

    #[test]
    fn notification_tag_hashes_the_full_session() {
        let mut a = ev(Some(AgentState::AllSet));
        let mut b = a.clone();
        a.session = Some("one/xx-ab12".into());
        b.session = Some("two/yy-ab12".into());
        let (ta, tb) = (notification_tag(&a), notification_tag(&b));
        assert!(ta.starts_with("np-") && ta.len() == 19 && ta[3..].bytes().all(|c| c.is_ascii_hexdigit()), "{ta}");
        assert_ne!(ta, tb, "same suffix, different tags");
        a.session = None;
        assert_eq!(notification_tag(&a), "np-all_set");
    }

    fn job(key: &str, n: u64) -> Job {
        Job { coalesce_key: key.into(), state: AgentState::AllSet, origin_ms: n, control_epoch: 0, payload: Arc::from(&b""[..]) }
    }

    #[test]
    fn coalescing_keeps_newest_per_session() {
        let out = coalesce(vec![job("session:a", 1), job("session:b", 2), job("session:a", 3)]);
        let got: Vec<(&str, u64)> = out.iter().map(|j| (j.coalesce_key.as_str(), j.origin_ms)).collect();
        assert_eq!(got, vec![("session:b", 2), ("session:a", 3)]);
    }

    #[test]
    fn rate_limiter_drops_the_21st_push_in_60_s() {
        let mut ls = LaneState::default();
        for i in 0..20 {
            assert!(ls.rate_ok(1_000 + i));
        }
        assert!(!ls.rate_ok(1_030), "21st inside the window");
        assert!(ls.rate_ok(61_000), "after the window passes");
    }

    #[test]
    fn retry_after_parsing() {
        assert_eq!(retry_after_secs(Some("30")), 30);
        assert_eq!(retry_after_secs(None), 60);
        assert_eq!(retry_after_secs(Some("Wed, 21 Oct 2015 07:28:00 GMT")), 60);
        assert_eq!(retry_after_secs(Some("9999")), 600);
    }

    #[test]
    fn response_classification() {
        assert_eq!(classify(201, None).class(), "ok");
        assert_eq!(classify(404, None).class(), "gone");
        assert_eq!(classify(410, None).class(), "gone");
        assert_eq!(classify(429, Some("30")), SendOutcome::RateLimited { status: 429, retry_after_secs: 30 });
        assert_eq!(classify(429, None), SendOutcome::RateLimited { status: 429, retry_after_secs: 60 });
        assert_eq!(classify(403, None).class(), "rejected");
        assert_eq!(classify(400, None).class(), "rejected");
        assert_eq!(classify(413, None).class(), "payload_too_large");
        assert!(classify(503, None).retryable());
        assert!(!classify(403, None).retryable());
        assert!(SendOutcome::Network.retryable());
    }

    fn rec(id: &str, rev: u64) -> SubRecord {
        SubRecord {
            id: id.into(),
            endpoint: format!("https://web.push.apple.com/{id}"),
            p256dh: "k".into(),
            auth: "a".into(),
            login: "l".into(),
            label: "d".into(),
            created_ms: 1,
            updated_ms: 1,
            last_ok_ms: 0,
            last_error_class: None,
            last_error_ms: 0,
            rev,
        }
    }

    #[test]
    fn apply_result_over_a_plain_subset() {
        let mut set = SubSet::default();
        set.subs.push(rec("a", 2));
        let none = Applied { action: ResultAction::None, superseded: false };
        assert_eq!(apply_result(&mut set, "gone-id", 2, &SendOutcome::Ok(201), 5), none, "removed id");
        assert_eq!(apply_result(&mut set, "a", 1, &SendOutcome::Ok(201), 5), Applied { action: ResultAction::None, superseded: true });
        assert_eq!(apply_result(&mut set, "a", 2, &SendOutcome::Gone(410), 5).action, ResultAction::Remove(2));
        assert_eq!(apply_result(&mut set, "a", 2, &SendOutcome::Encryption, 5).action, ResultAction::Remove(2));
        assert_eq!(set.subs[0].last_ok_ms, 0, "nothing changed so far");
        let t = OK_PERSIST_MS * 2;
        assert_eq!(apply_result(&mut set, "a", 2, &SendOutcome::Ok(201), t).action, ResultAction::Persist, "first ok");
        assert_eq!(apply_result(&mut set, "a", 2, &SendOutcome::Ok(201), t + 1000).action, ResultAction::None, "ok again soon");
        assert_eq!(apply_result(&mut set, "a", 2, &SendOutcome::Http5xx(503), t + 2000).action, ResultAction::Persist, "class changed");
        assert_eq!(apply_result(&mut set, "a", 2, &SendOutcome::Http5xx(502), t + 3000).action, ResultAction::None, "same class");
        assert_eq!(set.subs[0].last_error_class.as_deref(), Some("http_5xx"));
    }

    #[test]
    fn endpoint_policy() {
        let p = EndpointPolicy::new(&["web.push.apple.com".to_string()]);
        let ok = |u: &str| p.check(&Url::parse(u).unwrap()).is_ok();
        assert!(ok("https://web.push.apple.com/abc"));
        assert!(ok("https://WEB.PUSH.APPLE.COM/abc"));
        assert!(ok("https://web.push.apple.com:443/abc"));
        assert!(!ok("https://web.push.apple.com:8443/abc"));
        assert!(!ok("http://web.push.apple.com/abc"));
        assert!(!ok("https://user@web.push.apple.com/abc"));
        assert!(!ok("https://evil.example/abc"));
        assert!(!ok("https://web.push.apple.com.evil.example/abc"));
        assert!(!ok("http://127.0.0.1:41234/abc"));
        let t = p.clone().with_loopback_http(41234);
        assert!(t.check(&Url::parse("http://127.0.0.1:41234/abc").unwrap()).is_ok());
        assert!(t.check(&Url::parse("http://127.0.0.1:41235/abc").unwrap()).is_err());
    }

    fn key_b64() -> String {
        let sk = generate_signing_key().unwrap();
        Base64UrlUnpadded::encode_string(sk.verifying_key().to_encoded_point(false).as_bytes())
    }

    #[test]
    fn subscription_validation_table() {
        let p = EndpointPolicy::new(&["web.push.apple.com".to_string()]);
        let k = key_b64();
        let a = Base64UrlUnpadded::encode_string(&[7u8; 16]);
        let ep = "https://web.push.apple.com/abc";
        let v = |e: &str, k: &str, a: &str, l: Option<&str>| validate_subscription(&p, e, k, a, l);
        let s = v(ep, &k, &a, None).unwrap();
        assert_eq!(s.label, "device");
        assert_eq!(v(ep, &k, &a, Some("  pahud iPhone ")).unwrap().label, "pahud iPhone");
        assert!(v(ep, &format!("{k}="), &format!("{a}=="), None).is_ok(), "padding tolerated");
        assert_eq!(v("https://evil.example/x", &k, &a, None).unwrap_err(), "endpoint not allowed");
        assert_eq!(v("not a url", &k, &a, None).unwrap_err(), "endpoint not allowed");
        assert_eq!(v(&format!("{ep}/{}", "x".repeat(1024)), &k, &a, None).unwrap_err(), "endpoint not allowed");
        assert!(v(ep, "AAAA", &a, None).is_err(), "short p256dh");
        let off_curve = Base64UrlUnpadded::encode_string(&[4u8; 65]);
        assert!(v(ep, &off_curve, &a, None).is_err(), "not a curve point");
        assert!(v(ep, &k, &Base64UrlUnpadded::encode_string(&[1u8; 15]), None).is_err(), "auth 15 bytes");
        assert!(v(ep, &k, "!!!", None).is_err());
        assert!(v(ep, &k, &a, Some(&"x".repeat(33))).is_err(), "label too long");
        assert!(v(ep, &k, &a, Some("a\nb")).is_err(), "control char");
        assert_eq!(v(ep, &k, &a, Some(&"x".repeat(32))).unwrap().label.len(), 32);
    }

    #[test]
    fn vapid_key_from_random_retries_zero_and_round_trips_pem() {
        let mut calls = 0;
        let key = key_from_random(|b| {
            calls += 1;
            if calls == 1 {
                *b = [0u8; 32]; // zero scalar: invalid
            } else {
                getrandom::fill(b).unwrap();
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, 2, "zero scalar was retried");
        let pem = to_pem(&key).unwrap();
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----"));
        assert_eq!(from_pem(&pem).unwrap().to_bytes(), key.to_bytes());
    }

    #[test]
    fn vapid_file_private_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg").join(VAPID_FILE);
        let a = Vapid::new(load_or_create_vapid(&path).unwrap());
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        let b = Vapid::new(load_or_create_vapid(&path).unwrap());
        assert_eq!(a.key_id(), b.key_id());
        assert_eq!(a.key_id().len(), 16);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        load_or_create_vapid(&path).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600, "chmod'ed back");
        fs::write(&path, "not a key").unwrap();
        assert!(load_or_create_vapid(&path).is_err());
    }

    #[test]
    fn vapid_jwt_claims_and_signature() {
        use p256::ecdsa::signature::Verifier;
        let v = Vapid::new(generate_signing_key().unwrap());
        for (endpoint, aud) in [
            ("https://web.push.apple.com/abc", "https://web.push.apple.com"),
            ("http://127.0.0.1:41234/abc", "http://127.0.0.1:41234"),
        ] {
            assert_eq!(endpoint_origin(&Url::parse(endpoint).unwrap()), aud);
            let t = v.jwt(aud, "mailto:you@example.com", 1_900_000_000);
            let parts: Vec<&str> = t.split('.').collect();
            assert_eq!(parts.len(), 3);
            let header: serde_json::Value = serde_json::from_slice(&Base64UrlUnpadded::decode_vec(parts[0]).unwrap()).unwrap();
            assert_eq!(header, serde_json::json!({"typ": "JWT", "alg": "ES256"}));
            let claims: serde_json::Value = serde_json::from_slice(&Base64UrlUnpadded::decode_vec(parts[1]).unwrap()).unwrap();
            assert_eq!(claims, serde_json::json!({"aud": aud, "exp": 1_900_000_000u64, "sub": "mailto:you@example.com"}));
            let sig = Signature::from_slice(&Base64UrlUnpadded::decode_vec(parts[2]).unwrap()).unwrap();
            v.verifying_key().verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).unwrap();
        }
    }

    // ---- store, remove_sub, persist -----------------------------------------------

    fn control() -> Arc<RwLock<ControlState>> {
        let st = crate::server::AppState::new(
            Arc::new(crate::store::Store::open(tempfile::tempdir().unwrap().path(), 10, 4).unwrap()),
            "t".into(),
            vec![],
            false,
        );
        st.control.clone()
    }

    struct Counting {
        calls: Arc<AtomicU64>,
        fail: Arc<AtomicBool>,
    }

    fn counting_writer() -> (WriteAtomic, Counting) {
        let calls = Arc::new(AtomicU64::new(0));
        let fail = Arc::new(AtomicBool::new(false));
        let (c, f) = (calls.clone(), fail.clone());
        let w: WriteAtomic = Arc::new(move |p: &Path, b: &[u8]| {
            c.fetch_add(1, Ordering::SeqCst);
            if f.load(Ordering::SeqCst) {
                return Err(io::Error::other("injected fault"));
            }
            write_atomic_file(p, b)
        });
        (w, Counting { calls, fail })
    }

    fn start(dir: &Path, writer: WriteAtomic, key: SigningKey) -> Arc<WebPush> {
        WebPush::start(cfg(), key, dir, control(), EndpointPolicy::new(&cfg().endpoint_hosts), writer).unwrap()
    }

    fn sub(n: u32) -> NewSub {
        let k = key_b64();
        NewSub {
            endpoint: format!("https://web.push.apple.com/ep{n}"),
            p256dh: k,
            auth: Base64UrlUnpadded::encode_string(&[n as u8; 16]),
            label: format!("dev{n}"),
            login: "you@example.com".into(),
        }
    }

    #[tokio::test]
    async fn remove_sub_outcomes() {
        let dir = tempfile::tempdir().unwrap();
        let (w, c) = counting_writer();
        let wp = start(dir.path(), w, generate_signing_key().unwrap());
        let (a, saved) = wp.add_sub(sub(1)).await.unwrap();
        assert!(a.created && saved.is_ok());
        let base = c.calls.load(Ordering::SeqCst);

        assert_eq!(wp.remove_sub(Selector::Id("0000000000000000".into()), None).await, RemoveOutcome::Absent);
        assert_eq!(wp.remove_sub(Selector::Endpoint("https://web.push.apple.com/none".into()), None).await, RemoveOutcome::Absent);
        assert_eq!(c.calls.load(Ordering::SeqCst), base, "no write for Absent");

        // A key change bumps rev; the old rev no longer removes.
        let mut again = sub(1);
        again.p256dh = key_b64();
        let (b, _) = wp.add_sub(again).await.unwrap();
        assert!(!b.created && b.id == a.id);
        let base = c.calls.load(Ordering::SeqCst);
        assert_eq!(wp.remove_sub(Selector::Id(a.id.clone()), Some(0)).await, RemoveOutcome::RevMismatch);
        assert_eq!(c.calls.load(Ordering::SeqCst), base, "no write for RevMismatch");
        assert_eq!(wp.view().subscriptions.len(), 1);

        assert_eq!(wp.remove_sub(Selector::Id(a.id.clone()), Some(1)).await, RemoveOutcome::Removed { persisted: Ok(()) });
        {
            let set = wp.lock_subs();
            assert!(set.subs.is_empty() && set.lanes.is_empty(), "record and lane sender removed together");
        }

        let (d, _) = wp.add_sub(sub(2)).await.unwrap();
        c.fail.store(true, Ordering::SeqCst);
        assert_eq!(wp.remove_sub(Selector::Id(d.id), None).await, RemoveOutcome::Removed { persisted: Err(PersistError) });
        assert!(wp.lock_subs().subs.is_empty(), "removal kept in memory");
        assert!(wp.is_dirty());
        c.fail.store(false, Ordering::SeqCst);
        wp.persist().await.unwrap();
        assert!(!wp.is_dirty());
    }

    #[tokio::test]
    async fn store_round_trip_bad_file_and_key_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let key = generate_signing_key().unwrap();
        let wp = start(dir.path(), default_writer(), key.clone());
        let (a, _) = wp.add_sub(sub(1)).await.unwrap();
        let path = dir.path().join(WEBPUSH_FILE);
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        drop(wp);

        let wp = start(dir.path(), default_writer(), key.clone());
        assert_eq!(wp.view().subscriptions[0].id, a.id, "round trip");
        assert_eq!(wp.view().subscriptions[0].endpoint_host.as_deref(), Some("web.push.apple.com"));
        drop(wp);

        // Another key: subscriptions are discarded and the file rewritten.
        let wp = start(dir.path(), default_writer(), generate_signing_key().unwrap());
        assert!(wp.view().subscriptions.is_empty());
        for _ in 0..50 {
            let f: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            if f["key_id"] == wp.key_id() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let f: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(f["key_id"], wp.key_id());
        assert_eq!(f["subscriptions"], serde_json::json!([]));
        drop(wp);

        fs::write(&path, "{broken").unwrap();
        let wp = start(dir.path(), default_writer(), key);
        assert!(wp.view().subscriptions.is_empty());
        assert!(!path.exists());
        let moved = fs::read_dir(dir.path()).unwrap().filter_map(|e| e.ok()).any(|e| {
            e.file_name().to_string_lossy().starts_with("webpush.json.bad-")
        });
        assert!(moved, "bad file moved aside");
    }

    #[tokio::test]
    async fn concurrent_persists_leave_the_final_set() {
        let dir = tempfile::tempdir().unwrap();
        let wp = start(dir.path(), default_writer(), generate_signing_key().unwrap());
        let mut tasks = Vec::new();
        for n in 0..50u32 {
            let w = wp.clone();
            tasks.push(tokio::spawn(async move {
                if n % 10 == 0 {
                    let _ = w.add_sub(sub(n)).await;
                } else {
                    let _ = w.persist().await;
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        wp.persist().await.unwrap();
        let f: SubFile = serde_json::from_slice(&fs::read(dir.path().join(WEBPUSH_FILE)).unwrap()).unwrap();
        let mem = wp.lock_subs().subs.clone();
        assert_eq!(f.subscriptions, mem);
        assert_eq!(mem.len(), 5);
    }

    #[tokio::test]
    async fn capacity_is_eight() {
        let dir = tempfile::tempdir().unwrap();
        let wp = start(dir.path(), default_writer(), generate_signing_key().unwrap());
        for n in 0..8 {
            assert!(wp.add_sub(sub(n)).await.unwrap().0.created);
        }
        assert_eq!(wp.add_sub(sub(8)).await.err(), Some(AddError::TooMany));
        assert!(!wp.add_sub(sub(3)).await.unwrap().0.created, "an existing endpoint still upserts");
    }
}
