//! HTTP routes. Bodies and tokens are never logged.

use std::pin::pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::auth::{Auth, is_proxied};
use crate::store::{Event, Store};

pub const MAX_BODY_BYTES: usize = 64 * 1024;
pub const DEFAULT_LIMIT: u64 = 100;
pub const MAX_LIMIT: u64 = 500;
pub const MAX_WAIT_SECS: u64 = 25;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub auth: Arc<Auth>,
}

/// Builds the router. Bodies over 64 KiB get 413 from axum before the
/// handler runs.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/events", post(post_event).get(get_events))
        .route("/v1/ack", post(ack))
        .route("/healthz", get(healthz))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

#[derive(Debug)]
pub enum ApiError {
    Unauthorized,
    Forbidden,
    BadRequest(&'static str),
    Internal,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            ApiError::Forbidden => (
                StatusCode::FORBIDDEN,
                "source tokens cannot be used through a proxy",
            ),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

#[derive(Deserialize)]
struct PostIn {
    body: Value,
}

async fn post_event(
    State(st): State<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    // The source name comes from the token. A "source" field in the body is ignored.
    let source = st.auth.source_for(&headers).ok_or(ApiError::Unauthorized)?;
    if is_proxied(&headers) {
        return Err(ApiError::Forbidden);
    }
    let input: PostIn = serde_json::from_slice(&bytes)
        .map_err(|_| ApiError::BadRequest("body must be JSON with a \"body\" object field"))?;
    if !input.body.is_object() {
        return Err(ApiError::BadRequest("\"body\" must be a JSON object"));
    }
    let store = st.store.clone();
    let now = now_ms();
    let id = tokio::task::spawn_blocking(move || store.insert(&source, &input.body, now))
        .await
        .map_err(|_| ApiError::Internal)?
        .map_err(|e| {
            tracing::error!(error = %e, "insert failed");
            ApiError::Internal
        })?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}

#[derive(Deserialize)]
struct ListQuery {
    since: Option<u64>,
    limit: Option<u64>,
    wait: Option<u64>,
}

async fn get_events(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    if !st.auth.is_consumer(&headers) {
        return Err(ApiError::Unauthorized);
    }
    let since = q.since.unwrap_or(0);
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let wait = Duration::from_secs(q.wait.unwrap_or(0).min(MAX_WAIT_SECS));
    let deadline = Instant::now() + wait;

    loop {
        // Register for the wake-up before querying, so an insert between
        // the query and the wait is not missed.
        let mut notified = pin!(st.store.notify.notified());
        notified.as_mut().enable();

        let events = query_since(&st.store, since, limit).await?;
        if !events.is_empty() || wait.is_zero() || Instant::now() >= deadline {
            return Ok(Json(page(since, &events)));
        }
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {}
            _ = notified.as_mut() => {}
        }
    }
}

async fn query_since(store: &Arc<Store>, since: u64, limit: u64) -> Result<Vec<Event>, ApiError> {
    let store = store.clone();
    tokio::task::spawn_blocking(move || store.since(since, limit))
        .await
        .map_err(|_| ApiError::Internal)?
        .map_err(|e| {
            tracing::error!(error = %e, "read failed");
            ApiError::Internal
        })
}

/// `last_id` is the id of the last event in this page, or `since` when the
/// page is empty. Clients advance `since` with it, so a page cut short by
/// `limit` does not skip events.
fn page(since: u64, events: &[Event]) -> Value {
    let last_id = events.last().map_or(since, |e| e.id);
    let events: Vec<Value> = events
        .iter()
        .map(|e| {
            json!({
                "id": e.id,
                "source": e.source,
                "received_ms": e.received_ms,
                "body": e.body,
            })
        })
        .collect();
    json!({ "events": events, "last_id": last_id })
}

#[derive(Deserialize)]
struct AckIn {
    through: u64,
}

async fn ack(
    State(st): State<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<StatusCode, ApiError> {
    if !st.auth.is_consumer(&headers) {
        return Err(ApiError::Unauthorized);
    }
    let input: AckIn = serde_json::from_slice(&bytes)
        .map_err(|_| ApiError::BadRequest("body must be JSON with a \"through\" integer"))?;
    let store = st.store.clone();
    tokio::task::spawn_blocking(move || store.ack(input.through))
        .await
        .map_err(|_| ApiError::Internal)?
        .map_err(|e| {
            tracing::error!(error = %e, "ack failed");
            ApiError::Internal
        })?;
    Ok(StatusCode::NO_CONTENT)
}

async fn healthz(State(st): State<AppState>) -> StatusCode {
    let store = st.store.clone();
    match tokio::task::spawn_blocking(move || store.health()).await {
        Ok(Ok(())) => StatusCode::OK,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
