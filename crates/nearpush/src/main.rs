//! `nearpush send ...`: posts one event to the local nearpushd.
//! `nearpush hook`: Kiro CLI hook adapter; reads the hook event JSON on stdin.

mod hook;

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;

const DEFAULT_ADDR: &str = "http://127.0.0.1:8787";
/// Mirrors the daemon's limit so a huge stdin fails here with a clear message.
const MAX_STDIN_BYTES: u64 = 16 * 1024;

#[derive(Parser)]
#[command(name = "nearpush", version, about = "Push events to nearpushd")]
struct Cli {
    /// Daemon base URL.
    #[arg(long, global = true, env = "NEARPUSH_ADDR", default_value = DEFAULT_ADDR)]
    addr: String,
    /// Push token file [default: ~/.config/nearpush/token].
    #[arg(long, global = true, env = "NEARPUSH_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Send one event.
    Send(SendArgs),
    /// Kiro CLI hook: reads the hook event JSON on stdin and updates the display.
    /// Never fails the agent: always exits 0 and prints nothing on stdout.
    Hook,
}

#[derive(clap::Args)]
struct SendArgs {
    /// Headline for generic events; the task line for --state events.
    #[arg(long)]
    title: String,
    /// Body text; `-` reads it from stdin.
    #[arg(long)]
    body: Option<String>,
    #[arg(long, value_enum)]
    level: Option<Level>,
    /// Coding-agent state shown as the big card.
    #[arg(long, value_enum)]
    state: Option<AgentState>,
    /// Short line under the headline.
    #[arg(long)]
    subtitle: Option<String>,
    /// Footer, e.g. "frontend / 7008".
    #[arg(long)]
    project: Option<String>,
    /// When the task started: `now`, unix seconds/milliseconds, an RFC 3339 time like
    /// `2026-11-04T18:49:33Z`, or an age like `18m`, `1h5m`.
    #[arg(long, value_parser = parse_started_at)]
    started_at: Option<u64>,
    /// How long the task took, e.g. `65`, `90s`, `1m`, `1h5m`.
    #[arg(long, value_parser = parse_duration_ms)]
    duration: Option<u64>,
    /// Opened when the card is tapped (http/https).
    #[arg(long)]
    url: Option<String>,
    /// Tag; repeatable.
    #[arg(long = "tag")]
    tags: Vec<String>,
    /// Icon key (a-z, 0-9, -), served from /icons/<sender>.
    #[arg(long, default_value = "cli")]
    sender: String,
    /// App name shown on the card's top line [default: from --sender].
    #[arg(long)]
    app: Option<String>,
    /// Stable session id; cards of one session share a tag.
    #[arg(long)]
    session: Option<String>,
    /// What the session is for, shown under the project.
    #[arg(long)]
    session_label: Option<String>,
    /// Machine the event came from.
    #[arg(long)]
    host: Option<String>,
    /// Extra context line, e.g. the running tool.
    #[arg(long)]
    detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
enum AgentState {
    Allow,
    #[value(name = "your_turn", alias = "your-turn")]
    YourTurn,
    #[value(name = "all_set", alias = "all-set")]
    AllSet,
    Working,
}

#[derive(Debug, Default, Serialize)]
struct Payload {
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    level: Option<Level>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<AgentState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subtitle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sender: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    app: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    /// Sent only by `hook` at UserPromptSubmit, for the daemon's session summarizer.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Parses `90`, `90s`, `18m`, `1h5m`, `2h`, `1d` into milliseconds (bare numbers are seconds).
fn parse_duration_ms(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    if let Ok(secs) = s.parse::<u64>() {
        return secs.checked_mul(1000).ok_or_else(|| "duration too large".into());
    }
    let mut total: u64 = 0;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let unit = match c {
            's' => 1_000,
            'm' => 60_000,
            'h' => 3_600_000,
            'd' => 86_400_000,
            _ => return Err(format!("invalid duration {s:?}; use e.g. 90s, 18m, 1h5m")),
        };
        let n: u64 = num.parse().map_err(|_| format!("invalid duration {s:?}"))?;
        total = n.checked_mul(unit).and_then(|v| total.checked_add(v)).ok_or("duration too large")?;
        num.clear();
    }
    if !num.is_empty() {
        return Err(format!("invalid duration {s:?}: trailing number without unit"));
    }
    Ok(total)
}

/// `now`, unix seconds, unix milliseconds, an RFC 3339 / ISO 8601 time, or an age
/// (`18m` = 18 minutes ago).
fn parse_started_at(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("now") {
        return Ok(now_ms());
    }
    if let Ok(n) = s.parse::<u64>() {
        // Unix seconds stay below 1e11 until the year 5138.
        return Ok(if n < 100_000_000_000 { n * 1000 } else { n });
    }
    if s.len() >= 10 && s.as_bytes()[4] == b'-' {
        return parse_rfc3339_ms(s);
    }
    let age = parse_duration_ms(s).map_err(|_| {
        format!("invalid --started-at {s:?}; use now, unix seconds, 2026-11-04T18:49:33Z, or an age like 18m")
    })?;
    now_ms().checked_sub(age).ok_or_else(|| "--started-at is before 1970".into())
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM-DDTHH:MM[:SS[.fff]](Z|±HH:MM|±HHMM)`; `T` may be `t` or a space. A time
/// with no offset is rejected rather than guessed.
fn parse_rfc3339_ms(s: &str) -> Result<u64, String> {
    let bad = || format!("invalid time {s:?}; use e.g. 2026-11-04T18:49:33Z or 2026-11-05T02:49:33+08:00");
    let b = s.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Result<i64, String> {
        let t = s.get(r).ok_or_else(bad)?;
        if t.is_empty() || !t.bytes().all(|c| c.is_ascii_digit()) {
            return Err(bad());
        }
        t.parse::<i64>().map_err(|_| bad())
    };
    if b.len() < 16 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') || b[13] != b':' {
        return Err(bad());
    }
    let (y, mo, d, h, mi) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?);
    let mut i = 16;
    let mut sec = 0;
    let mut ms = 0;
    if b.get(i) == Some(&b':') {
        sec = num(i + 1..i + 3)?;
        i += 3;
        if matches!(b.get(i), Some(b'.') | Some(b',')) {
            let start = i + 1;
            let mut end = start;
            while end < b.len() && b[end].is_ascii_digit() {
                end += 1;
            }
            if end == start {
                return Err(bad());
            }
            let frac = &s[start..end.min(start + 3)];
            ms = frac.parse::<i64>().map_err(|_| bad())? * 10i64.pow(3 - frac.len() as u32);
            i = end;
        }
    }
    let offset_min = match b.get(i) {
        Some(b'Z') | Some(b'z') if i + 1 == b.len() => 0,
        Some(&sign @ (b'+' | b'-')) => {
            let rest = &s[i + 1..];
            let (oh, om) = match rest.len() {
                5 if rest.as_bytes()[2] == b':' => (num(i + 1..i + 3)?, num(i + 4..i + 6)?),
                4 => (num(i + 1..i + 3)?, num(i + 3..i + 5)?),
                _ => return Err(bad()),
            };
            if oh > 23 || om > 59 {
                return Err(bad());
            }
            let m = oh * 60 + om;
            if sign == b'+' { m } else { -m }
        }
        _ => return Err(format!("{} (a UTC offset such as Z or +08:00 is required)", bad())),
    };
    let dim = [31, if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if !(1..=12).contains(&mo) || d < 1 || d > dim[(mo - 1) as usize] || h > 23 || mi > 59 || sec > 60 {
        return Err(bad());
    }
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec.min(59) - offset_min * 60;
    if secs <= 0 {
        return Err("--started-at is before 1970".into());
    }
    Ok(secs as u64 * 1000 + ms as u64)
}

fn default_token_file() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("NEARPUSH_CONFIG_DIR") {
        return Ok(PathBuf::from(dir).join("token"));
    }
    let home = std::env::var_os("HOME").ok_or("HOME is not set; pass --token-file")?;
    Ok(PathBuf::from(home).join(".config/nearpush/token"))
}

fn read_token(path: &PathBuf) -> Result<String, String> {
    let t = std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("token file {} not found; start nearpushd once to create it", path.display())
        } else {
            format!("cannot read token file {}: {e}", path.display())
        }
    })?;
    let t = t.trim().to_string();
    if t.is_empty() {
        return Err(format!("token file {} is empty", path.display()));
    }
    Ok(t)
}

fn read_stdin_body() -> Result<String, String> {
    let mut buf = String::new();
    std::io::stdin()
        .take(MAX_STDIN_BYTES + 1)
        .read_to_string(&mut buf)
        .map_err(|e| format!("cannot read body from stdin: {e}"))?;
    if buf.len() as u64 > MAX_STDIN_BYTES {
        return Err(format!("body from stdin exceeds {MAX_STDIN_BYTES} bytes"));
    }
    Ok(buf.trim_end_matches(['\n', '\r']).to_string())
}

async fn send(addr: &str, token: &str, payload: &Payload) -> Result<u64, String> {
    send_with_timeout(addr, token, payload, Duration::from_secs(10)).await
}

async fn send_with_timeout(addr: &str, token: &str, payload: &Payload, timeout: Duration) -> Result<u64, String> {
    let url = format!("{}/push", addr.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(timeout)
        .connect_timeout(timeout)
        .build()
        .map_err(|e| format!("cannot build HTTP client: {e}"))?;
    let resp = client
        .post(&url)
        .bearer_auth(token)
        .json(payload)
        .send()
        .await
        .map_err(|e| {
            if e.is_connect() {
                format!("cannot reach nearpushd at {addr} (is it running?): {e}")
            } else if e.is_timeout() {
                format!("nearpushd at {addr} did not answer in time")
            } else {
                format!("request to {url} failed: {e}")
            }
        })?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let json: Option<serde_json::Value> = serde_json::from_str(&text).ok();
    if !status.is_success() {
        let msg = json
            .as_ref()
            .and_then(|v| v.get("error"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or(text);
        return Err(format!("nearpushd rejected the event ({status}): {msg}"));
    }
    json.as_ref()
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_u64())
        .ok_or_else(|| format!("unexpected response from nearpushd: {text}"))
}

async fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::Hook => {
            hook::run(&cli.addr, cli.token_file).await;
            Ok(())
        }
        Command::Send(a) => {
            let token_file = match cli.token_file {
                Some(p) => p,
                None => default_token_file()?,
            };
            let token = read_token(&token_file)?;
            let body = match a.body.as_deref() {
                Some("-") => Some(read_stdin_body()?),
                _ => a.body,
            };
            let payload = Payload {
                title: a.title,
                body,
                level: a.level,
                state: a.state,
                subtitle: a.subtitle,
                project: a.project,
                started_at: a.started_at,
                duration_ms: a.duration,
                url: a.url,
                tags: a.tags,
                sender: Some(a.sender),
                app: a.app,
                session: a.session,
                session_label: a.session_label,
                host: a.host,
                detail: a.detail,
                prompt: None,
            };
            let id = send(&cli.addr, &token, &payload).await?;
            println!("sent event #{id}");
            Ok(())
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // Recursion guard: the daemon's summarizer runs kiro-cli with this set, so its
    // own hooks must not report back. Checked before anything else, stdin included.
    if hook::disabled() && std::env::args().skip(1).any(|a| a == "hook") {
        return ExitCode::SUCCESS;
    }
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nearpush: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration_ms("65").unwrap(), 65_000);
        assert_eq!(parse_duration_ms("90s").unwrap(), 90_000);
        assert_eq!(parse_duration_ms("18m").unwrap(), 18 * 60_000);
        assert_eq!(parse_duration_ms("1h5m").unwrap(), 65 * 60_000);
        assert!(parse_duration_ms("5x").is_err());
        assert!(parse_duration_ms("1h5").is_err());
        assert!(parse_duration_ms("").is_err());
    }

    #[test]
    fn started_at_forms() {
        assert_eq!(parse_started_at("1700000000").unwrap(), 1_700_000_000_000);
        assert_eq!(parse_started_at("1700000000123").unwrap(), 1_700_000_000_123);
        let ago = parse_started_at("18m").unwrap();
        let diff = now_ms() - ago;
        assert!((18 * 60_000..18 * 60_000 + 5_000).contains(&diff));
        assert!(parse_started_at("yesterday").is_err());
    }

    #[test]
    fn started_at_rfc3339() {
        assert_eq!(parse_started_at("2026-11-04T18:49:33Z").unwrap(), 1_793_818_173_000);
        assert_eq!(parse_started_at("2026-11-05T02:49:33+08:00").unwrap(), 1_793_818_173_000);
        assert_eq!(parse_started_at("2026-11-04T13:49:33-0500").unwrap(), 1_793_818_173_000);
        assert_eq!(parse_started_at("2026-11-04 18:49:33.25Z").unwrap(), 1_793_818_173_250);
        assert_eq!(parse_started_at("2026-11-04T18:49Z").unwrap(), 1_793_818_140_000);
        assert_eq!(parse_started_at("1970-01-01T00:00:01Z").unwrap(), 1000);
        assert_eq!(parse_started_at("2024-02-29T00:00:00Z").unwrap(), 1_709_164_800_000);
        for bad in ["2026-11-04T18:49:33", "2026-13-01T00:00:00Z", "2026-02-30T00:00:00Z", "2026-11-04T18:49:33+8", "2026-11-04X18:49:33Z", "2026-11-04T18:49:33Zjunk"] {
            assert!(parse_started_at(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn state_names_match_daemon() {
        let p = Payload {
            title: "t".into(),
            level: Some(Level::Warn),
            state: Some(AgentState::YourTurn),
            duration_ms: Some(1),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&p).unwrap(),
            r#"{"title":"t","level":"warn","state":"your_turn","duration_ms":1}"#
        );
        let cli = Cli::try_parse_from(["nearpush", "send", "--title", "t", "--state", "your_turn"]).unwrap();
        let Command::Send(a) = cli.command else { panic!("send") };
        assert!(matches!(a.state, Some(AgentState::YourTurn)));
        assert_eq!(a.sender, "cli", "default sender");
        assert!(Cli::try_parse_from(["nearpush", "send", "--title", "t", "--state", "all-set"]).is_ok());
    }
}
