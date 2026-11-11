use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use eventrelay::auth::Auth;
use eventrelay::http::{AppState, router};
use eventrelay::store::{Retention, Store};
use serde_json::{Value, json};
use tower::ServiceExt;

const SOURCE: &str = "src-token-0123456789abcdef0123456789";
const CONSUMER: &str = "cons-token-0123456789abcdef0123456789";
const DAY: u64 = 86_400_000;

fn default_retention() -> Retention {
    Retention {
        max_events: 10_000,
        max_age_ms: 7 * DAY,
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    app: Router,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("relay.db"), default_retention()).unwrap());
    let auth = Auth::new(
        vec![("pod-a".to_owned(), SOURCE.to_owned())],
        CONSUMER.to_owned(),
    );
    let app = router(AppState {
        store: store.clone(),
        auth: Arc::new(auth),
    });
    Harness {
        _dir: dir,
        store,
        app,
    }
}

/// Sends one request through the router and returns the status and the JSON body.
/// The JSON body is `Null` when the response has none.
async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Vec<u8>>,
    extra: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    let body = match body {
        Some(bytes) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(bytes)
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn post_event(app: &Router, token: Option<&str>, payload: &Value) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        "/v1/events",
        token,
        Some(serde_json::to_vec(payload).unwrap()),
        &[],
    )
    .await
}

async fn get_events(app: &Router, query: &str) -> (StatusCode, Value) {
    call(
        app,
        "GET",
        &format!("/v1/events{query}"),
        Some(CONSUMER),
        None,
        &[],
    )
    .await
}

async fn ack(app: &Router, through: u64) -> StatusCode {
    call(
        app,
        "POST",
        "/v1/ack",
        Some(CONSUMER),
        Some(serde_json::to_vec(&json!({ "through": through })).unwrap()),
        &[],
    )
    .await
    .0
}

fn ids(store: &Store) -> Vec<u64> {
    store
        .since(0, 10_000)
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect()
}

#[tokio::test]
async fn post_requires_a_source_token() {
    let h = harness();
    let payload = json!({ "body": { "title": "hi" } });

    let (status, _) = post_event(&h.app, None, &payload).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no token");

    let (status, _) = post_event(&h.app, Some("not-a-real-token-0123456789abcdef"), &payload).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "wrong token");

    let (status, _) = post_event(&h.app, Some(CONSUMER), &payload).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "consumer token used for POST"
    );

    let (status, body) = post_event(&h.app, Some(SOURCE), &payload).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["id"], 1);
}

