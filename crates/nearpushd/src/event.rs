//! Event payloads: what the CLI posts and what the page receives.

use serde::{Deserialize, Serialize};

pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_SUBTITLE_CHARS: usize = 200;
pub const MAX_PROJECT_CHARS: usize = 100;
pub const MAX_BODY_CHARS: usize = 4000;
pub const MAX_URL_CHARS: usize = 2048;
pub const MAX_TAGS: usize = 10;
pub const MAX_TAG_CHARS: usize = 32;
pub const MAX_SENDER_CHARS: usize = 32;
pub const MAX_APP_CHARS: usize = 32;
pub const MAX_SESSION_CHARS: usize = 128;
pub const MAX_SESSION_LABEL_CHARS: usize = 100;
pub const MAX_HOST_CHARS: usize = 64;
pub const MAX_DETAIL_CHARS: usize = 300;
/// Longest prompt accepted for the session summarizer (never stored in events).
pub const MAX_PROMPT_CHARS: usize = 4000;
/// Longest accepted `duration_ms` (30 days).
pub const MAX_DURATION_MS: u64 = 30 * 24 * 3600 * 1000;
/// How far in the future `started_at` may lie (clock skew allowance).
pub const MAX_FUTURE_SKEW_MS: u64 = 24 * 3600 * 1000;
/// Largest `workflows_waiting` kept; a larger value is dropped.
pub const MAX_WORKFLOWS: u32 = 1000;
/// Longest reply tail sent to the turn-end classifier.
pub const MAX_CLASSIFY_TAIL_CHARS: usize = 2000;
/// Longest question line in a classify payload.
pub const MAX_CLASSIFY_LINE_CHARS: usize = 80;
/// Longest "took …" text in a classify payload.
pub const MAX_CLASSIFY_TOOK_CHARS: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    #[default]
    Info,
    Warn,
    Error,
}

/// Coding-agent status shown as the big card on the desk display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Agent is waiting for a permission decision.
    Allow,
    /// Agent is waiting for user input.
    YourTurn,
    /// Task finished.
    AllSet,
    /// Agent is busy.
    Working,
}

/// Material for the turn-end classifier, sent with a Stop push. Never stored.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnClassify {
    /// Last lines of the reply, already redacted by the hook.
    #[serde(default)]
    pub reply_tail: String,
    /// The hook's question line (rules verdict), if any.
    #[serde(default)]
    pub question_line: Option<String>,
    /// "took Xm" text for an all_set card.
    #[serde(default)]
    pub took: Option<String>,
}

/// Body of `POST /push`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewEvent {
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub level: Level,
    #[serde(default)]
    pub state: Option<AgentState>,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    /// Unix milliseconds when the task started; the page renders "Xm so far".
    #[serde(default)]
    pub started_at: Option<u64>,
    /// How long the task took, in milliseconds; the page renders "took Xm".
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Icon key: `[a-z0-9-]`, at most 32 characters, e.g. `kiro`, `cli`.
    #[serde(default)]
    pub sender: Option<String>,
    /// Display name of the sending app, e.g. "Kiro".
    #[serde(default)]
    pub app: Option<String>,
    /// Stable session id, so cards from two sessions can be told apart.
    #[serde(default)]
    pub session: Option<String>,
    /// Human-readable session purpose.
    #[serde(default)]
    pub session_label: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    /// Extra context line, e.g. the tool being run or "took 3m".
    #[serde(default)]
    pub detail: Option<String>,
    /// The user's prompt, fed to the session summarizer. Never persisted.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Running workflow count from older hook builds. Accepted and dropped: running
    /// counts never reach a card.
    #[serde(default)]
    pub workflows_running: Option<u32>,
    /// Waiting workflow runs of the session. FYI data only: never used in a verdict,
    /// and kept on the stored card only when the final verdict is `all_set`.
    #[serde(default)]
    pub workflows_waiting: Option<u32>,
    /// Stop payload material for the turn-end classifier. Not stored.
    #[serde(default)]
    pub classify: Option<TurnClassify>,
    /// Turn-end record, written by the daemon on Stop cards. Clients cannot send it.
    #[serde(skip)]
    pub classify_provider: Option<String>,
    #[serde(skip)]
    pub verdict_source: Option<String>,
    #[serde(skip)]
    pub question_line: Option<String>,
    /// Probability of the parallel noul question (Jev only). Logged, never decides.
    #[serde(skip)]
    pub jev_noul: Option<f64>,
    /// Sent by the hook on UserPromptSubmit: the session's turn started.
    #[serde(default)]
    pub turn_start: bool,
    /// Sent by the hook on SessionEnd: the session no longer waits on the user.
    /// Only clears its pending state; nothing is stored or shown.
    #[serde(default)]
    pub session_end: bool,
    /// Set by the server while paused; clients cannot send it.
    #[serde(skip)]
    pub muted: bool,
    /// Id of the relay event this came from; set by the relay puller, never by clients.
    #[serde(skip)]
    pub relay_id: Option<u64>,
    /// A history record of a dismissed card, written by the daemon only; clients cannot send it.
    #[serde(skip)]
    pub dismissed: bool,
}

