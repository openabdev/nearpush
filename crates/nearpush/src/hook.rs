//! `nearpush hook`: turns Kiro CLI hook events into display states.
//!
//! Kiro runs the command with the hook event JSON on stdin (`hook_event_name`,
//! `cwd`, `session_id`, plus `prompt` for UserPromptSubmit and `tool_name` /
//! `tool_input` for the tool events). For SessionStart and UserPromptSubmit, Kiro adds
//! a successful hook's stdout to the agent's context, so this command never prints
//! to stdout. It always exits 0: a hook failure must not reach the agent.
//!
//! Mapping:
//! - UserPromptSubmit → `working`, starts the turn timer.
//! - PreToolUse → `working` with the tool name and a redacted summary. Kiro's hook
//!   input says nothing about whether the call needs approval, so `allow` is never
//!   guessed.
//! - Stop → `all_set` with "took Xm".
//! - SessionStart / PostToolUse → no event; SessionEnd removes the session's state.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{AgentState, Payload, now_ms};

/// Network budget for the whole push; the hook must never hold the agent up.
pub const PUSH_TIMEOUT: Duration = Duration::from_millis(900);
/// Bytes of stdin parsed; the rest is drained (PostToolUse can carry big results).
const MAX_INPUT_BYTES: usize = 1024 * 1024;
const PROVISIONAL_LABEL_CHARS: usize = 40;
const MAX_PROMPT_CHARS: usize = 2000;
const MAX_DETAIL_CHARS: usize = 80;
/// Repeated PreToolUse for the same tool within this window is not re-sent.
const TOOL_THROTTLE_MS: u64 = 5_000;
/// Session state files untouched this long are removed.
const STATE_TTL_MS: u64 = 7 * 24 * 3600 * 1000;

pub const SENDER: &str = "kiro";
pub const APP: &str = "Kiro";

/// True when `NEARPUSH_DISABLE` is set to anything but empty or `0`.
pub fn disabled() -> bool {
    std::env::var_os("NEARPUSH_DISABLE").is_some_and(|v| !v.is_empty() && v != "0")
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct HookInput {
    pub hook_event_name: String,
    pub cwd: Option<String>,
    pub session_id: Option<String>,
    pub conversation_id: Option<String>,
    pub prompt: Option<String>,
    pub tool_name: Option<String>,
    pub tool_input: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    SessionStart,
    PromptSubmit,
    PreTool,
    PostTool,
    Stop,
    SessionEnd,
    Other,
}

/// Accepts the V3 PascalCase names and the CLI 2.x camelCase ones.
pub fn kind(name: &str) -> Kind {
    match name.to_ascii_lowercase().replace(['_', '-'], "").as_str() {
        "sessionstart" | "agentspawn" => Kind::SessionStart,
        "userpromptsubmit" | "promptsubmit" => Kind::PromptSubmit,
        "pretooluse" => Kind::PreTool,
        "posttooluse" => Kind::PostTool,
        "stop" | "agentstop" => Kind::Stop,
        "sessionend" => Kind::SessionEnd,
        _ => Kind::Other,
    }
}

/// Per-session state kept between hook runs.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionState {
    /// Start of the current turn (unix ms).
    pub started_at: Option<u64>,
    /// Provisional label: the session's first prompt, truncated.
    pub label: Option<String>,
    pub prompts: u32,
    pub last_tool: Option<String>,
    pub last_push_ms: u64,
}

/// Facts about the environment, gathered once per run.
#[derive(Debug, Default, Clone)]
pub struct Context {
    pub now: u64,
    pub host: Option<String>,
    pub branch: Option<String>,
    /// `USER_PROMPT`, which Kiro also sets for UserPromptSubmit.
    pub env_prompt: Option<String>,
    /// Used when the input has no session id.
    pub fallback_session: Option<String>,
}

