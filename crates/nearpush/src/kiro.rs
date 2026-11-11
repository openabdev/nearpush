//! Reads Kiro CLI's local session files for `nearpush hook`.
//!
//! Kiro's Stop hook input carries no reply text (kiro.dev/docs/hooks; Kiro#6258),
//! but Kiro CLI writes each session's transcript to
//! `~/.kiro/sessions/<workspace-hash>/<session_id>/messages.jsonl` before the Stop
//! hook runs, next to `session.json`. Workflow runs live in
//! `~/.kiro/sessions/<workspace-hash>/workflows/wf_*/`. Everything here is
//! read-only, parsed as JSON, path-injected for tests, and fails quietly.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;
use serde_json::Value;

/// Tail of `messages.jsonl` scanned for the final reply.
const TRANSCRIPT_TAIL_BYTES: u64 = 512 * 1024;
/// A `paused` workflow counts as waiting only while its state changed this recently.
const PAUSED_FRESH: Duration = Duration::from_secs(24 * 3600);
/// Time budget for the workflow scan; past it the count is dropped.
pub const WORKFLOW_SCAN_BUDGET: Duration = Duration::from_millis(40);

/// `NEARPUSH_KIRO_SESSIONS_DIR`, else `$HOME/.kiro/sessions`.
pub fn sessions_root() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("NEARPUSH_KIRO_SESSIONS_DIR") {
        return Some(PathBuf::from(d));
    }
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".kiro/sessions"))
}

fn valid_id(id: &str) -> bool {
    (1..=128).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `<root>/<hash>/<id>` whose `session.json` exists. Ids outside
/// `[A-Za-z0-9_-]{1,128}` are rejected so they can never form a path.
pub fn find_session_dir(root: &Path, id: &str) -> Option<PathBuf> {
    if !valid_id(id) {
        return None;
    }
    fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path().join(id))
        .find(|d| d.join("session.json").is_file())
}

/// Reads `session.json`; `None` when missing or not JSON.
pub fn read_session_json(dir: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(dir.join("session.json")).ok()?).ok()
}

/// A workflow step / sub-agent session: `rootConversationId` names another
/// conversation, or `_meta.kiro.workflow` is present, or
/// `_meta.kiro.serverSession.ownerSessionId` is set.
pub fn is_subagent(session: &Value, id: &str) -> bool {
    let nonempty = |v: Option<&Value>| v.and_then(Value::as_str).is_some_and(|s| !s.is_empty());
    let root = session.get("rootConversationId").and_then(Value::as_str).unwrap_or("");
    let own = session.get("id").and_then(Value::as_str).unwrap_or(id);
    let kiro = session.get("_meta").and_then(|m| m.get("kiro"));
    (!root.is_empty() && root != own)
        || kiro.and_then(|k| k.get("workflow")).is_some_and(Value::is_object)
        || nonempty(kiro.and_then(|k| k.get("serverSession")).and_then(|s| s.get("ownerSessionId")))
}

/// Text of an `assistant` record's `content`: a string, or an array of
/// `{ "text": ... }` parts.
fn content_text(content: &Value) -> Option<String> {
    let s = match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str).or_else(|| p.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    (!s.trim().is_empty()).then_some(s)
}

/// The current turn's last visible assistant reply in `messages.jsonl`.
///
/// Reads the last 512 KiB, walks the records backward and returns the first
/// `assistant` record whose `operationType` is `Say` (or absent); `Reasoning`
/// records are the model's thinking and are skipped. Stops with `None` at a
/// `turn_start`, so a reply from an earlier turn is never used.
pub fn last_assistant_text(messages: &Path) -> Option<String> {
    let mut f = File::open(messages).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(TRANSCRIPT_TAIL_BYTES);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.take(TRANSCRIPT_TAIL_BYTES).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0); // partial first line
    }
    for line in lines.iter().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
        let Some(p) = v.get("payload") else { continue };
        match p.get("type").and_then(Value::as_str) {
            Some("turn_start") => return None,
            Some("assistant") => {
                let op = p.get("operationType").and_then(Value::as_str);
                if !matches!(op, None | Some("Say")) {
                    continue;
                }
                if let Some(t) = p.get("content").and_then(content_text) {
                    return Some(t);
                }
            }
            _ => {}
        }
    }
    None
}

#[derive(Deserialize)]
struct WorkflowState {
    status: String,
    #[serde(rename = "parentSessionId")]
    parent: Option<String>,
}

