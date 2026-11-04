//! Per-session prompt history and LLM-made purpose labels.
//!
//! Prompts arrive on `POST /push` (the `prompt` field, sent by `nearpush hook` at
//! UserPromptSubmit). They are kept here, capped, in a 0600 `sessions.json` next to
//! the event log, and are only ever handed to the configured summarizer. Events in
//! `events.jsonl` carry the label, never the prompts.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

pub const SESSIONS_FILE: &str = "sessions.json";
/// Sessions remembered; the least recently updated are dropped first.
pub const MAX_SESSIONS: usize = 100;
/// Latest prompts kept per session, besides the first one.
pub const MAX_RECENT: usize = 4;
/// Characters kept of each stored prompt.
pub const MAX_STORED_PROMPT_CHARS: usize = 500;
/// Labels sent in the hello frame.
pub const MAX_HELLO_LABELS: usize = 50;
/// Summarize again after this many prompts.
pub const RESUMMARIZE_EVERY: u32 = 5;

/// Broadcast when the summarizer produced a new label for a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionUpdate {
    pub session: String,
    pub label: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Entry {
    first: String,
    #[serde(default)]
    recent: VecDeque<String>,
    #[serde(default)]
    count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(default)]
    updated: u64,
}

pub struct Sessions {
    inner: Mutex<HashMap<String, Entry>>,
    path: Option<PathBuf>,
}

fn cap_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Character-bigram Jaccard similarity, case- and whitespace-insensitive.
fn similarity(a: &str, b: &str) -> f64 {
    fn grams(s: &str) -> std::collections::HashSet<(char, char)> {
        let cs: Vec<char> = s.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();
        cs.windows(2).map(|w| (w[0], w[1])).collect()
    }
    let (ga, gb) = (grams(a), grams(b));
    if ga.is_empty() || gb.is_empty() {
        return 1.0;
    }
    let inter = ga.intersection(&gb).count();
    let union = ga.len() + gb.len() - inter;
    inter as f64 / union as f64
}

/// Topic change: a substantial prompt that shares almost nothing with what came before.
fn topic_changed(prompt: &str, e: &Entry) -> bool {
    if prompt.chars().filter(|c| !c.is_whitespace()).count() < 12 {
        return false;
    }
    let best = std::iter::once(&e.first)
        .chain(e.recent.iter())
        .map(|p| similarity(prompt, p))
        .fold(0.0f64, f64::max);
    best < 0.05
}

impl Sessions {
    pub fn in_memory() -> Sessions {
        Sessions { inner: Mutex::new(HashMap::new()), path: None }
    }