/// Session id: `session_id`, then `conversation_id`, then the caller's fallback
/// (`KIRO_SESSION_ID`, `TERM_SESSION_ID`), then a hash of the cwd.
pub fn session_id(input: &HookInput, ctx: &Context) -> String {
    [&input.session_id, &input.conversation_id, &ctx.fallback_session]
        .into_iter()
        .flatten()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("cwd-{:08x}", fnv64(input.cwd.as_deref().unwrap_or("")) as u32))
}

fn fnv64(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.truncate(out.trim_end().len());
    out.push('…');
    out
}

fn first_line(s: &str) -> &str {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

pub fn provisional_label(prompt: &str) -> Option<String> {
    let l = first_line(prompt);
    (!l.is_empty()).then(|| truncate(l, PROVISIONAL_LABEL_CHARS))
}

pub fn project(cwd: Option<&str>, branch: Option<&str>) -> Option<String> {
    let base = Path::new(cwd?).file_name()?.to_str()?.to_string();
    Some(match branch {
        Some(b) if !b.is_empty() => format!("{base} / {b}"),
        _ => base,
    })
}

pub fn fmt_duration(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        return format!("{s}s");
    }
    let m = s / 60;
    if m < 60 {
        return format!("{m}m");
    }
    let h = m / 60;
    if m % 60 == 0 { format!("{h}h") } else { format!("{h}h {}m", m % 60) }
}

fn looks_secret(word: &str) -> bool {
    let w = word.trim_matches(|c| matches!(c, '"' | '\'' | '`' | '(' | ')' | ',' | ';'));
    const PREFIXES: &[&str] =
        &["ghp_", "gho_", "ghs_", "ghu_", "github_pat_", "sk-", "xox", "AKIA", "ASIA", "eyJ", "tskey-", "glpat-", "npm_"];
    if PREFIXES.iter().any(|p| w.starts_with(p)) && w.len() > 8 {
        return true;
    }
    // Long opaque strings: tokens, hashes, base64 blobs.
    w.len() >= 24
        && w.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-'))
        && w.chars().any(|c| c.is_ascii_digit())
        && w.chars().any(|c| c.is_ascii_alphabetic())
        && !w.contains('/')
}

/// Hides env assignments (`KEY=value` → `KEY=…`), values after secret-looking
/// flags, `Bearer` credentials, URL userinfo and token-shaped words.
pub fn redact(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut hide_next = false;
    for word in text.split_whitespace() {
        let lower = word.to_ascii_lowercase();
        if hide_next {
            // `Authorization: Bearer <token>`: keep the scheme, hide what follows it.
            if matches!(lower.as_str(), "bearer" | "basic" | "token") {
                out.push(word.to_string());
            } else {
                out.push("…".into());
                hide_next = false;
            }
            continue;
        }
        let flag = lower.trim_start_matches('-');
        let secret_flag = word.starts_with('-')
            && ["token", "secret", "password", "passwd", "key", "auth", "credential"].iter().any(|k| flag.contains(k));
        if let Some((name, _)) = word.split_once('=') {
            let env_like = !name.is_empty()
                && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
            if env_like || secret_flag {
                out.push(format!("{name}=…"));
                continue;
            }
        }
        if secret_flag || lower == "bearer" || lower == "authorization:" {
            hide_next = true;
            out.push(word.to_string());
            continue;
        }
        if let Some(rest) = word.find("://").map(|i| &word[i + 3..]) {
            if let Some(at) = rest.find('@').filter(|&at| !rest[..at].contains('/')) {
                let scheme_end = word.len() - rest.len();
                out.push(format!("{}…@{}", &word[..scheme_end], &rest[at + 1..]));
                continue;
            }
        }
        if looks_secret(word) {
            out.push("…".into());
            continue;
        }
        out.push(word.to_string());
    }
    out.join(" ")
}

/// Last two components of a path, e.g. `src/main.rs`.
fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p.trim_end_matches('/').rsplit('/').take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