#[tokio::test]
async fn post_is_refused_through_a_proxy() {
    let h = harness();
    let payload = json!({ "body": {} });
    let bytes = serde_json::to_vec(&payload).unwrap();
    let (status, _) = call(
        &h.app,
        "POST",
        "/v1/events",
        Some(SOURCE),
        Some(bytes),
        &[("x-forwarded-for", "192.0.2.9")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(ids(&h.store).is_empty(), "a refused POST stores nothing");
}

#[tokio::test]
async fn get_requires_the_consumer_token() {
    let h = harness();
    let (status, _) = call(&h.app, "GET", "/v1/events", None, None, &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(&h.app, "GET", "/v1/events", Some(SOURCE), None, &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(
        &h.app,
        "POST",
        "/v1/ack",
        Some(SOURCE),
        Some(br#"{"through":1}"#.to_vec()),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn get_with_wait_returns_empty_after_the_wait() {
    let h = harness();
    let started = Instant::now();
    let (status, body) = get_events(&h.app, "?since=0&wait=2").await;
    let elapsed = started.elapsed();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["events"], json!([]));
    assert_eq!(body["last_id"], 0);
    assert!(
        elapsed >= Duration::from_millis(1800),
        "returned after {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(6),
        "returned after {elapsed:?}"
    );
}

#[tokio::test]
async fn get_with_wait_returns_when_an_event_arrives() {
    let h = harness();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = h.app.clone();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let started = Instant::now();
    let waiter = {
        let client = client.clone();
        let url = format!("{base}/v1/events?since=0&wait=10");
        tokio::spawn(async move {
            client
                .get(url)
                .bearer_auth(CONSUMER)
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let posted = client
        .post(format!("{base}/v1/events"))
        .bearer_auth(SOURCE)
        .json(&json!({ "body": { "title": "wake" } }))
        .send()
        .await
        .unwrap();
    assert_eq!(posted.status(), StatusCode::CREATED);

    let got = waiter.await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "long poll did not wake"
    );
    let events = got["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["source"], "pod-a");
    assert_eq!(events[0]["body"]["title"], "wake");
    assert_eq!(got["last_id"], 1);
}

#[tokio::test]
async fn ack_is_monotonic() {
    let h = harness();
    for _ in 0..5 {
        h.store.insert("pod-a", &json!({}), 1_000).unwrap();
    }
    assert_eq!(ack(&h.app, 5).await, StatusCode::NO_CONTENT);
    assert_eq!(ack(&h.app, 3).await, StatusCode::NO_CONTENT);
    assert_eq!(h.store.ack_watermark().unwrap(), 5);
}

#[tokio::test]
async fn prune_respects_the_count_cap() {
    let h = harness();
    for _ in 0..5 {
        h.store.insert("pod-a", &json!({}), 1_000).unwrap();
    }
    let report = h
        .store
        .prune(
            1_000,
            Retention {
                max_events: 3,
                max_age_ms: 7 * DAY,
            },
        )
        .unwrap();
    assert_eq!(report.removed, 2);
    assert_eq!(
        report.removed_unacked, 2,
        "nothing is acked, so the cap forced both"
    );
    assert_eq!(ids(&h.store), vec![3, 4, 5]);
}

#[tokio::test]
async fn prune_respects_the_age_limit() {
    let h = harness();
    h.store
        .insert("pod-a", &json!({ "old": true }), 1_000)
        .unwrap();
    h.store
        .insert("pod-a", &json!({ "old": true }), 1_000)
        .unwrap();
    // Inserting eight days later runs the default 7-day age prune.
    let id = h
        .store
        .insert("pod-a", &json!({ "old": false }), 1_000 + 8 * DAY)
        .unwrap();
    assert_eq!(id, 3);
    assert_eq!(ids(&h.store), vec![3]);
}

#[tokio::test]
async fn acked_events_go_first_and_unacked_only_when_forced() {
    let h = harness();
    for _ in 0..5 {
        h.store.insert("pod-a", &json!({}), 1_000).unwrap();
    }
    assert_eq!(ack(&h.app, 2).await, StatusCode::NO_CONTENT);

    let first = h
        .store
        .prune(
            1_000,
            Retention {
                max_events: 3,
                max_age_ms: 7 * DAY,
            },
        )
        .unwrap();
    assert_eq!(first.removed, 2);
    assert_eq!(first.removed_unacked, 0, "rows 1 and 2 were acked");
    assert_eq!(ids(&h.store), vec![3, 4, 5]);

    let second = h
        .store
        .prune(
            1_000,
            Retention {
                max_events: 2,
                max_age_ms: 7 * DAY,
            },
        )
        .unwrap();
    assert_eq!(second.removed, 1);
    assert_eq!(
        second.removed_unacked, 1,
        "row 3 is above the watermark, so the cap forced it"
    );
    assert_eq!(ids(&h.store), vec![4, 5]);
}

#[tokio::test]
async fn ids_are_not_reused_after_prune() {
    let h = harness();
    for _ in 0..3 {
        assert!(h.store.insert("pod-a", &json!({}), 1_000).is_ok());
    }
    let report = h
        .store
        .prune(
            2_000,
            Retention {
                max_events: 10_000,
                max_age_ms: 0,
            },
        )
        .unwrap();
    assert_eq!(report.removed, 3);
    assert!(ids(&h.store).is_empty());
    assert_eq!(h.store.insert("pod-a", &json!({}), 2_000).unwrap(), 4);
}

#[tokio::test]
async fn body_over_64_kib_is_rejected_with_413() {
    let h = harness();
    let big = "a".repeat(70 * 1024);
    let (status, _) = post_event(&h.app, Some(SOURCE), &json!({ "body": { "text": big } })).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(ids(&h.store).is_empty());

    let fits = "a".repeat(40 * 1024);
    let (status, _) = post_event(&h.app, Some(SOURCE), &json!({ "body": { "text": fits } })).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn body_must_be_a_json_object() {
    let h = harness();
    let cases = [
        json!({ "body": [1, 2] }),
        json!({ "body": "text" }),
        json!({ "body": 5 }),
        json!({ "body": null }),
        json!({ "title": "no body field" }),
        json!([1, 2]),
    ];
    for payload in &cases {
        let (status, _) = post_event(&h.app, Some(SOURCE), payload).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "payload {payload}");
    }
    let (status, _) = call(
        &h.app,
        "POST",
        "/v1/events",
        Some(SOURCE),
        Some(b"not json".to_vec()),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(ids(&h.store).is_empty());
}

#[tokio::test]
async fn source_is_taken_from_the_token_not_the_body() {
    let h = harness();
    let (status, _) = post_event(
        &h.app,
        Some(SOURCE),
        &json!({ "body": {}, "source": "someone-else" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let stored = h.store.since(0, 10).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].source, "pod-a");
}

#[tokio::test]
async fn limit_is_clamped_and_since_must_be_numeric() {
    let h = harness();
    for _ in 0..510 {
        h.store.insert("pod-a", &json!({}), 1_000).unwrap();
    }
    let (status, body) = get_events(&h.app, "?since=0&limit=100000").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["events"].as_array().unwrap().len(), 500);
    assert_eq!(body["last_id"], 500, "last_id is the last id of the page");

    let (status, body) = get_events(&h.app, "?since=0").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["events"].as_array().unwrap().len(),
        100,
        "default limit is 100"
    );

    let (status, _) = get_events(&h.app, "?since=abc").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn healthz_needs_no_token() {
    let h = harness();
    let (status, _) = call(&h.app, "GET", "/healthz", None, None, &[]).await;
    assert_eq!(status, StatusCode::OK);
}
