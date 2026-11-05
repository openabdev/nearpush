//! Bearer-token authentication for the two token classes.

use axum::http::{HeaderMap, header::AUTHORIZATION};
use subtle::ConstantTimeEq;

/// Source tokens (producers, POST) and the single consumer token (GET, ack).
#[derive(Clone)]
pub struct Auth {
    sources: Vec<(String, String)>,
    consumer: String,
}

impl Auth {
    /// `sources` holds `(name, token)` pairs.
    pub fn new(sources: Vec<(String, String)>, consumer: String) -> Self {
        Self { sources, consumer }
    }

    /// Returns the name of the source token that the request presents.
    ///
    /// Every entry is compared, with no early exit, so the time taken does
    /// not show which entry matched.
    pub fn source_for(&self, headers: &HeaderMap) -> Option<String> {
        let presented = bearer(headers)?;
        let mut hit: Option<&str> = None;
        for (name, token) in &self.sources {
            if bool::from(token.as_bytes().ct_eq(presented.as_bytes())) {
                hit = Some(name);
            }
        }
        hit.map(str::to_owned)
    }

    /// True when the request presents the consumer token.
    pub fn is_consumer(&self, headers: &HeaderMap) -> bool {
        bearer(headers).is_some_and(|presented| {
            bool::from(self.consumer.as_bytes().ct_eq(presented.as_bytes()))
        })
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// True when the request came through a proxy such as `tailscale serve`.
///
/// Producer tokens are refused on proxied requests, so a source token that
/// reaches the tailnet path cannot post events.
pub fn is_proxied(headers: &HeaderMap) -> bool {
    headers.keys().any(|name| {
        let name = name.as_str();
        name.starts_with("tailscale-")
            || name == "x-forwarded-for"
            || name == "x-forwarded-host"
            || name == "forwarded"
    })
}