/// "tool: summary" with only whitelisted input fields, redacted and truncated.
pub fn tool_detail(tool: &str, input: Option<&Value>) -> String {
    let field = |k: &str| input.and_then(|v| v.get(k)).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let summary = if let Some(cmd) = field("command") {
        Some(redact(first_line(cmd)))
    } else if let Some(p) = field("path").or_else(|| field("file_path")) {
        Some(short_path(p))
    } else if let Some(u) = field("url") {
        Some(redact(u).split(['?', '#']).next().unwrap_or("").to_string())
    } else {
        field("query").or_else(|| field("pattern")).map(|q| redact(first_line(q)))
    };
    let s = match summary {
        Some(s) if !s.is_empty() => format!("{tool}: {s}"),
        _ => tool.to_string(),
    };
    truncate(&s, MAX_DETAIL_CHARS)
}

/// Updates `st` for `input` and returns the event to push, if any.
pub fn plan(input: &HookInput, st: &mut SessionState, ctx: &Context) -> Option<Payload> {
    let k = kind(&input.hook_event_name);
    let base = |state: AgentState, title: &str, st: &SessionState| Payload {
        title: title.into(),
        state: Some(state),
        sender: Some(SENDER.into()),
        app: Some(APP.into()),
        project: project(input.cwd.as_deref(), ctx.branch.as_deref()),
        session: Some(session_id(input, ctx)),
        session_label: st.label.clone(),
        host: ctx.host.clone(),
        started_at: st.started_at,
        ..Default::default()
    };
    match k {
        Kind::PromptSubmit => {
            let prompt = input.prompt.clone().or_else(|| ctx.env_prompt.clone()).unwrap_or_default();
            st.prompts += 1;
            if st.label.is_none() {
                st.label = provisional_label(&prompt);
            }
            st.started_at = Some(ctx.now);
            st.last_tool = None;
            st.last_push_ms = ctx.now;
            let mut p = base(AgentState::Working, "working.", st);
            let prompt = prompt.trim();
            if !prompt.is_empty() {
                p.prompt = Some(prompt.chars().take(MAX_PROMPT_CHARS).collect());
            }
            Some(p)
        }
        Kind::PreTool => {
            let tool = input.tool_name.as_deref().map(str::trim).filter(|t| !t.is_empty())?;
            let same = st.last_tool.as_deref() == Some(tool);
            if same && ctx.now.saturating_sub(st.last_push_ms) < TOOL_THROTTLE_MS {
                return None;
            }
            if st.started_at.is_none() {
                st.started_at = Some(ctx.now);
            }
            st.last_tool = Some(tool.to_string());
            st.last_push_ms = ctx.now;
            let mut p = base(AgentState::Working, "working.", st);
            p.detail = Some(tool_detail(tool, input.tool_input.as_ref()));
            Some(p)
        }
        Kind::Stop => {
            let mut p = base(AgentState::AllSet, "all set.", st);
            if let Some(start) = st.started_at {
                let d = ctx.now.saturating_sub(start);
                p.duration_ms = Some(d);
                p.detail = Some(format!("took {}", fmt_duration(d)));
            }
            st.started_at = None;
            st.last_tool = None;
            st.last_push_ms = ctx.now;
            Some(p)
        }
        Kind::SessionStart | Kind::PostTool | Kind::SessionEnd | Kind::Other => None,
    }
}

// ---- state files ----------------------------------------------------------------

pub fn state_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("NEARPUSH_STATE_DIR") {
        return Some(PathBuf::from(d));
    }
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".local/state/nearpush"))
}

/// `<state>/hooks/<safe-id>-<hash>.json`; the id is sanitized so it cannot form a path.
pub fn state_file(dir: &Path, session: &str) -> PathBuf {
    let safe: String =
        session.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(48).collect();
    dir.join("hooks").join(format!("{safe}-{:016x}.json", fnv64(session)))
}

fn private_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

pub fn load_state(path: &Path) -> SessionState {
    fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_state(path: &Path, st: &SessionState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        private_dir(parent.parent().unwrap_or(parent))?;
        private_dir(parent)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        f.write_all(&serde_json::to_vec(st).map_err(std::io::Error::other)?)?;
    }
    fs::rename(&tmp, path)
}

