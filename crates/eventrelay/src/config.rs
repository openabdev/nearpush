//! Environment and token-file configuration.

use std::net::SocketAddr;
use std::path::PathBuf;

use crate::auth::Auth;
use crate::store::Retention;

const DAY_MS: u64 = 86_400_000;
const MIN_TOKEN_LEN: usize = 32;
const MAX_NAME_LEN: usize = 32;

pub struct Config {
    pub db_path: PathBuf,
    pub listen: SocketAddr,
    pub retention: Retention,
    pub auth: Auth,
}

/// Reads the configuration through `get`, which returns env values.
pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
    let db_path = get("EVENTRELAY_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/data/eventrelay.db"));
    let listen = match get("EVENTRELAY_LISTEN") {
        Some(v) => v
            .parse::<SocketAddr>()
            .map_err(|_| "EVENTRELAY_LISTEN must be a socket address".to_owned())?,
        None => "0.0.0.0:8080".parse().expect("default listen address"),
    };

    let max_events = positive(&get, "EVENTRELAY_MAX_EVENTS", 10_000)?;
    let max_age_days = positive(&get, "EVENTRELAY_MAX_AGE_DAYS", 7)?;
    let max_age_ms = max_age_days
        .checked_mul(DAY_MS)
        .ok_or_else(|| "EVENTRELAY_MAX_AGE_DAYS is too large".to_owned())?;

    let sources_path = get("EVENTRELAY_SOURCE_TOKENS_FILE")
        .ok_or_else(|| "EVENTRELAY_SOURCE_TOKENS_FILE is not set".to_owned())?;
    let consumer_path = get("EVENTRELAY_CONSUMER_TOKEN_FILE")
        .ok_or_else(|| "EVENTRELAY_CONSUMER_TOKEN_FILE is not set".to_owned())?;

    let sources = parse_source_tokens(&read(&sources_path)?)?;
    let consumer = parse_consumer_token(&read(&consumer_path)?)?;
    if sources.iter().any(|(_, token)| *token == consumer) {
        return Err("the consumer token must differ from every source token".to_owned());
    }

    Ok(Config {
        db_path,
        listen,
        retention: Retention {
            max_events,
            max_age_ms,
        },
        auth: Auth::new(sources, consumer),
    })
}

fn read(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("cannot read token file {path}: {e}"))
}

fn positive(get: &impl Fn(&str) -> Option<String>, key: &str, default: u64) -> Result<u64, String> {
    match get(key) {
        None => Ok(default),
        Some(v) => match v.trim().parse::<u64>() {
            Ok(n) if n >= 1 => Ok(n),
            _ => Err(format!("{key} must be a positive integer")),
        },
    }
}

/// Parses `name:token` lines. Errors name the line number, never the token.
pub fn parse_source_tokens(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let lineno = index + 1;
        let (name, token) = line
            .split_once(':')
            .ok_or_else(|| format!("line {lineno}: expected name:token"))?;
        if !valid_name(name) {
            return Err(format!(
                "line {lineno}: name must be 1-{MAX_NAME_LEN} characters of a-z, 0-9 or -"
            ));
        }
        if !valid_token(token) {
            return Err(format!(
                "line {lineno}: token must be at least {MIN_TOKEN_LEN} characters with no whitespace"
            ));
        }
        if out.iter().any(|(n, t)| n == name || t == token) {
            return Err(format!("line {lineno}: duplicate name or token"));
        }
        out.push((name.to_owned(), token.to_owned()));
    }
    if out.is_empty() {
        return Err("source token file contains no entries".to_owned());
    }
    Ok(out)
}

/// The consumer token file holds one token on one line.
pub fn parse_consumer_token(text: &str) -> Result<String, String> {
    let token = text.trim();
    if !valid_token(token) {
        return Err(format!(
            "consumer token must be at least {MIN_TOKEN_LEN} characters with no whitespace"
        ));
    }
    Ok(token.to_owned())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn valid_token(token: &str) -> bool {
    token.len() >= MIN_TOKEN_LEN && token.bytes().all(|b| b.is_ascii_graphic())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn source_tokens_parse_and_reject_bad_lines() {
        let ok = parse_source_tokens(&format!("pod-a:{TOKEN}\n")).unwrap();
        assert_eq!(ok, vec![("pod-a".to_owned(), TOKEN.to_owned())]);
        assert!(parse_source_tokens("").is_err());
        assert!(parse_source_tokens(&format!("Pod:{TOKEN}")).is_err());
        assert!(parse_source_tokens("pod-a:short").is_err());
        assert!(parse_source_tokens(&format!("pod-a:{TOKEN}\npod-a:{TOKEN}2")).is_err());
    }

    #[test]
    fn consumer_token_is_one_token() {
        assert_eq!(parse_consumer_token(&format!("{TOKEN}\n")).unwrap(), TOKEN);
        assert!(parse_consumer_token(&format!("{TOKEN}\n{TOKEN}")).is_err());
    }

    #[test]
    fn defaults_and_positive_limits() {
        let dir = std::env::temp_dir();
        let src = dir.join("eventrelay-test-src.txt");
        let cons = dir.join("eventrelay-test-cons.txt");
        std::fs::write(&src, format!("pod-a:{TOKEN}\n")).unwrap();
        std::fs::write(&cons, format!("{}\n", "f".repeat(40))).unwrap();
        let env = |k: &str| match k {
            "EVENTRELAY_SOURCE_TOKENS_FILE" => Some(src.display().to_string()),
            "EVENTRELAY_CONSUMER_TOKEN_FILE" => Some(cons.display().to_string()),
            _ => None,
        };
        let cfg = from_env(env).unwrap();
        assert_eq!(cfg.db_path, PathBuf::from("/data/eventrelay.db"));
        assert_eq!(cfg.retention.max_events, 10_000);
        assert_eq!(cfg.retention.max_age_ms, 7 * DAY_MS);
        let bad = |k: &str| {
            if k == "EVENTRELAY_MAX_EVENTS" {
                Some("0".to_owned())
            } else {
                env(k)
            }
        };
        assert!(from_env(bad).is_err());
    }
}