    /// Loads `sessions.json` from `state_dir`; a missing or unreadable file starts empty.
    pub fn load(state_dir: &Path) -> Sessions {
        let path = state_dir.join(SESSIONS_FILE);
        let map = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!(error = %e, path = %path.display(), "ignoring unreadable sessions file");
                HashMap::new()
            }),
            Err(_) => HashMap::new(),
        };
        if path.exists() {
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
        Sessions { inner: Mutex::new(map), path: Some(path) }
    }

    pub fn label(&self, session: &str) -> Option<String> {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.get(session).and_then(|e| e.label.clone())
    }

    /// Known labels, most recently updated first.
    pub fn labels(&self) -> Vec<SessionUpdate> {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let mut v: Vec<(&String, &Entry)> = inner.iter().filter(|(_, e)| e.label.is_some()).collect();
        v.sort_by(|a, b| b.1.updated.cmp(&a.1.updated));
        v.into_iter()
            .take(MAX_HELLO_LABELS)
            .map(|(s, e)| SessionUpdate { session: s.clone(), label: e.label.clone().unwrap_or_default() })
            .collect()
    }

    /// Records a prompt. Returns true when the summarizer should run: the first
    /// prompt, every [`RESUMMARIZE_EVERY`] prompts, or a clear topic change.
    pub fn record_prompt(&self, session: &str, prompt: &str, now: u64) -> bool {
        let prompt = cap_chars(prompt.trim(), MAX_STORED_PROMPT_CHARS);
        if prompt.is_empty() {
            return false;
        }
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let run = match inner.get_mut(session) {
            None => {
                inner.insert(
                    session.to_string(),
                    Entry { first: prompt, recent: VecDeque::new(), count: 1, label: None, updated: now },
                );
                true
            }
            Some(e) => {
                let changed = topic_changed(&prompt, e);
                e.count += 1;
                e.updated = now;
                if e.recent.len() == MAX_RECENT {
                    e.recent.pop_front();
                }
                e.recent.push_back(prompt);
                changed || e.count % RESUMMARIZE_EVERY == 0 || e.label.is_none()
            }
        };
        if inner.len() > MAX_SESSIONS {
            let mut by_age: Vec<(String, u64)> = inner.iter().map(|(k, e)| (k.clone(), e.updated)).collect();
            by_age.sort_by_key(|(_, t)| *t);
            for (k, _) in by_age.into_iter().take(inner.len() - MAX_SESSIONS) {
                inner.remove(&k);
            }
        }
        self.save_locked(&inner);
        run
    }

    /// Text handed to the summarizer for `session`, or `None` if it has no prompts.
    pub fn summary_input(&self, session: &str) -> Option<String> {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let e = inner.get(session)?;
        let mut s = String::from(crate::summarizer::INSTRUCTION);
        s.push_str("\nFirst prompt:\n<<<\n");
        s.push_str(&e.first);
        s.push_str("\n>>>\n");
        if !e.recent.is_empty() {
            s.push_str("Latest prompts, oldest first:\n");
            for p in &e.recent {
                s.push_str("<<<\n");
                s.push_str(p);
                s.push_str("\n>>>\n");
            }
        }
        Some(s)
    }

    /// Stores a new label; returns false if it equals the current one.
    pub fn set_label(&self, session: &str, label: &str, now: u64) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(e) = inner.get_mut(session) else { return false };
        if e.label.as_deref() == Some(label) {
            return false;
        }
        e.label = Some(label.to_string());
        e.updated = now;
        self.save_locked(&inner);
        true
    }

    fn save_locked(&self, map: &HashMap<String, Entry>) {
        let Some(path) = &self.path else { return };
        if let Err(e) = write_private(path, map) {
            tracing::warn!(error = %e, path = %path.display(), "could not save sessions file");
        }
    }
}

fn write_private(path: &Path, map: &HashMap<String, Entry>) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec(map).map_err(io::Error::other)?;
    {
        let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        f.write_all(&data)?;
    }
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_on_first_every_fifth_and_topic_change() {
        let s = Sessions::in_memory();
        assert!(s.record_prompt("a", "fix the login redirect bug in the auth service", 1));
        assert!(s.set_label("a", "Fix login redirect", 1));
        assert!(!s.record_prompt("a", "yes", 2));
        assert!(!s.record_prompt("a", "the login redirect still loops after the fix", 3));
        assert!(!s.record_prompt("a", "ok", 4));
        assert!(s.record_prompt("a", "ok", 5), "fifth prompt");
        assert!(s.record_prompt("a", "幫我把相簿匯出成 PDF 並寄給客戶", 6), "topic change");
        assert!(s.record_prompt("b", "another session", 7), "first prompt of b");
    }

    #[test]
    fn similarity_behaves() {
        assert!(similarity("fix the login bug", "fix the login bug please") > 0.5);
        assert!(similarity("fix the login bug", "export album to pdf 幫我") < 0.1);
    }

    #[test]
    fn persisted_private_and_capped() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = Sessions::load(dir.path());
            s.record_prompt("a", &"x".repeat(2000), 1);
            for i in 0..10 {
                s.record_prompt("a", &format!("prompt {i}"), 2 + i);
            }
            s.set_label("a", "Label A", 20);
        }
        let path = dir.path().join(SESSIONS_FILE);
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let s = Sessions::load(dir.path());
        assert_eq!(s.label("a").as_deref(), Some("Label A"));
        let input = s.summary_input("a").unwrap();
        assert!(input.contains(&"x".repeat(MAX_STORED_PROMPT_CHARS)));
        assert!(!input.contains(&"x".repeat(MAX_STORED_PROMPT_CHARS + 1)));
        assert!(input.contains("prompt 9") && !input.contains("prompt 5"));
        assert_eq!(s.labels(), vec![SessionUpdate { session: "a".into(), label: "Label A".into() }]);
    }

    #[test]
    fn session_count_bounded() {
        let s = Sessions::in_memory();
        for i in 0..(MAX_SESSIONS + 10) {
            s.record_prompt(&format!("s{i}"), "p", i as u64);
        }
        assert_eq!(s.inner.lock().unwrap().len(), MAX_SESSIONS);
        assert!(s.summary_input("s0").is_none(), "oldest dropped");
    }
}