fn prune(hooks_dir: &Path, now: u64) {
    let Ok(rd) = fs::read_dir(hooks_dir) else { return };
    for entry in rd.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|t| now.saturating_sub(t.as_millis() as u64) > STATE_TTL_MS);
        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

// ---- environment ------------------------------------------------------------------

/// Branch from `.git/HEAD` (walking up from `cwd`; worktree `.git` files followed).
/// Reads at most a few small files, no subprocess, so it stays well under 50 ms.
pub fn git_branch(cwd: &Path) -> Option<String> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let dotgit = d.join(".git");
        let head = if dotgit.is_dir() {
            fs::read_to_string(dotgit.join("HEAD")).ok()
        } else if dotgit.is_file() {
            let text = fs::read_to_string(&dotgit).ok()?;
            let gitdir = text.trim().strip_prefix("gitdir:")?.trim();
            let gitdir = if Path::new(gitdir).is_absolute() { PathBuf::from(gitdir) } else { d.join(gitdir) };
            fs::read_to_string(gitdir.join("HEAD")).ok()
        } else {
            None
        };
        if let Some(head) = head {
            let head = head.trim();
            return Some(match head.strip_prefix("ref: refs/heads/") {
                Some(b) => b.to_string(),
                None => head.chars().take(7).collect(),
            });
        }
        dir = d.parent();
    }
    None
}

