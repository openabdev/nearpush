//! `nearpush hook`: turns Kiro CLI hook events into display states.
//!
//! Kiro runs the command with the hook event JSON on stdin (`hook_event_name`,
//! `cwd`, `session_id`, plus `prompt` for UserPromptSubmit and `tool_name` /
//! `tool_input` for the tool events). For SessionStart and UserPromptSubmit, Kiro adds
//! a successful hook's stdout to the agent's context, so this command never prints
//! to stdout. It always exits 0: a hook failure must not reach the agent.
//!
//! Mapping:
//! - UserPromptSubmit → `working`, starts the turn timer. The daemon's default
//!   display filter hides `working`, but the event is still stored and the timer
//!   still feeds "took Xm" at Stop.
//! - PreToolUse → `working` with the tool name and a redacted summary. Kiro's hook
//!   input says nothing about whether the call needs approval, so `allow` is never
//!   guessed. Only useful when `working` is enabled in `[filter] states`.
//! - Stop → `your_turn` when the turn ended waiting on the user (see
//!   [`classify_turn_end`]); otherwise `all_set` with "took Xm". The session's
//!   waiting workflow count rides along as a separate `workflows_waiting` field; it
//!   never changes the verdict or the detail, and the daemon shows it only as an FYI
//!   footer on a final `all_set` card. The reply text comes from a stdin field if
//!   Kiro ever sends one, else from the session transcript (see [`crate::kiro`]).
//! - SessionStart / PostToolUse → no event. SessionEnd removes the session's state and
//!   sends `session_end`, so the daemon stops showing the session as waiting on the user.
//!
//! Workflow-step / sub-agent sessions (per their `session.json`) push nothing
//! unless `[hook] include_subagents = true` in config.toml.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use nearpush_common::text::{MAX_DETAIL_CHARS, classifier_tail, classify_turn_end, first_line, fmt_duration, redact, truncate};

use super::{AgentState, Payload, TurnClassify, now_ms};

#[cfg(test)]
use nearpush_common::text::{CLASSIFIER_TAIL_CHARS, strip_quoted};