/// Waiting (`paused`) workflow runs started by `session_id`, from
/// `<ws_dir>/wf_*/workflow-state.json` (`status`, `parentSessionId`). A paused run
/// counts only when its state file changed in the last 24 h; older dirs are skipped
/// before parsing. `None` when the dir is unreadable or `deadline` passes. FYI only:
/// the count never takes part in a verdict.
pub fn workflows_waiting(ws_dir: &Path, session_id: &str, now: SystemTime, deadline: Instant) -> Option<u32> {
    let mut waiting = 0u32;
    for entry in fs::read_dir(ws_dir).ok()?.flatten() {
        if Instant::now() > deadline {
            return None;
        }
        if !entry.file_name().to_string_lossy().starts_with("wf_") {
            continue;
        }
        let state_path = entry.path().join("workflow-state.json");
        let fresh = fs::metadata(&state_path)
            .and_then(|m| m.modified())
            .is_ok_and(|m| now.duration_since(m).unwrap_or(Duration::ZERO) < PAUSED_FRESH);
        if !fresh {
            continue;
        }
        let Some(st) = fs::read(&state_path).ok().and_then(|b| serde_json::from_slice::<WorkflowState>(&b).ok()) else {
            continue;
        };
        if st.parent.as_deref() == Some(session_id) && st.status == "paused" {
            waiting += 1;
        }
    }
    Some(waiting)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    #[test]
    fn find_session_dir_rejects_traversal() {
        let root = tempfile::tempdir().unwrap();
        let d = root.path().join("hash1/sess_ok-1");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("session.json"), "{}").unwrap();
        assert_eq!(find_session_dir(root.path(), "sess_ok-1"), Some(d));
        assert_eq!(find_session_dir(root.path(), "../x"), None);
        assert_eq!(find_session_dir(root.path(), "a/b"), None);
        assert_eq!(find_session_dir(root.path(), ""), None);
        assert_eq!(find_session_dir(root.path(), "missing"), None);
    }

    #[test]
    fn subagent_detection() {
        let top = json!({ "id": "sess_a", "title": "chat" });
        assert!(!is_subagent(&top, "sess_a"));
        let same_root = json!({ "id": "sess_a", "rootConversationId": "sess_a" });
        assert!(!is_subagent(&same_root, "sess_a"));
        let step = json!({ "id": "sess_b", "rootConversationId": "sess_a",
            "_meta": { "kiro": { "workflow": { "workflowId": "wf_1", "nodeId": "n", "type": "step" } } } });
        assert!(is_subagent(&step, "sess_b"));
        let owned = json!({ "id": "sess_c", "_meta": { "kiro": { "serverSession": { "role": "workflow-step", "ownerSessionId": "sess_a" } } } });
        assert!(is_subagent(&owned, "sess_c"));
        // A missing file reads as top-level (fail open).
        let dir = tempfile::tempdir().unwrap();
        assert!(read_session_json(dir.path()).is_none());
    }

    fn rec(payload: Value) -> String {
        json!({ "id": "x", "timestamp": 1, "payload": payload }).to_string()
    }

    #[test]
    fn last_assistant_text_from_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("messages.jsonl");
        let lines = [
            rec(json!({ "type": "turn_start" })),
            rec(json!({ "type": "assistant", "operationType": "Say", "content": "first turn reply?" })),
            rec(json!({ "type": "turn_end" })),
            rec(json!({ "type": "turn_start" })),
            rec(json!({ "type": "assistant", "operationType": "Say", "content": "working on it" })),
            rec(json!({ "type": "tool_call", "toolName": "x" })),
            rec(json!({ "type": "tool_result", "content": "Output: ok?" })),
            rec(json!({ "type": "assistant", "operationType": "Say", "content": "Done. Want me to push?" })),
            rec(json!({ "type": "assistant", "operationType": "Reasoning", "content": "thinking about it." })),
            rec(json!({ "type": "session_metadata" })),
        ];
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        assert_eq!(last_assistant_text(&path).as_deref(), Some("Done. Want me to push?"));

        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{}", rec(json!({ "type": "turn_end" }))).unwrap();
        writeln!(f, "{}", rec(json!({ "type": "turn_start" }))).unwrap();
        writeln!(f, "{{\"partial").unwrap();
        drop(f);
        assert_eq!(last_assistant_text(&path), None, "a new turn without a reply yields nothing");
        assert_eq!(last_assistant_text(&dir.path().join("missing.jsonl")), None);

        let parts = dir.path().join("parts.jsonl");
        fs::write(&parts, rec(json!({ "type": "assistant", "content": [{ "text": "a" }, { "text": "b?" }] }))).unwrap();
        assert_eq!(last_assistant_text(&parts).as_deref(), Some("a\nb?"));
    }

    #[test]
    fn last_assistant_text_reads_only_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("messages.jsonl");
        let mut f = File::create(&path).unwrap();
        let filler = rec(json!({ "type": "tool_result", "content": "x".repeat(1000) }));
        for _ in 0..700 {
            writeln!(f, "{filler}").unwrap();
        }
        writeln!(f, "{}", rec(json!({ "type": "assistant", "operationType": "Say", "content": "tail reply" }))).unwrap();
        drop(f);
        assert!(fs::metadata(&path).unwrap().len() > TRANSCRIPT_TAIL_BYTES);
        assert_eq!(last_assistant_text(&path).as_deref(), Some("tail reply"));
    }

    #[test]
    fn workflows_waiting_from_fixture() {
        let ws = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let mk = |name: &str, status: &str, parent: &str| {
            let d = ws.path().join(name);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("workflow-state.json"), json!({ "status": status, "parentSessionId": parent, "extra": 1 }).to_string())
                .unwrap();
            d
        };
        mk("wf_a", "running", "sess_me");
        mk("wf_c", "paused", "sess_me");
        mk("wf_d", "paused", "sess_other");
        mk("wf_e", "completed", "sess_me");
        let old = mk("wf_f", "paused", "sess_me");
        File::options().write(true).open(old.join("workflow-state.json")).unwrap().set_modified(now - Duration::from_secs(48 * 3600)).unwrap();
        mk("not_a_run", "paused", "sess_me");
        fs::create_dir_all(ws.path().join("wf_empty")).unwrap();
        let far = Instant::now() + Duration::from_secs(5);
        assert_eq!(workflows_waiting(ws.path(), "sess_me", now, far), Some(1));
        assert_eq!(workflows_waiting(ws.path(), "nobody", now, far), Some(0));
        assert_eq!(workflows_waiting(&ws.path().join("missing"), "sess_me", now, far), None);
        assert_eq!(workflows_waiting(ws.path(), "sess_me", now, Instant::now() - Duration::from_millis(1)), None);
    }
}