/// The machine's name: macOS's LocalHostName (gethostname often reports just "Mac"),
/// else the first label of gethostname.
pub fn hostname() -> Option<String> {
    if cfg!(target_os = "macos") {
        if let Ok(out) = std::process::Command::new("/usr/sbin/scutil")
            .args(["--get", "LocalHostName"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
        {
            let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if out.status.success() && !name.is_empty() {
                return Some(name);
            }
        }
    }
    unsafe extern "C" {
        fn gethostname(name: *mut std::ffi::c_char, len: usize) -> std::ffi::c_int;
    }
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for `len` bytes; gethostname NUL-terminates on success
    // when the name fits, and the final byte is reserved so a NUL is always present.
    let rc = unsafe { gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]);
    let short = name.split('.').next().unwrap_or("").trim().to_string();
    (!short.is_empty()).then_some(short)
}

fn read_input() -> Option<HookInput> {
    let mut stdin = std::io::stdin().lock();
    let mut buf = Vec::new();
    (&mut stdin).take(MAX_INPUT_BYTES as u64).read_to_end(&mut buf).ok()?;
    // Drain the rest so the writer never sees a broken pipe.
    let _ = std::io::copy(&mut stdin, &mut std::io::sink());
    match serde_json::from_slice::<HookInput>(&buf) {
        Ok(v) => Some(v),
        Err(_) if buf.len() == MAX_INPUT_BYTES => {
            // Truncated (huge tool_response): the event name and ids come first in
            // practice, so fall back to a lenient scan of the leading fields.
            let head = String::from_utf8_lossy(&buf[..buf.len().min(64 * 1024)]).into_owned();
            let get = |k: &str| -> Option<String> {
                let i = head.find(&format!("\"{k}\""))?;
                let rest = &head[i + k.len() + 2..];
                let rest = rest.trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
                Some(rest[..rest.find('"')?].to_string())
            };
            Some(HookInput {
                hook_event_name: get("hook_event_name")?,
                cwd: get("cwd"),
                session_id: get("session_id"),
                tool_name: get("tool_name"),
                ..Default::default()
            })
        }
        Err(_) => None,
    }
}

/// Entry point. Never panics outward, never prints to stdout, always returns.
pub async fn run(addr: &str, token_file: Option<PathBuf>) {
    if disabled() {
        return;
    }
    let Some(input) = read_input() else { return };
    let cwd = input.cwd.clone().or_else(|| std::env::current_dir().ok().map(|p| p.display().to_string()));
    let input = HookInput { cwd, ..input };
    let k = kind(&input.hook_event_name);
    if matches!(k, Kind::Other | Kind::PostTool) {
        return;
    }
    let ctx = Context {
        now: now_ms(),
        host: hostname(),
        branch: input.cwd.as_deref().and_then(|c| git_branch(Path::new(c))),
        env_prompt: std::env::var("USER_PROMPT").ok(),
        // Kiro exports KIRO_SESSION_ID to the processes it starts.
        fallback_session: ["KIRO_SESSION_ID", "TERM_SESSION_ID"]
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty())),
    };
    let Some(dir) = state_dir() else { return };
    let path = state_file(&dir, &session_id(&input, &ctx));
    if k == Kind::SessionEnd {
        let _ = fs::remove_file(&path);
        return;
    }
    if matches!(k, Kind::SessionStart | Kind::PromptSubmit) {
        prune(&dir.join("hooks"), ctx.now);
    }
    let mut st = load_state(&path);
    let payload = plan(&input, &mut st, &ctx);
    if let Err(e) = save_state(&path, &st) {
        eprintln!("nearpush hook: cannot save {}: {e}", path.display());
    }
    let Some(payload) = payload else { return };
    let token_file = match token_file.map(Ok).unwrap_or_else(super::default_token_file) {
        Ok(p) => p,
        Err(_) => return,
    };
    let Ok(token) = super::read_token(&token_file) else { return };
    if let Err(e) = super::send_with_timeout(addr, &token, &payload, PUSH_TIMEOUT).await {
        eprintln!("nearpush hook: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input(v: Value) -> HookInput {
        serde_json::from_value(v).unwrap()
    }

    fn ctx(now: u64) -> Context {
        Context { now, host: Some("laptop".into()), branch: Some("main".into()), ..Default::default() }
    }

    #[test]
    fn parses_documented_events() {
        let i = input(json!({
            "hook_event_name": "PreToolUse", "cwd": "/Users/p/repo/nearpush", "session_id": "abc123-def456-789",
            "tool_name": "execute_bash", "tool_input": { "command": "cargo test" }, "unknown_future_field": 1
        }));
        assert_eq!(kind(&i.hook_event_name), Kind::PreTool);
        assert_eq!(i.session_id.as_deref(), Some("abc123-def456-789"));
        for (n, k) in [
            ("agentSpawn", Kind::SessionStart), ("SessionStart", Kind::SessionStart),
            ("userPromptSubmit", Kind::PromptSubmit), ("UserPromptSubmit", Kind::PromptSubmit),
            ("preToolUse", Kind::PreTool), ("postToolUse", Kind::PostTool), ("PostToolUse", Kind::PostTool),
            ("stop", Kind::Stop), ("Stop", Kind::Stop), ("SessionEnd", Kind::SessionEnd), ("PostFileSave", Kind::Other),
        ] {
            assert_eq!(kind(n), k, "{n}");
        }
        assert!(serde_json::from_str::<HookInput>("{}").is_ok());
    }

    #[test]
    fn prompt_then_stop_gives_working_then_all_set_with_duration() {
        let mut st = SessionState::default();
        let p = plan(
            &input(json!({ "hook_event_name": "userPromptSubmit", "cwd": "/r/nearpush", "session_id": "s-1",
                "prompt": "Add a hook subcommand to nearpush\nand tests" })),
            &mut st,
            &ctx(1_000_000),
        )
        .unwrap();
        assert_eq!(p.state, Some(AgentState::Working));
        assert_eq!(p.title, "working.");
        assert_eq!(p.sender.as_deref(), Some("kiro"));
        assert_eq!(p.app.as_deref(), Some("Kiro"));
        assert_eq!(p.project.as_deref(), Some("nearpush / main"));
        assert_eq!(p.session.as_deref(), Some("s-1"));
        assert_eq!(p.session_label.as_deref(), Some("Add a hook subcommand to nearpush"));
        assert_eq!(p.host.as_deref(), Some("laptop"));
        assert_eq!(p.started_at, Some(1_000_000));
        assert!(p.prompt.as_deref().unwrap().contains("and tests"));

        let p = plan(&input(json!({ "hook_event_name": "stop", "cwd": "/r/nearpush", "session_id": "s-1" })), &mut st, &ctx(1_000_000 + 18 * 60_000 + 5_000))
            .unwrap();
        assert_eq!(p.state, Some(AgentState::AllSet));
        assert_eq!(p.title, "all set.");
        assert_eq!(p.duration_ms, Some(18 * 60_000 + 5_000));
        assert_eq!(p.detail.as_deref(), Some("took 18m"));
        assert!(p.prompt.is_none());
        assert_eq!(st.started_at, None);
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["state"], "all_set");
        assert_eq!(json["sender"], "kiro");
    }

    #[test]
    fn pre_tool_use_is_working_never_allow() {
        let mut st = SessionState::default();
        let ev = |tool: &str, inp: Value| {
            input(json!({ "hook_event_name": "preToolUse", "cwd": "/r/x", "session_id": "s", "tool_name": tool, "tool_input": inp }))
        };
        let p = plan(&ev("execute_bash", json!({ "command": "GITHUB_TOKEN=ghp_abcdefghijklmnop1234 gh pr list" })), &mut st, &ctx(10)).unwrap();
        assert_eq!(p.state, Some(AgentState::Working));
        assert_eq!(p.detail.as_deref(), Some("execute_bash: GITHUB_TOKEN=… gh pr list"));
        // Same tool again inside the throttle window: nothing sent.
        assert!(plan(&ev("execute_bash", json!({ "command": "ls" })), &mut st, &ctx(1000)).is_none());
        // A different tool is sent at once.
        let p = plan(&ev("read_file", json!({ "path": "/Users/p/repo/x/src/main.rs" })), &mut st, &ctx(1500)).unwrap();
        assert_eq!(p.detail.as_deref(), Some("read_file: src/main.rs"));
        let p = plan(&ev("mcp_postgres_query", json!({ "sql": "SELECT secret FROM t" })), &mut st, &ctx(2000)).unwrap();
        assert_eq!(p.detail.as_deref(), Some("mcp_postgres_query"), "non-whitelisted input fields are left out");
        assert!(plan(&input(json!({ "hook_event_name": "postToolUse", "tool_name": "x" })), &mut st, &ctx(3000)).is_none());
        assert!(plan(&input(json!({ "hook_event_name": "agentSpawn" })), &mut st, &ctx(3000)).is_none());
    }

    #[test]
    fn redaction() {
        assert_eq!(redact("curl -H Authorization: Bearer abc.def https://u:pw@host/x"), "curl -H Authorization: Bearer … https://…@host/x");
        assert_eq!(redact("deploy --token s3cr3t --name app"), "deploy --token … --name app");
        assert_eq!(redact("run --api-key=xyz"), "run --api-key=…");
        assert_eq!(redact("AWS_SECRET_ACCESS_KEY=abc aws s3 ls"), "AWS_SECRET_ACCESS_KEY=… aws s3 ls");
        assert_eq!(redact("echo 0123456789abcdef0123456789abcdef"), "echo …");
        assert_eq!(redact("git commit -m fix"), "git commit -m fix");
        assert_eq!(redact("cat src/very/long/path/to/some_file_name_2.rs"), "cat src/very/long/path/to/some_file_name_2.rs");
    }

    #[test]
    fn two_interleaved_sessions_do_not_mix() {
        let dir = tempfile::tempdir().unwrap();
        let run = |v: Value, now: u64| {
            let i = input(v);
            let c = ctx(now);
            let path = state_file(dir.path(), &session_id(&i, &c));
            let mut st = load_state(&path);
            let p = plan(&i, &mut st, &c);
            save_state(&path, &st).unwrap();
            p
        };
        let a = |e: &str| json!({ "hook_event_name": e, "cwd": "/r/alpha", "session_id": "aaaa-1111", "prompt": "修正登入轉址的問題，然後補測試", "tool_name": "execute_bash", "tool_input": { "command": "cargo test" } });
        let b = |e: &str| json!({ "hook_event_name": e, "cwd": "/r/beta", "session_id": "bbbb-2222", "prompt": "Write the release notes", "tool_name": "read_file", "tool_input": { "path": "/r/beta/CHANGELOG.md" } });

        let pa = run(a("userPromptSubmit"), 0).unwrap();
        let pb = run(b("userPromptSubmit"), 60_000).unwrap();
        let ta = run(a("preToolUse"), 120_000).unwrap();
        let tb = run(b("preToolUse"), 130_000).unwrap();
        let sb = run(b("stop"), 180_000).unwrap();
        let sa = run(a("stop"), 300_000).unwrap();

        for p in [&pa, &ta, &sa] {
            assert_eq!(p.session.as_deref(), Some("aaaa-1111"));
            assert_eq!(p.project.as_deref(), Some("alpha / main"));
            assert_eq!(p.session_label.as_deref(), Some("修正登入轉址的問題，然後補測試"));
            assert_eq!(p.app.as_deref(), Some("Kiro"));
        }
        for p in [&pb, &tb, &sb] {
            assert_eq!(p.session.as_deref(), Some("bbbb-2222"));
            assert_eq!(p.project.as_deref(), Some("beta / main"));
            assert_eq!(p.session_label.as_deref(), Some("Write the release notes"));
        }
        assert_eq!(ta.detail.as_deref(), Some("execute_bash: cargo test"));
        assert_eq!(tb.detail.as_deref(), Some("read_file: beta/CHANGELOG.md"));
        assert_eq!((sa.duration_ms, sa.detail.as_deref()), (Some(300_000), Some("took 5m")));
        assert_eq!((sb.duration_ms, sb.detail.as_deref()), (Some(120_000), Some("took 2m")));

        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let fa = state_file(dir.path(), "aaaa-1111");
        assert_eq!(mode(&fa), 0o600);
        assert_eq!(mode(&dir.path().join("hooks")), 0o700);
        assert_ne!(fa, state_file(dir.path(), "bbbb-2222"));
    }

    #[test]
    fn state_file_names_are_safe() {
        let d = Path::new("/s");
        let f = state_file(d, "../../etc/passwd");
        assert_eq!(f.parent().unwrap(), Path::new("/s/hooks"));
        assert!(f.file_name().unwrap().to_str().unwrap().starts_with("etcpasswd-"));
    }

    #[test]
    fn session_fallbacks() {
        let c = Context { fallback_session: Some("term-1".into()), ..Default::default() };
        assert_eq!(session_id(&input(json!({ "hook_event_name": "stop", "conversation_id": "c9" })), &c), "c9");
        assert_eq!(session_id(&input(json!({ "hook_event_name": "stop" })), &c), "term-1");
        let a = session_id(&input(json!({ "hook_event_name": "stop", "cwd": "/a" })), &Context::default());
        assert!(a.starts_with("cwd-"));
        assert_eq!(a, session_id(&input(json!({ "hook_event_name": "stop", "cwd": "/a" })), &Context::default()));
    }

    #[test]
    fn git_branch_reads_head() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/feat/x\n").unwrap();
        fs::create_dir_all(dir.path().join("sub/deeper")).unwrap();
        assert_eq!(git_branch(&dir.path().join("sub/deeper")).as_deref(), Some("feat/x"));
        let wt = tempfile::tempdir().unwrap();
        fs::write(wt.path().join(".git"), format!("gitdir: {}\n", dir.path().join(".git").display())).unwrap();
        assert_eq!(git_branch(wt.path()).as_deref(), Some("feat/x"));
    }

    #[test]
    fn recursion_guard_env() {
        // SAFETY: tests in this module that read NEARPUSH_DISABLE run only here.
        unsafe { std::env::set_var("NEARPUSH_DISABLE", "1") };
        assert!(disabled());
        unsafe { std::env::set_var("NEARPUSH_DISABLE", "0") };
        assert!(!disabled());
        unsafe { std::env::remove_var("NEARPUSH_DISABLE") };
        assert!(!disabled());
    }
}