/// Network budget for the whole push; the hook must never hold the agent up.
pub const PUSH_TIMEOUT: Duration = Duration::from_millis(900);
/// Bytes of stdin parsed; the rest is drained (PostToolUse can carry big results).
const MAX_INPUT_BYTES: usize = 1024 * 1024;
const PROVISIONAL_LABEL_CHARS: usize = 40;
const MAX_PROMPT_CHARS: usize = 2000;
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
    /// Not sent by Kiro today (Kiro#6258); read first if a future version adds one.
    pub response: Option<String>,
    pub assistant_response: Option<String>,
    pub last_assistant_message: Option<String>,
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
    /// Stop only: the turn's final reply text.
    pub final_text: Option<String>,
    /// Stop only: this session's waiting workflow runs. Sent as data for the daemon's
    /// FYI footer; never changes the verdict or the detail.
    pub workflows_waiting: u32,
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
            p.turn_start = Some(true);
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
            let took = st.started_at.map(|start| ctx.now.saturating_sub(start));
            let question = ctx.final_text.as_deref().and_then(classify_turn_end);
            let classify_line = question.clone();
            let mut p = match question {
                Some(q) => {
                    let mut p = base(AgentState::YourTurn, "your turn.", st);
                    p.detail = Some(q);
                    p
                }
                None => {
                    let mut p = base(AgentState::AllSet, "all set.", st);
                    p.detail = took.map(|d| format!("took {}", fmt_duration(d)));
                    p
                }
            };
            p.duration_ms = took;
            // Sent whatever the rules verdict: the daemon's final verdict decides
            // whether the footer shows.
            p.workflows_waiting = Some(ctx.workflows_waiting).filter(|&n| n > 0);
            if let Some(reply_tail) = ctx.final_text.as_deref().and_then(classifier_tail) {
                p.classify = Some(TurnClassify {
                    reply_tail,
                    question_line: classify_line,
                    took: took.map(|d| format!("took {}", fmt_duration(d))),
                });
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

/// The `[hook]` section of config.toml, read leniently: unknown keys and other
/// sections are ignored, and any error yields the defaults.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct HookConfig {
    pub include_subagents: bool,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct HookConfigFile {
    hook: HookConfig,
}

pub fn parse_hook_config(text: &str) -> HookConfig {
    toml::from_str::<HookConfigFile>(text).map(|f| f.hook).unwrap_or_default()
}

/// `$NEARPUSH_CONFIG_DIR/config.toml`, else `~/.config/nearpush/config.toml`.
fn hook_config() -> HookConfig {
    let path = match std::env::var_os("NEARPUSH_CONFIG_DIR") {
        Some(d) => PathBuf::from(d).join("config.toml"),
        None => match std::env::var_os("HOME") {
            Some(h) => PathBuf::from(h).join(".config/nearpush/config.toml"),
            None => return HookConfig::default(),
        },
    };
    fs::read_to_string(path).map(|t| parse_hook_config(&t)).unwrap_or_default()
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
        ..Default::default()
    };
    let Some(dir) = state_dir() else { return };
    let session = session_id(&input, &ctx);
    let kiro_dir = crate::kiro::sessions_root().and_then(|root| crate::kiro::find_session_dir(&root, &session));
    let subagent = kiro_dir
        .as_deref()
        .and_then(crate::kiro::read_session_json)
        .is_some_and(|s| crate::kiro::is_subagent(&s, &session));
    if subagent && !hook_config().include_subagents {
        return;
    }
    let path = state_file(&dir, &session);
    if k == Kind::SessionEnd {
        let _ = fs::remove_file(&path);
        send_to_daemon(addr, token_file, &session_end_payload(&session)).await;
        return;
    }
    let mut ctx = ctx;
    if k == Kind::Stop {
        ctx.final_text = [&input.response, &input.assistant_response, &input.last_assistant_message]
            .into_iter()
            .flatten()
            .find(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| kiro_dir.as_deref().and_then(|d| crate::kiro::last_assistant_text(&d.join("messages.jsonl"))));
        let deadline = std::time::Instant::now() + crate::kiro::WORKFLOW_SCAN_BUDGET;
        ctx.workflows_waiting = kiro_dir
            .as_deref()
            .and_then(Path::parent)
            .and_then(|ws| crate::kiro::workflows_waiting(&ws.join("workflows"), &session, std::time::SystemTime::now(), deadline))
            .unwrap_or(0);
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
    send_to_daemon(addr, token_file, &payload).await;
}

/// SessionEnd payload: the daemon clears the session's pending state and stores nothing.
fn session_end_payload(session: &str) -> Payload {
    Payload { title: "session ended".into(), session: Some(session.into()), session_end: Some(true), ..Default::default() }
}

/// Sends `payload` to the daemon with the token from `token_file` (or the default). Never fails loudly.
async fn send_to_daemon(addr: &str, token_file: Option<PathBuf>, payload: &Payload) {
    let token_file = match token_file.map(Ok).unwrap_or_else(super::default_token_file) {
        Ok(p) => p,
        Err(_) => return,
    };
    let Ok(token) = super::read_token(&token_file) else { return };
    if let Err(e) = super::send_with_timeout(addr, &token, payload, PUSH_TIMEOUT).await {
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
            "hook_event_name": "PreToolUse", "cwd": "/Users/example/repo/nearpush", "session_id": "abc123-def456-789",
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
        let p = plan(&ev("execute_bash", json!({ "command": "GITHUB_TOKEN=ghp_EXAMPLE_NOT_A_TOKEN gh pr list" })), &mut st, &ctx(10)).unwrap();
        assert_eq!(p.state, Some(AgentState::Working));
        assert_eq!(p.detail.as_deref(), Some("execute_bash: GITHUB_TOKEN=… gh pr list"));
        // Same tool again inside the throttle window: nothing sent.
        assert!(plan(&ev("execute_bash", json!({ "command": "ls" })), &mut st, &ctx(1000)).is_none());
        // A different tool is sent at once.
        let p = plan(&ev("read_file", json!({ "path": "/Users/example/repo/x/src/main.rs" })), &mut st, &ctx(1500)).unwrap();
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

    fn your_turn(text: &str) -> String {
        classify_turn_end(text).unwrap_or_else(|| panic!("expected your_turn: {text:?}"))
    }

    fn all_set(text: &str) {
        assert_eq!(classify_turn_end(text), None, "expected all_set: {text:?}");
    }

    #[test]
    fn turn_end_fixture_samples_hold_on_rules_path() {
        let samples: Vec<Value> =
            serde_json::from_str(include_str!("../tests/fixtures/turn_end_samples.json")).expect("fixture parses");
        assert!(samples.len() >= 15, "fixture shrank to {}", samples.len());
        for s in &samples {
            let text = s["text"].as_str().expect("text");
            match s["expected"].as_str().expect("expected") {
                "your_turn" => {
                    your_turn(text);
                }
                "all_set" => all_set(text),
                other => panic!("unknown expected {other:?}"),
            }
        }
    }

    #[test]
    fn classifier_future_plan_is_all_set() {
        all_set("全部換完後會再停一次，跟你要 node-a 的 admin credential 來驗證。");
        all_set("Once everything is done I will need your admin credential.");
        all_set("Verify the smoke test later. I will need your admin credential after that is done.");
        assert_eq!(your_turn("全部完成後會再停一次。\n要不要現在就 ship？"), "要不要現在就 ship？");
    }

    #[test]
    fn classifier_same_line_blocking_question_after_future_marker_is_your_turn() {
        let q = "全部完成後會再停一次，要不要現在就 ship？";
        assert_eq!(your_turn(q), q);
        assert_eq!(your_turn(&format!("{q}\n我等你回覆。")), q);
    }

    #[test]
    fn classifier_choice_with_future_option_is_your_turn() {
        assert_eq!(your_turn("Choose one:\n1. Ship now\n2. Ship later"), "Choose one:");
        assert_eq!(your_turn("請選一個：\n1. 現在 ship\n2. 之後再 ship"), "請選一個：");
    }

    #[test]
    fn classifier_generic_after_plan_is_all_set() {
        all_set("Restarts are done. Once that is done I will ask: do you want to ship.");
        all_set("全部完成後，我會問你要不要 ship。");
    }

    #[test]
    fn classifier_future_clause_question_mark_is_your_turn() {
        // A line ending in a question mark is a question the user answers. The
        // question-mark rule covers it before the future-marker check, as before.
        assert_eq!(your_turn("之後要不要 ship？"), "之後要不要 ship？");
    }

    #[test]
    fn classifier_conditional_offer_is_all_set() {
        all_set("已改好：hook 不再暫停。\n\n如果你想現在就清掉，我再做一次短暫的解除暫停測試。");
        all_set("Done. If you'd like, I can also clean up the stash afterwards.");
        all_set("Done. If you want, let me know and I'll rerun the tests.");
    }

    #[test]
    fn classifier_choice_with_waiting_statement_is_your_turn() {
        assert_eq!(your_turn("兩個方案：\n1. 用 SMAppService\n2. 用子程序\n請選一個，我等你選擇後再繼續。"), "兩個方案：");
        assert_eq!(your_turn("Two options:\n1. Use SMAppService\n2. Use a subprocess\nI'll wait for your choice."), "Two options:");
    }

    #[test]
    fn classifier_plain_summary_and_closing_question() {
        all_set("測試通過，已 commit。\n總結如上。");
        all_set("All tests pass and the change is committed.");
        assert_eq!(your_turn("測試通過，已 commit。\n要我 ship 嗎？"), "要我 ship 嗎？");
    }

    #[test]
    fn classifier_zh_tw() {
        assert_eq!(your_turn("已改好 hook。\n\n要不要我順便把 README 也更新？"), "要不要我順便把 README 也更新？");
        assert_eq!(your_turn("目前有兩個方案：\n1. 用 SMAppService\n2. 用子程序\n請選一個"), "目前有兩個方案：");
        assert_eq!(your_turn("這樣可以嗎"), "這樣可以嗎");
        assert_eq!(your_turn("你決定要用哪一個方案：\n1. A\n2. B"), "你決定要用哪一個方案：");
        assert_eq!(your_turn("**要繼續部署嗎？**"), "要繼續部署嗎？");
        all_set("已完成，測試全部通過。");
        all_set("變更：\n1. 新增 /status\n2. 新增測試");
        all_set("不需要我再做什麼了。");
        all_set("照你要我的方式改好了。");
        all_set("");
    }

    #[test]
    fn classifier_en() {
        assert_eq!(your_turn("Should I also bump the version?"), "Should I also bump the version?");
        assert_eq!(your_turn("Options:\n1. Keep it\n2. Remove it"), "Options:");
        assert_eq!(your_turn("Pushed the fix.\nLet me know which one you prefer."), "Let me know which one you prefer.");
        assert_eq!(your_turn("Your call — merge now or wait"), "Your call — merge now or wait");
        assert_eq!(your_turn("Which do you want?\n\nA) rebase\nB) merge"), "Which do you want?");
        assert_eq!(your_turn("- Proceed with the migration (y/n)?"), "Proceed with the migration (y/n)?");
        all_set("Done. All 65 tests pass.");
        all_set("Summary:\n1. Added X\n2. Fixed Y");
        all_set("Ran it:\n```sh\nread -p 'continue?' x\n```\nAll green.");
        all_set("The tests should include the edge case, and they do now.");
        all_set("Is it fast? Yes.\nIt finished in 2s.");
    }

    #[test]
    fn classifier_detail_redacted_truncated() {
        let q = your_turn(&format!("Should I push with GITHUB_TOKEN=ghp_EXAMPLE_NOT_A_TOKEN and then {}?", "word ".repeat(30)));
        assert!(q.contains("GITHUB_TOKEN=…"), "{q}");
        assert!(!q.contains("ghp_EXAMPLE_NOT_A_TOKEN"), "{q}");
        assert!(q.chars().count() <= MAX_DETAIL_CHARS, "{q}");
    }

    #[test]
    fn strip_quoted_removes_quoted_and_code_spans() {
        assert_eq!(strip_quoted("a「b「c」d」e"), "ae");
        assert_eq!(strip_quoted("x 'quoted' y"), "x  y");
        assert_eq!(strip_quoted("say “hi” now"), "say  now");
        assert_eq!(strip_quoted("`code` text"), " text");
        assert_eq!(strip_quoted("don't and I'm"), "don't and I'm");
        assert_eq!(strip_quoted("open 「 never closes"), "open 「 never closes");
    }

    #[test]
    fn rules_quoted_ask_phrase_zh_is_all_set() {
        all_set("前面那則被判成 your turn，原因是句中的「跟我說」命中了規則。\n這次只改了分類方式，其他行為不變。");
        all_set("『要不要』是引用的詞，不是在問你。\n已經處理完。");
    }

    #[test]
    fn rules_negated_yao_wo_zh_is_all_set() {
        all_set("這次不需要我再確認，已經自動完成。");
    }

    #[test]
    fn rules_question_mark_zh_stays_your_turn() {
        assert_eq!(your_turn("已改好。\n這個結果符合預期？"), "這個結果符合預期？");
    }

    #[test]
    fn rules_numbered_choice_zh_without_phrase_stays_your_turn() {
        assert_eq!(your_turn("目前有兩個方案：\n1. 用 SMAppService\n2. 用子程序"), "目前有兩個方案：");
    }

    #[test]
    fn rules_quoted_ask_phrase_en_is_all_set() {
        all_set("The earlier turn showed your turn because the phrase \"let me know\" matched.\nI changed the matcher to skip quoted text.");
    }

    #[test]
    fn rules_backtick_code_en_is_all_set() {
        all_set("The rule matched `should I` inside code.\nAll tests pass.");
    }

    #[test]
    fn rules_apostrophe_not_a_quote_en_stays_your_turn() {
        assert_eq!(
            your_turn("I don't think we need that, and it's fine.\nLet me know if you want more."),
            "Let me know if you want more."
        );
    }

    #[test]
    fn rules_phrase_middle_of_long_reply_is_all_set() {
        let mut long = String::from("如果你想要的話，可以跟我說細節。\n");
        for i in 1..=8 {
            long.push_str(&format!("第 {i} 點已完成。\n"));
        }
        all_set(&long);
        all_set("Let me know which part you want.\nThe change is small.\nAll tests pass.");
    }

    #[test]
    fn rules_unclosed_bracket_is_plain_text() {
        assert_eq!(your_turn("我還沒改完「跟我說這句\n目前沒有其他問題。"), "我還沒改完「跟我說這句");
    }

    #[test]
    fn rules_code_option_items_still_count_as_options() {
        assert_eq!(your_turn("目前有兩個方案：\n1. `SMAppService`\n2. `子程序`"), "目前有兩個方案：");
        assert_eq!(your_turn("Pick one:\n1. `SMAppService`\n2. \"subprocess\""), "Pick one:");
    }

    #[test]
    fn rules_options_early_in_reply_are_all_set() {
        all_set("請選一個方案：\n1. A\n2. B\n已完成改動。\n測試通過。");
        all_set("Which option do you want?\n1. A\n2. B\nI already applied the change.\nAll tests pass.");
    }

    #[test]
    fn rules_option_list_at_end_is_your_turn() {
        assert_eq!(your_turn("已列出方案。\n請選一個方案：\n1. A\n2. B"), "請選一個方案：");
        assert_eq!(your_turn("Which option?\n1. A\n2. B"), "Which option?");
    }

    #[test]
    fn session_end_payload_only_flags_the_session() {
        let v = serde_json::to_value(session_end_payload("s-9")).unwrap();
        assert_eq!((v["session"].as_str(), v["session_end"].as_bool()), (Some("s-9"), Some(true)));
        assert!(v.get("state").is_none() && v.get("turn_start").is_none());
    }

    fn stop_with(text: Option<&str>) -> Payload {
        stop_waiting(text, 0)
    }

    fn stop_waiting(text: Option<&str>, workflows_waiting: u32) -> Payload {
        let mut st = SessionState { started_at: Some(1_000), ..Default::default() };
        let c = Context { final_text: text.map(str::to_string), workflows_waiting, ..ctx(1_000 + 3 * 60_000) };
        plan(&input(json!({ "hook_event_name": "Stop", "cwd": "/r/x", "session_id": "s" })), &mut st, &c).unwrap()
    }

    #[test]
    fn stop_with_question_is_your_turn() {
        let p = stop_with(Some("I finished the refactor.\n要不要我也更新文件？"));
        assert_eq!(p.state, Some(AgentState::YourTurn));
        assert_eq!(p.title, "your turn.");
        assert_eq!(p.detail.as_deref(), Some("要不要我也更新文件？"));
        assert_eq!(p.duration_ms, Some(3 * 60_000), "duration kept");
        assert_eq!(serde_json::to_value(&p).unwrap()["state"], "your_turn");
    }

    #[test]
    fn stop_with_summary_is_all_set() {
        let p = stop_with(Some("Done. All tests pass."));
        assert_eq!(p.state, Some(AgentState::AllSet));
        assert_eq!(p.detail.as_deref(), Some("took 3m"));
        assert_eq!(p.duration_ms, Some(3 * 60_000));
        assert!(serde_json::to_value(&p).unwrap().get("workflows_waiting").is_none(), "no waiting workflow, no field");
    }

    #[test]
    fn waiting_workflows_are_sent_as_data_and_never_change_the_verdict() {
        for text in ["Started the workflow.", "Started the workflow.\n要不要我也更新文件？"] {
            let plain = stop_with(Some(text));
            let p = stop_waiting(Some(text), 2);
            assert_eq!((p.state, &p.title, &p.detail), (plain.state, &plain.title, &plain.detail), "verdict and detail unchanged: {text}");
            assert_eq!(p.classify.as_ref().map(|c| &c.reply_tail), plain.classify.as_ref().map(|c| &c.reply_tail));
            assert!(!p.detail.as_deref().unwrap_or_default().contains("wf"), "no count in the detail");
            let json = serde_json::to_value(&p).unwrap();
            assert_eq!(json["workflows_waiting"], 2);
            assert!(json.get("workflows_running").is_none(), "running count is never sent");
        }
        assert_eq!(stop_waiting(Some("Done."), 1).state, Some(AgentState::AllSet));
        assert_eq!(stop_waiting(Some("要不要我也更新文件？"), 1).state, Some(AgentState::YourTurn), "a waiting workflow never forces or blocks your_turn");
    }

    #[test]
    fn stop_your_turn_shows_question_line() {
        let q = "要不要我也更新文件？";
        let text = format!("Started the workflow.\n{q}");
        let p = stop_with(Some(text.as_str()));
        assert_eq!(p.state, Some(AgentState::YourTurn));
        assert_eq!(p.detail.as_deref(), Some(q));
    }

    #[test]
    fn classifier_tail_keeps_last_eight_lines_outside_fences() {
        let text = (0..10).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let tail = classifier_tail(&text).unwrap();
        assert!(!tail.contains("line 1"), "{tail}");
        assert!(tail.starts_with("line 2\n"), "{tail}");
        assert!(tail.ends_with("line 9"));
        assert_eq!(classifier_tail("```\nsecret code\n```\nreal reply").as_deref(), Some("real reply"));
        assert!(classifier_tail("```rust\nlet x = 1;\n```").is_none());
    }

    #[test]
    fn classifier_tail_redacts_tokens_and_assignments() {
        let text = "export API_KEY=abc123xyz\nkey is sk-or-v1-abcdefghijklmnopqrstuv1234\n你要哪一個方案？";
        let tail = classifier_tail(text).unwrap();
        assert!(!tail.contains("abc123xyz"), "{tail}");
        assert!(!tail.contains("sk-or-v1-"), "{tail}");
        assert!(tail.contains("API_KEY=…"), "{tail}");
        assert!(tail.ends_with("你要哪一個方案？"), "{tail}");
    }

    #[test]
    fn classifier_tail_caps_characters_and_keeps_end() {
        let text = (0..8).map(|i| format!("L{i} {}", "é".repeat(400))).collect::<Vec<_>>().join("\n");
        let tail = classifier_tail(&text).unwrap();
        assert_eq!(tail.chars().count(), CLASSIFIER_TAIL_CHARS);
        assert!(tail.ends_with('é'));
        assert!(!tail.contains("L0"));
    }

    #[test]
    fn classifier_tail_none_when_blank() {
        assert!(classifier_tail("").is_none());
        assert!(classifier_tail("  \n\n  ").is_none());
    }

    #[test]
    fn stop_question_carries_classify_with_question_line() {
        let p = stop_with(Some("我整理了兩個方案：\n1. 用 SQLite 存\n2. 用 JSON 檔存\n你要哪一個方案？"));
        assert_eq!(p.state, Some(AgentState::YourTurn));
        let c = p.classify.as_ref().expect("classify");
        assert_eq!(c.question_line, p.detail);
        assert_eq!(c.question_line.as_deref(), Some("你要哪一個方案？"));
        assert!(c.reply_tail.ends_with("你要哪一個方案？"));
        assert_eq!(c.took.as_deref(), Some("took 3m"));
    }

    #[test]
    fn stop_summary_carries_classify_with_took() {
        let p = stop_with(Some("已修正 classifier 的測試，全部通過。\n變更已 commit 到 llm-classifier-providers 分支。"));
        assert_eq!(p.state, Some(AgentState::AllSet));
        let c = p.classify.as_ref().expect("classify");
        assert_eq!(c.took.as_deref(), Some("took 3m"));
        assert!(c.question_line.is_none());
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["classify"]["took"], "took 3m");
        assert!(json["classify"].get("question_line").is_none());
    }

    #[test]
    fn working_events_have_no_classify_and_only_prompt_sets_turn_start() {
        let mut st = SessionState::default();
        let prompt = plan(
            &input(json!({ "hook_event_name": "userPromptSubmit", "cwd": "/r/x", "session_id": "s", "prompt": "go" })),
            &mut st,
            &ctx(10),
        )
        .unwrap();
        let v = serde_json::to_value(&prompt).unwrap();
        assert_eq!(v["turn_start"], true);
        assert!(v.get("classify").is_none());

        let tool = plan(
            &input(json!({ "hook_event_name": "preToolUse", "cwd": "/r/x", "session_id": "s", "tool_name": "read_file", "tool_input": { "path": "/r/x/a.rs" } })),
            &mut st,
            &ctx(20_000),
        )
        .unwrap();
        let v = serde_json::to_value(&tool).unwrap();
        assert!(v.get("turn_start").is_none());
        assert!(v.get("classify").is_none());
    }

    #[test]
    fn hook_config_is_lenient() {
        assert!(!parse_hook_config("").include_subagents);
        assert!(parse_hook_config("listen = \"127.0.0.1:1\"\n[hook]\ninclude_subagents = true\n[summarizer]\nenabled = false").include_subagents);
        assert!(!parse_hook_config("[hook]\ninclude_subagents = \"yes\"").include_subagents);
        assert!(!parse_hook_config("not toml [[[").include_subagents);
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
