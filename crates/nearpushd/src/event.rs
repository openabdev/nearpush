//! Event payloads: what the CLI posts and what the page receives.

use serde::{Deserialize, Serialize};

pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_SUBTITLE_CHARS: usize = 200;
pub const MAX_PROJECT_CHARS: usize = 100;
pub const MAX_BODY_CHARS: usize = 4000;
pub const MAX_URL_CHARS: usize = 2048;
pub const MAX_TAGS: usize = 10;
pub const MAX_TAG_CHARS: usize = 32;
/// Longest accepted `duration_ms` (30 days).
pub const MAX_DURATION_MS: u64 = 30 * 24 * 3600 * 1000;
/// How far in the future `started_at` may lie (clock skew allowance).
pub const MAX_FUTURE_SKEW_MS: u64 = 24 * 3600 * 1000;

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
    fn unknown_fields_and_bad_state_rejected() {
        assert!(serde_json::from_str::<NewEvent>(r#"{"title":"t","titel":"x"}"#).is_err());
        assert!(serde_json::from_str::<NewEvent>(r#"{"title":"t","state":"sleeping"}"#).is_err());
        let e: NewEvent = serde_json::from_str(r#"{"title":"t","state":"your_turn"}"#).unwrap();
        assert_eq!(e.state, Some(AgentState::YourTurn));
    }
}