/// A stored event, as persisted in JSONL and sent to subscribers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: u64,
    /// Unix milliseconds when the daemon accepted the event.
    pub ts: u64,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default)]
    pub level: Level,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<AgentState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Waiting workflow runs, an FYI footer on an `all_set` card only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflows_waiting: Option<u32>,
    /// Turn-end record of a Stop card: the provider asked (`kiro`, `openrouter`,
    /// `typesafe`), absent when no provider ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classify_provider: Option<String>,
    /// Where the card's verdict came from: `model`, `rules` or `fallback`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict_source: Option<String>,
    /// The question line of a `your_turn` card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_line: Option<String>,
    /// Jev's noul probability from the parallel question. Logged only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jev_noul: Option<f64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub muted: bool,
    /// Relay event id, kept so the puller does not show the same event twice after a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_id: Option<u64>,
    /// History record of a card the user swiped away. Never a notification: the page
    /// lists it in history and skips it for the main card.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dismissed: bool,
}

impl Event {
    /// Whether the page shows this event: not muted, and either a generic event
    /// (no state) or a state in `states`.
    pub fn visible(&self, states: &[AgentState]) -> bool {
        !self.muted && self.state.is_none_or(|s| states.contains(&s))
    }
}

/// `[a-z0-9-]`, 1..=32 characters. Also used to resolve `/icons/<sender>`, so it
/// must never admit `/`, `.` or anything else a path could be built from.
pub fn valid_sender(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_SENDER_CHARS
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Cleans and length-checks an optional single-line field.
fn opt_field(field: &str, value: Option<String>, max: usize) -> Result<Option<String>, String> {
    let v = clean(value);
    if let Some(s) = &v {
        check_len(field, s, max)?;
    }
    Ok(v)
}

fn check_len(field: &str, value: &str, max: usize) -> Result<(), String> {
    let n = value.chars().count();
    if n > max {
        return Err(format!("{field} is {n} characters; the limit is {max}"));
    }
    Ok(())
}

/// Empty optional strings are treated as absent.
fn clean(opt: Option<String>) -> Option<String> {
    opt.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

impl NewEvent {
    /// Validates limits and normalises the payload. `now_ms` is the current time.
    pub fn validate(self, now_ms: u64) -> Result<NewEvent, String> {
        let title = self.title.trim().to_string();
        if title.is_empty() {
            return Err("title is required".into());
        }
        check_len("title", &title, MAX_TITLE_CHARS)?;
        let body = clean(self.body);
        if let Some(b) = &body {
            check_len("body", b, MAX_BODY_CHARS)?;
        }
        let subtitle = clean(self.subtitle);
        if let Some(s) = &subtitle {
            check_len("subtitle", s, MAX_SUBTITLE_CHARS)?;
        }
        let project = clean(self.project);
        if let Some(p) = &project {
            check_len("project", p, MAX_PROJECT_CHARS)?;
        }
        if let Some(t) = self.started_at {
            if t == 0 || t > now_ms + MAX_FUTURE_SKEW_MS {
                return Err("started_at must be unix milliseconds, not in the future".into());
            }
        }
        if let Some(d) = self.duration_ms {
            if d > MAX_DURATION_MS {
                return Err(format!("duration_ms exceeds {MAX_DURATION_MS}"));
            }
        }
        let url = clean(self.url);
        if let Some(u) = &url {
            check_len("url", u, MAX_URL_CHARS)?;
            let lower = u.to_ascii_lowercase();
            if !(lower.starts_with("https://") || lower.starts_with("http://")) {
                return Err("url must start with http:// or https://".into());
            }
        }
        if self.tags.len() > MAX_TAGS {
            return Err(format!("at most {MAX_TAGS} tags are allowed"));
        }
        let mut tags = Vec::with_capacity(self.tags.len());
        for t in self.tags {
            let t = t.trim().to_string();
            if t.is_empty() {
                continue;
            }
            check_len("tag", &t, MAX_TAG_CHARS)?;
            tags.push(t);
        }
        let sender = clean(self.sender);
        if let Some(s) = &sender {
            if !valid_sender(s) {
                return Err("sender must be 1-32 characters of a-z, 0-9 and -".into());
            }
        }
        let app = opt_field("app", self.app, MAX_APP_CHARS)?;
        let session = opt_field("session", self.session, MAX_SESSION_CHARS)?;
        let session_label = opt_field("session_label", self.session_label, MAX_SESSION_LABEL_CHARS)?;
        let host = opt_field("host", self.host, MAX_HOST_CHARS)?;
        let detail = opt_field("detail", self.detail, MAX_DETAIL_CHARS)?;
        let prompt = opt_field("prompt", self.prompt, MAX_PROMPT_CHARS)?;
        let classify = match self.classify {
            Some(c) => {
                check_len("classify.reply_tail", &c.reply_tail, MAX_CLASSIFY_TAIL_CHARS)?;
                if let Some(q) = &c.question_line {
                    check_len("classify.question_line", q, MAX_CLASSIFY_LINE_CHARS)?;
                }
                if let Some(t) = &c.took {
                    check_len("classify.took", t, MAX_CLASSIFY_TOOK_CHARS)?;
                }
                Some(c)
            }
            None => None,
        };
        Ok(NewEvent {
            title,
            body,
            level: self.level,
            state: self.state,
            subtitle,
            project,
            started_at: self.started_at,
            duration_ms: self.duration_ms,
            url,
            tags,
            sender,
            app,
            session,
            session_label,
            host,
            detail,
            prompt,
            workflows_running: None,
            // Kept whatever the state: the verdict may still change before commit.
            workflows_waiting: self.workflows_waiting.filter(|n| (1..=MAX_WORKFLOWS).contains(n)),
            classify,
            turn_start: self.turn_start,
            session_end: self.session_end,
            muted: self.muted,
            classify_provider: None,
            verdict_source: None,
            question_line: None,
            jev_noul: None,
            relay_id: self.relay_id,
            dismissed: self.dismissed,
        })
    }

    pub fn into_event(self, id: u64, ts: u64) -> Event {
        Event {
            id,
            ts,
            title: self.title,
            body: self.body,
            level: self.level,
            state: self.state,
            subtitle: self.subtitle,
            project: self.project,
            started_at: self.started_at,
            duration_ms: self.duration_ms,
            url: self.url,
            tags: self.tags,
            sender: self.sender,
            app: self.app,
            session: self.session,
            session_label: self.session_label,
            host: self.host,
            detail: self.detail,
            // Footer only when the final verdict is all_set.
            workflows_waiting: self.workflows_waiting.filter(|_| self.state == Some(AgentState::AllSet)),
            classify_provider: self.classify_provider,
            verdict_source: self.verdict_source,
            question_line: self.question_line,
            jev_noul: self.jev_noul,
            muted: self.muted,
            relay_id: self.relay_id,
            dismissed: self.dismissed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(title: &str) -> NewEvent {
        NewEvent { title: title.into(), ..Default::default() }
    }

    #[test]
    fn title_required_and_bounded() {
        assert!(ev("  ").validate(1).is_err());
        assert!(ev(&"x".repeat(MAX_TITLE_CHARS)).validate(1).is_ok());
        assert!(ev(&"x".repeat(MAX_TITLE_CHARS + 1)).validate(1).is_err());
        // Limits count characters, not bytes, so CJK titles get the full allowance.
        assert!(ev(&"多".repeat(MAX_TITLE_CHARS)).validate(1).is_ok());
    }

    #[test]
    fn url_scheme_checked() {
        let mut e = ev("t");
        e.url = Some("javascript:alert(1)".into());
        assert!(e.validate(1).is_err());
        let mut e = ev("t");
        e.url = Some("https://example.com".into());
        assert!(e.validate(1).is_ok());
    }

    #[test]
    fn started_at_not_in_future() {
        let now = 1_000_000_000_000;
        let mut e = ev("t");
        e.started_at = Some(now + MAX_FUTURE_SKEW_MS + 1);
        assert!(e.validate(now).is_err());
        let mut e = ev("t");
        e.started_at = Some(now - 60_000);
        assert!(e.validate(now).is_ok());
    }

    #[test]
    fn tags_bounded() {
        let mut e = ev("t");
        e.tags = (0..=MAX_TAGS).map(|i| i.to_string()).collect();
        assert!(e.validate(1).is_err());
    }

    #[test]
    fn sender_validated() {
        for ok in ["kiro", "cli", "a", "my-agent-2", &"x".repeat(32)] {
            assert!(valid_sender(ok), "{ok}");
            let mut e = ev("t");
            e.sender = Some(ok.to_string());
            assert!(e.validate(1).is_ok(), "{ok}");
        }
        for bad in ["", "Kiro", "../etc", "a/b", "a.svg", "a b", "ki_ro", "é", &"x".repeat(33)] {
            assert!(!valid_sender(bad), "{bad}");
        }
        let mut e = ev("t");
        e.sender = Some("../x".into());
        assert!(e.validate(1).is_err());
    }

    #[test]
    fn context_fields_bounded_and_prompt_not_stored() {
        let mut e = ev("t");
        e.session_label = Some("修".repeat(MAX_SESSION_LABEL_CHARS + 1));
        assert!(e.validate(1).is_err());
        let mut e = ev("t");
        e.app = Some("Kiro".into());
        e.session = Some("abc".into());
        e.prompt = Some("fix the login bug".into());
        let e = e.validate(1).unwrap().into_event(1, 1);
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"app\":\"Kiro\""));
        assert!(!json.contains("login bug"), "prompt must not reach the stored event");
    }

    #[test]
    fn unknown_fields_and_bad_state_rejected() {
        assert!(serde_json::from_str::<NewEvent>(r#"{"title":"t","titel":"x"}"#).is_err());
        assert!(serde_json::from_str::<NewEvent>(r#"{"title":"t","state":"sleeping"}"#).is_err());
        let e: NewEvent = serde_json::from_str(r#"{"title":"t","state":"your_turn"}"#).unwrap();
        assert_eq!(e.state, Some(AgentState::YourTurn));
    }

    #[test]
    fn dismissed_cannot_be_set_by_client() {
        assert!(serde_json::from_str::<NewEvent>(r#"{"title":"t","dismissed":true}"#).is_err());
        let json = serde_json::to_string(&ev("t").validate(1).unwrap().into_event(1, 1)).unwrap();
        assert!(!json.contains("dismissed"), "ordinary events keep the old line shape");
    }

    #[test]
    fn muted_cannot_be_set_by_client() {
        assert!(serde_json::from_str::<NewEvent>(r#"{"title":"t","muted":true}"#).is_err());
        let mut e = ev("t");
        e.muted = true;
        let json = serde_json::to_string(&e.validate(1).unwrap().into_event(1, 1)).unwrap();
        assert!(json.contains("\"muted\":true"));
        let json = serde_json::to_string(&ev("t").validate(1).unwrap().into_event(1, 1)).unwrap();
        assert!(!json.contains("muted"), "unmuted events keep the old line shape");
    }

    #[test]
    fn waiting_workflow_footer_only_on_all_set() {
        let stored = |state: Option<AgentState>, waiting: Option<u32>| {
            let mut e = ev("t");
            e.state = state;
            e.workflows_running = Some(3);
            e.workflows_waiting = waiting;
            e.validate(1).unwrap().into_event(1, 1)
        };
        let e = stored(Some(AgentState::AllSet), Some(1));
        assert_eq!(e.workflows_waiting, Some(1));
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"workflows_waiting\":1") && !json.contains("workflows_running"), "{json}");
        let json = serde_json::to_string(&stored(Some(AgentState::YourTurn), Some(2))).unwrap();
        assert!(!json.contains("workflows_"), "your_turn carries no workflow count: {json}");
        let json = serde_json::to_string(&stored(None, Some(2))).unwrap();
        assert!(!json.contains("workflows_"), "generic events carry no workflow count: {json}");
        assert!(!serde_json::to_string(&stored(Some(AgentState::AllSet), Some(0))).unwrap().contains("workflows_"), "0 → no footer");
        assert!(!serde_json::to_string(&stored(Some(AgentState::AllSet), None)).unwrap().contains("workflows_"), "missing → no footer");
        let big = stored(Some(AgentState::AllSet), Some(MAX_WORKFLOWS + 1));
        assert_eq!(big.workflows_waiting, None, "an out-of-range count is dropped, the card is kept");
        // Kept through validation on any state, so a later verdict of all_set can show it.
        let mut held = ev("your turn.");
        held.state = Some(AgentState::YourTurn);
        held.workflows_waiting = Some(1);
        let mut held = held.validate(1).unwrap();
        assert_eq!(held.workflows_waiting, Some(1));
        held.state = Some(AgentState::AllSet);
        assert_eq!(held.into_event(1, 1).workflows_waiting, Some(1));
    }

    #[test]
    fn visibility_rule() {
        let defaults = crate::config::DEFAULT_FILTER_STATES;
        let with = |state: Option<AgentState>, muted: bool| {
            let mut e = ev("t");
            e.state = state;
            e.muted = muted;
            e.validate(1).unwrap().into_event(1, 1)
        };
        assert!(!with(Some(AgentState::Working), false).visible(defaults));
        assert!(with(Some(AgentState::AllSet), false).visible(defaults));
        assert!(with(Some(AgentState::YourTurn), false).visible(defaults));
        assert!(with(None, false).visible(defaults), "generic events always show");
        assert!(with(None, false).visible(&[]));
        assert!(!with(Some(AgentState::AllSet), true).visible(defaults), "muted is hidden");
        assert!(with(Some(AgentState::Working), false).visible(&[AgentState::Working]));
    }
}
