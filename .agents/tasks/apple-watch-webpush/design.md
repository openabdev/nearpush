# Design: Apple Watch support through Web Push to the home-screen web app

Status: draft for council review, round 3. Base: `eventrelay` @ 89c8b673. Evidence:
[spike-report.md](spike-report.md) (Q1 PASS, Q2 PASS, both observed on pahud's carried
iPhone, iOS 18.7, with a paired Apple Watch). Nothing here is implemented or merged.

Two decisions are left to pahud and are not made here: which devices receive pushes
(Q3, see "Open decisions"), and whether the default payload text should be the
summary-or-tag default argued below or something else (D2).

## Overview

nearpushd gains an optional Web Push sender. The existing home-screen web app (same
origin, `https://<laptop>.<tailnet>.ts.net/`, same manifest) registers a service worker
and, after the user taps **Enable notifications** in the drawer, subscribes with the
daemon's VAPID public key and posts the subscription to the daemon. When an event in
state `your_turn`, `allow` or `all_set` passes the pause flag, the display filter and a
freshness check, the daemon encrypts a small padded payload (RFC 8291, `aes128gcm`) and
posts it, signed with VAPID (RFC 8292), to the subscription's endpoint on
`web.push.apple.com`. iOS shows it as a notification of the nearpush web app and mirrors
it to the paired Watch when the iPhone is locked (spike Q1). No native iOS or watchOS
code. `working` is never pushed. The feature is off by default
(`[webpush] enabled = false`).

```
nearpush hook / send ─▶ POST /push ─┐
relay puller (origin_ms) ───────────┴▶ accept_event ─▶ store ─▶ WS viewers (unchanged)
                                                     └▶ webpush::notify (try_send only)
                                                          │ queue (64) → dispatcher
                                                          │   coalesce, age check
                                                          ▼
                                       one lane per subscription (≤ 8, concurrent)
                                                          ▼
                                     HTTPS POST web.push.apple.com (VAPID, encrypted)
                                                          ▼
                                     iPhone (home-screen app) ─mirror─▶ Apple Watch
```

## Technology stack (locked on approval)

- Rust, inside `crates/nearpushd`. No new crate in the workspace.
- New dependencies:
  - `web-push-native = { version = "0.5", default-features = false }`: RFC 8291 payload
    encryption only (`WebPushBuilder::new(..).build(body)` returns an `http::Request`).
    Pure Rust (p256, aes-gcm, hkdf, ece-native).
  - `p256 = { version = "0.13", features = ["ecdh", "ecdsa", "pkcs8", "pem"] }`, the
    same major version web-push-native uses, for the VAPID key and the ES256 JWT.
    These features do not enable `rand_core/getrandom`, so key generation does not use
    `SigningKey::random(&mut OsRng)`. It fills 32 bytes with the existing `getrandom`
    dependency, calls `SigningKey::from_bytes(&bytes.into())`, and retries on `Err` (a
    zero or out-of-range scalar, probability about 2^-128). ES256 signing uses
    `signature::Signer::sign`, which is deterministic (RFC 6979) and needs no RNG.
  - `base64ct = { version = "1", features = ["alloc"] }` for base64url.
  - `http = "1"` (already in the tree through axum) to adjust the built request.
  - `sha2` is already a nearpushd dependency (used for `key_id` and the notification tag).
- The HTTP client is the existing `reqwest` 0.12 with rustls. A dedicated client:
  10 s total timeout, `redirect::Policy::none()`, no proxy from env (`no_proxy()`).
- **The VAPID JWT is built by hand** (about 30 lines) instead of enabling
  web-push-native's `vapid` feature. That feature pulls `jwt-simple` with
  `default-features = false, features = ["pure-rust"]`, so it would add no BoringSSL;
  the reason for skipping it is size: jwt-simple brings a multi-algorithm tree (RSA,
  Ed25519, several hash and encoding crates) for one ES256 signature that `p256`
  already does. The hand-built token:
  - header `{"typ":"JWT","alg":"ES256"}`;
  - claims `{"aud": <endpoint origin>, "exp": now + 12 h, "sub": <subject>}`, where the
    endpoint origin is the scheme, the host and the port if it is not the scheme's
    default (`https://web.push.apple.com`, or `http://127.0.0.1:41234` for the test
    mock), with no path and no trailing slash;
  - signed with `p256::ecdsa::SigningKey` (fixed 64-byte r‖s, the JWS ES256 form);
  - sent as `Authorization: vapid t=<jwt>, k=<base64url uncompressed public key>`.
  - The 12 h `exp` is what the spike sent: pywebpush 2.5.0 sets
    `exp = now + 12 * 60 * 60` when none is given (`pywebpush/__init__.py`, line 490 in
    the spike venv), and Apple answered 201 to all three spike pushes.
- **MSRV is raised for nearpushd only.** web-push-native 0.5 declares MSRV 1.87.
  `crates/nearpushd/Cargo.toml` replaces `rust-version.workspace = true` with
  `rust-version = "1.87"`. The workspace value stays `1.85`, so `crates/eventrelay` and
  `crates/nearpush` keep declaring 1.85 and `deploy/eventrelay/Dockerfile` stays on
  `rust:1.85-bookworm` with `cargo build --release --locked -p eventrelay`. The other
  option, raising the workspace and the Dockerfile to 1.87, was rejected: it changes the
  relay image for a feature the relay does not use, and it forces a relay rebuild and p1
  import that this feature does not otherwise need.
- **The shared lockfile is the remaining risk.** web-push-native raises the locked
  versions of crates that eventrelay also uses (`serde ^1.0.229`, `serde_json ^1.0.151`,
  `http ^1.4.2`). If one of those declares an MSRV above 1.85, the relay's `--locked`
  build on 1.85 fails. The implementation plan therefore has a gate step: build
  eventrelay with `--locked` on rustc 1.85 (step 2). On failure, the fix is to pin the
  offending crate in `Cargo.lock` to the newest version that builds on 1.85
  (`cargo update -p <crate> --precise <version>`), provided web-push-native's
  requirement still resolves. If it does not resolve, implementation stops and the
  choice between a relay toolchain bump and another crate goes back to review.
  macmini has rustc 1.100 for nearpushd.
- Page side: plain JS in `assets/index.html` plus a new `assets/sw.js`, both embedded
  with `include_str!` like the other assets.

## Key decisions

**Where the hook sits.** `server::accept_event` is the single path for `POST /push` and
the relay puller, so the push decision is made there, after `store.push` returns the
stored `Event`. It calls `state.webpush.notify(&ev, ctx)`, which never blocks and never
fails the request: it only does a `try_send` on a bounded channel. `ctx` carries two
transient values that are not stored with the event: `origin_ms` (see freshness) and
`summary` (the summarized session label, if one existed when the event was accepted;
see payload). Events loaded from `events.jsonl` at start never pass through
`accept_event`, so a restart sends nothing. The relay's dedupe (cursor plus `relay_id`
in the ring) already stops `accept_event` from running twice for one relay event, so a
relay replay after a crash does not push twice.

**Event origin time.** Relay events are stamped with the time they are pulled
(`relay::to_new_event` calls `validate(now_ms())`), and eventrelay keeps events for up to
7 days, so the stored `ts` cannot tell an old relay event from a new one. The change:

- `relay::RelayEvent` gains `#[serde(default)] received_ms: Option<u64>`. eventrelay
  already sends it (`crates/eventrelay/src/http.rs:156`).
- `NewEvent` gains `#[serde(skip)] pub origin_ms: Option<u64>`, set only by
  `to_new_event` from `received_ms`, like `relay_id`. Clients cannot send it.
  `Event` and `events.jsonl` do not change.
- In `accept_event`, `origin_ms = new.origin_ms.unwrap_or(ev.ts)`. A direct `POST /push`
  is therefore "now". A relay event without `received_ms` (an older relay) is also
  treated as "now"; the eventrelay in this repo always sends it.
- Age is `now_ms.saturating_sub(origin_ms)`, so a relay clock ahead of the laptop gives
  age 0. Skew of a few seconds between hosts is small next to the 300 s bound.

**Which events push.** `should_push(ev, origin_ms, now_ms, controls, cfg)` is true only
when all hold:

1. `ev.muted` is false (pause suppresses pushes, same as the display).
2. `ev.state` is `Some(s)`. Generic level events (no state) never push.
3. `s` is in `[webpush] states` (default and maximum: `your_turn`, `allow`, `all_set`;
   `working` is rejected at config load).
4. `s` is in the current display filter (`ControlState.states`, i.e. `[filter] states`
   or the `/control` override).
5. The event is fresh: its age is at most `ttl_secs` (default 300 s). This is the same
   bound Apple applies to a queued push. After the lid has been closed overnight, the
   relay backlog still reaches the page as cards, but older events do not buzz the
   Watch. The lanes check age again before each send attempt, so a job that waited
   behind a slow send or the retry is dropped once it passes the bound
   (`dropped_stale` += 1).

**Pause and filter are re-checked at send time.** `muted` is fixed when the event is
accepted, but a job can wait in a lane behind a 10 s timeout and the 2 s retry. Each
lane therefore reads the live `ControlState` (`AppState.control`, an
`Arc<RwLock<ControlState>>` that `WebPush` holds a clone of) before every send attempt,
the retry included. If `paused` is true, or the job's state is no longer in
`ControlState.states`, the event job is dropped (`dropped_paused` += 1). Pausing thus
stops pushes that are already queued, not only new ones. Test jobs skip this check (see
`/webpush/test`).

Rule 4 makes the menu's existing *Show on iPhone* checkboxes and `POST /control
{"states"}` also govern pushes, so there is one switch per state for "what my iPhone
tells me". The alternative, an independent runtime push filter in `/control`, needs new
menu UI and a second mental model; it is backlogged. Consequence to document: unchecking
`all_set` in the menu stops both the card and the push.

**Unpausing does not push.** Events stored while paused stay muted forever (existing
rule), and they are never pushed later either.

**Coalescing.** The dispatcher waits 500 ms after the first job of a burst, then drains
the queue and keeps only the newest job per coalescing key: the event's `session` if
set, otherwise its state. A fresh relay backlog therefore yields at most one push per
session, and a `your_turn` immediately followed by `all_set` for the same session sends
only `all_set`. The 500 ms costs nothing noticeable on a wrist.

**One lane per subscription.** A slow or dead endpoint must not delay the others. Each
subscription has its own lane: a tokio task with a 4-slot channel. The dispatcher
`try_send`s each coalesced job into every lane and never waits on the network, so it
keeps draining the 64-slot queue. A full lane drops the job for that subscription only
(`dropped_queue` += 1). Lanes run concurrently; at most 8 subscriptions exist, so at most
8 sends are in flight, which is the concurrency bound. Within a lane, sends are in
order. Lane state is per subscription: rate-limit window, `Retry-After` hold, and the
retry. The lane owns only the subscription `id`; it reads the current record by id
before each send (see "Lane and record ownership" under Sender), so an upsert that
changes the keys takes effect on the next send without restarting the lane.

**Rate limit.** At most 20 pushes per subscription per rolling 60 s, counted in the
lane. Excess jobs are dropped and counted (`dropped_rate`), logged once per minute at
warn. Apple's limits are not documented; 20/min is far above real agent traffic and
far below a runaway loop.

**`Retry-After` drops, it does not wait.** On a 429 the lane sets
`hold_until = now + Retry-After` (seconds; 60 when absent or unparseable; capped at
600). While `now < hold_until`, the lane takes each job off its channel and drops it at
once: event jobs count as `dropped_rate`, and test jobs reply `rate_limited` with
`status: null`. The lane never sleeps with jobs in its 4 slots. This is consistent with
"no retry queue": a hold of up to 600 s is longer than the 300 s freshness bound, so a
job kept through the hold would be stale anyway.

**No retry queue.** A late notification is noise, which is also why stale events are
dropped. A send gets at most one retry (2 s later, inside its own lane) on network
error or 5xx, then it is dropped.

**TTL and urgency.** `TTL: <ttl_secs>` (default 300, as in the spike) and
`Urgency: high` (as in the spike). No `Topic` header: Apple's handling of it was not
tested.

## Payload and privacy

The payload is JSON, built by `webpush::payload(ev, summary, content)`. The fields are
truncated so the JSON is at most 960 bytes, and then padded with a `"p"` field of ASCII
`0`s to exactly 1024 bytes. Every push therefore has the same ciphertext size
(web-push-native writes one record, `message.len() + 17` bytes, plus the fixed
`aes128gcm` header), and Apple cannot read label length from it. Apple allows 4 KiB.

```json
{"v":1,"id":123,"state":"your_turn","title":"your turn","body":"#ab12 · frontend","tag":"np-3f9a0c1e7b2d4a58","url":"/","p":"000…"}
```

What the code puts in each field today (checked against the base):

- `session_label` on an event is the summarizer's label only if `SessionStore::label`
  already had one when the event was accepted. Otherwise it is the hook's provisional
  label: the user's first prompt line, verbatim and truncated (hook.rs tests show
  `"修正登入轉址的問題，然後補測試"`). The summarizer runs after the event is stored, so
  the first event of a session always carries the raw line, and with the summarizer off
  every event does.
- `detail` for `allow` is `tool_detail(tool, input)`: the tool name plus the command's
  first line, a shortened file path, a URL without query, or a search pattern, e.g.
  `execute_bash: GITHUB_TOKEN=… gh pr list` or `read_file: src/main.rs`. Env values are
  redacted; commands and paths are not. For `your_turn` it can be `N wf waiting`; for
  `all_set` it is `took Xm`.
- `project` is the working directory's name.

Fields:

- `title`: fixed per state, `your turn` / `allow?` / `all set`. Never the event title.
- `body`, by `[webpush] content`:
  - `"state"`: empty. The Watch shows only "nearpush: your turn"-style text.
  - `"summary"` (default): the summarized label if `ctx.summary` is set, otherwise the
    session tag `#xxxx` (the page's own `sessionTag` rule: last 4 alphanumerics of the
    session), then `project`, joined with ` · `, cut to 120 characters with an
    ellipsis. Raw prompt text never reaches the wrist or the lock screen under this
    setting.
  - `"label"`: as `"summary"`, but uses the event's `session_label` as stored, which
    may be the raw first prompt line.
  - `"detail"`: as `"label"`, then the `detail` line, cut to 160 characters total. This
    shows commands and file paths on the Watch.
- `tag`: `np-` + the first 16 hex characters (64 bits) of SHA-256 over the full session
  id (or `np-<state>` when there is no session), so a newer notification for the same
  session can replace the older one where iOS honours `tag`. Sessions that share a
  suffix get different tags. A 64-bit prefix makes a collision unlikely, not
  impossible: for n sessions the chance is about n²/2^65, under 10^-12 for 10,000
  sessions. The accepted consequence of a collision is that one session's notification
  replaces another's on the device.
- `url`: always `/`. The event's own `url` field is not sent.
- Never sent: `body`, `prompt` (never stored anyway), `tags`, `host`, `sender`, the
  event `title`, `url`, `subtitle`.

**Why `"summary"` is the default (D2, pahud may override).** The lock screen and the
Watch face are readable by anyone nearby, and a raw first prompt line is the user's own
words about the task, often more specific than they would put on a wrist. The summary
is short and generated for display, and the session tag plus project name is enough to
tell sessions apart. The cost: the first push of a new session reads `#ab12 · frontend`
rather than a description, because the summary is not ready yet. `"state"` is safer
still but makes two waiting sessions indistinguishable on the Watch. This choice is put
to pahud as D2; the implementation only changes a default string.

What third parties see:

- **Apple's push service** carries the payload as RFC 8291 ciphertext encrypted to the
  subscription's `p256dh`/`auth` keys; by the protocol it cannot read the title or body.
  It does see metadata: the endpoint (a device-scoped token), the laptop's public IP,
  the time of each push, the constant size, `TTL`, `Urgency`, and the VAPID `sub` claim
  (the configured contact, e.g. a mailto address). This is protocol behaviour; the spike
  did not inspect Apple's side.
- **The iPhone lock screen and the Watch face** show the decrypted title and body to
  anyone who looks. Under the default that is the state, the summary or session tag,
  and the project name. Under `"label"` it can be the raw first prompt line; under
  `"detail"` it adds commands and file paths. iOS *Show Previews: When Unlocked* hides
  the body on the locked iPhone; whether the Watch follows it was not tested.
- **Logs** never contain the endpoint path, the keys, the JWT, the payload, or any part
  of a push service response body. They name the subscription id, its label, the
  endpoint host (validated against the allowlist), the HTTP status and a local error
  class.

## Storage

**VAPID key.** `<config_dir>/webpush-vapid.pem` (default `~/.config/nearpush/`), a
PKCS#8 PEM P-256 private key, beside the push token because it is the same class of
secret. Created only when `[webpush] enabled = true` and the file is missing:
`OpenOptions::create_new(true).mode(0o600)`, parent dir created 0700 (same code shape as
`load_or_create_token`). An existing file with group/other bits set is chmod'ed to 0600
and a warn is logged. The key is never printed; only its public half is served.

**Key id.** `key_id` = first 16 hex chars of SHA-256 over the uncompressed public key.
It ties subscriptions to the key that created them.

**Subscriptions.** `<state_dir>/webpush.json` (default `~/.local/state/nearpush/`),
0600, written atomically (tmp + fsync + rename, same as `control.json`):

```json
{"v":1,"key_id":"9f1c…","subscriptions":[
  {"id":"5d0e8a1f3b2c4e67","endpoint":"https://web.push.apple.com/…","p256dh":"B…","auth":"…",
   "login":"<your-email>","label":"pahud iPhone","created_ms":0,"updated_ms":0,
   "last_ok_ms":0,"last_error_class":null,"last_error_ms":0}]}
```

- `id`: 16 random hex chars from `getrandom`, used in `/status` and for removal. Ids
  are never reused: a removed endpoint that subscribes again gets a new id.
- At most 8 subscriptions. Upsert is keyed by `endpoint`: an existing endpoint keeps
  its `id`, and its `p256dh`, `auth`, `label` and `login` are replaced by the new
  values. The endpoint of an id never changes.
- `rev` (in memory only, not in the file; starts at 0 on load): incremented whenever an
  upsert changes `p256dh` or `auth`. Lanes use it so that a send made with old keys
  cannot change or remove the newer record (see "Lane and record ownership").
- Load at start: missing file → empty. Unparseable file → renamed to
  `webpush.json.bad-<ms>`, start with empty, log error (the device relearns itself on
  its next page load, see the page section). `key_id` different from the current key →
  all subscriptions discarded, file rewritten, warn logged (they would all fail with
  403 anyway).
- Writes happen on subscribe, unsubscribe, removal, and when a send result changes
  `last_ok_ms` by more than 10 minutes or changes `last_error_class`, so a busy session
  does not rewrite the file on every push. In-memory state is authoritative; a failed
  background write logs an error and is retried on the next change.

**One writer at a time.** The in-memory set lives in a `std::sync::Mutex<SubSet>` that is
only held for short in-memory updates, never across I/O. Every save, from route
handlers and from lanes, goes through `WebPush::persist()`:

1. acquire `save_lock` (a `tokio::sync::Mutex<()>`, the same pattern as `control_write`);
2. under the `SubSet` lock, serialize a snapshot to bytes, then release that lock;
3. write tmp, fsync, rename inside `spawn_blocking`;
4. release `save_lock`.

Because the snapshot is taken after `save_lock` is held, the last save to finish writes
the newest state, and no two writers ever share the tmp path. Route handlers await
`persist()` so they can return 500 on failure; lanes await it too (a short disk write
inside one lane does not delay the others).

## HTTP routes

Page-facing routes use `authorize_viewer` (loopback peer plus allowlisted
`Tailscale-User-Login`, or the local-testing flag). State-changing page routes also
require a same-origin browser `Origin`, which is the CSRF defence: a page on another
site opened in Safari on the iPhone could otherwise send a request through the tailnet
that carries pahud's login header.

| Route | Auth | Purpose |
|---|---|---|
| `GET /sw.js` | viewer | service worker, `text/javascript`, `Cache-Control: no-cache` |
| `GET /webpush/key` | viewer | `{"enabled":bool,"key":"<b64url public key>"|null,"key_id":…}` |
| `POST /webpush/subscribe` | viewer + Origin required and matching + `Content-Type: application/json` | upsert a subscription |
| `POST /webpush/unsubscribe` | viewer + same Origin rule | `{"endpoint":…}` removes it |
| `POST /webpush/test` | local (loopback, token, no proxy headers) | sends a test push to all subscriptions, ignoring pause, filter and freshness |
| `POST /webpush/remove` | local | `{"id":…}` removes a subscription |

**`POST /webpush/test` path.** Test pushes do not go through `notify` or the
dispatcher, so coalescing cannot drop them and each one has its own reply channel.
`WebPush::test_all()`:

1. Under the `SubSet` lock, takes a snapshot: a list of `(id, label, lane sender clone)`
   for every subscription, in `created_ms` order. Then it releases the lock.
2. For each entry, it builds an independent `LaneJob::Test { payload, reply }` with a
   fresh `oneshot` pair (payload: title `nearpush test`, empty body, padded to 1024
   bytes) and calls `try_send` on that lane. If the lane is full, that entry is
   `lane_full` at once. If the lane has already closed (send error), that entry is
   `removed`.
3. Awaits all receivers together, at most 25 s in total.

Test jobs skip the freshness, pause and filter checks. They still use the lane's rate
limit, `Retry-After` hold, rev check and send logic, so a test exercises the same code
as a real push.

The response lists exactly the subscriptions in the snapshot, in snapshot order. A
subscription added after the snapshot is not tested and not listed. A subscription
removed after the snapshot is listed: if its lane ends before replying, the receiver
sees the sender dropped and the entry is `removed`. If its lane sends and gets 404/410,
the entry is `gone`. Response 200:

```json
{"results":[{"id":"5d0e8a1f3b2c4e67","label":"pahud iPhone","status":201,"class":"ok"},
            {"id":"a1b2c3d4e5f60718","label":"old iPad","status":null,"class":"network"}]}
```

- `status`: the push service's HTTP status of the last attempt, or `null` when no
  response arrived.
- `class`: `ok`, `network`, `http_5xx`, `gone` (404/410, the subscription was removed),
  `rejected` (400/403), `rate_limited` (429, an active `Retry-After` hold, or the local
  limit), `payload_too_large`, `encryption` (the stored keys could not be used; the
  subscription was removed), `lane_full`, `removed` (the subscription was removed by
  another path before its lane replied), `superseded` (the keys changed during the
  send; the result was discarded), or `pending` (no reply within 25 s).
- Never the endpoint, its host or path, the keys or any response body. With no
  subscriptions the result list is empty. When disabled: 409 `{"error":"web push is disabled"}`.

`GET /status` gains a `webpush` object: `enabled`, `key_id`, `sent`, `failed`,
`dropped_rate`, `dropped_queue`, `dropped_stale`, `dropped_paused`, `last_ok_ms`, `last_error_class`, and
`subscriptions` as `[{id, label, login, endpoint_host, created_ms, last_ok_ms,
last_error_class}]`. No endpoint path, no keys. The Swift `DaemonStatus` decoder ignores
unknown keys, so the menu app needs no change in this phase.

CSP (`page_headers`) adds `worker-src 'self'`. Without it `default-src 'none'` blocks
the service worker registration, because `script-src` allows only inline scripts.
`assets::ui_version()` hashes `sw.js` too, so a changed worker also reloads the page.

When `[webpush] enabled = false`: `/sw.js` is still served (the page may already have a
registered worker; a 404 would leave a stale one), `/webpush/key` returns
`enabled:false`, `subscribe` and `test` return 409 `{"error":"web push is disabled"}`,
and `notify` is a no-op.

### Input validation

`POST /webpush/subscribe` body, route limit 4 KiB (413 above):

```json
{"subscription":{"endpoint":"…","expirationTime":null,"keys":{"p256dh":"…","auth":"…"}},"label":"pahud iPhone"}
```

| Field | Rule | On failure |
|---|---|---|
| top level | only `subscription` and `label` (`deny_unknown_fields`) | 400 |
| `subscription` | object; unknown inner fields ignored (browser `toJSON` may add some) | 400 |
| `endpoint` | required string ≤ 1024 bytes; parses as URL; scheme `https`; no userinfo; no explicit port other than 443; host exactly in `[webpush] endpoint_hosts` (case-insensitive) | 400 `endpoint not allowed` |
| `keys.p256dh` | required; base64url (padding tolerated) → 65 bytes; `p256::PublicKey::from_sec1_bytes` succeeds | 400 |
| `keys.auth` | required; base64url → exactly 16 bytes | 400 |
| `expirationTime` | optional, null or number; stored nowhere | — |
| `label` | optional string, trimmed, ≤ 32 chars, no control characters; default `"device"` | 400 |
| capacity | a new endpoint when 8 are stored | 409 `too many subscriptions` |
| `Origin` | present and its host equals `Host` (or `X-Forwarded-Host`) | 403 |
| `Content-Type` | `application/json` (a non-simple type, so a cross-site form cannot send it without a preflight, which the daemon never answers) | 415 |

The endpoint allowlist is the SSRF defence: the daemon only ever posts to hosts in
`endpoint_hosts`, with redirects off. Error responses from this route never echo the
submitted endpoint.

Success: 200 `{"id":…,"created":bool}`. `login` is taken from the request's
`Tailscale-User-Login` header (or `"loopback"` with the local-testing flag), not from
the body.

`POST /webpush/unsubscribe`: `{"endpoint": string ≤ 1024}`; unknown endpoint is 200
`{"removed":false}` so the page cannot probe. `POST /webpush/remove`: `{"id": 16 hex}`;
unknown id is 404.

## Config keys

```toml
[webpush]                       # every key optional; the section needs this nearpushd version
enabled = false                 # true: create the VAPID key and send pushes
subject = "mailto:you@example.com"  # VAPID contact sent to Apple; required when enabled
states = ["your_turn", "allow", "all_set"]  # subset of these three; "working" is rejected
content = "summary"             # "state" | "summary" | "label" | "detail" (see Payload and privacy)
ttl_secs = 300                  # 30..=3600; also the freshness bound for pushing an event
endpoint_hosts = ["web.push.apple.com"]  # push services the daemon may post to
```

Validation in `Config::validate`, failing start with a clear message (existing pattern):
`subject` must start with `mailto:` or `https://` and be ≤ 256 bytes when `enabled`;
`states` may be empty (nothing pushes) but must not contain `working` and must not
repeat; `content` is one of the four values; `ttl_secs` in range; `endpoint_hosts`
non-empty when enabled, each a bare lowercase hostname (no scheme, port or path).
`WebPushConfig` uses `#[serde(default, deny_unknown_fields)]` like the other sections.
`subject` has no default because it is sent to Apple: choosing it is a privacy decision
the user makes. Only `mailto:` was verified against Apple in the spike.

Rollback note for README: the older daemon rejects a `[webpush]` section, so remove it
before rolling back (same as `[filter]`/`[hook]` today).

## Page and service worker

`assets/sw.js`:

- `install` → `skipWaiting()`; `activate` → `clients.claim()`.
- `push`: parse JSON; on any parse failure use title `nearpush` and an empty body. It
  **always** calls `showNotification` inside `event.waitUntil`, because iOS revokes a
  subscription whose pushes show nothing. Options: `body`, `tag`, `icon: /icon-192.png`,
  `data: {url}`. The `p` filler is ignored.
- `notificationclick`: close, then focus an existing client or `openWindow('/')`. The
  page then reconnects with `since=<lastId>` and replays as today.
- No `fetch` handler: the worker never caches or intercepts the page.

`index.html`:

- On load: if `'serviceWorker' in navigator`, register `/sw.js` (scope `/`).
- A notifications row in the drawer (next to the connection info), shown only when
  `navigator.standalone` or display-mode standalone, and `PushManager` exists. States:
  - `GET /webpush/key` says disabled → row hidden.
  - permission `default` or no subscription → button **Enable notifications** and an
    optional device-label text field (prefilled `iPhone`). The tap runs
    `Notification.requestPermission()` then `pushManager.subscribe({userVisibleOnly:
    true, applicationServerKey})` inside the gesture, then `POST /webpush/subscribe`.
  - subscribed → "notifications on (label)" and a **Turn off** button
    (`subscription.unsubscribe()` plus `POST /webpush/unsubscribe`).
  - permission `denied` → "notifications blocked in iOS Settings".
- On every load with permission `granted` and an existing subscription
  (`pushManager.getSubscription()`), the page first fetches `/webpush/key` and compares
  the bytes of `subscription.options.applicationServerKey` with the decoded key. The
  re-POST comes only after this check:
  - **Match:** re-POST the subscription (idempotent upsert), so a daemon that lost
    `webpush.json` relearns it.
  - **Mismatch** (the key was rotated): do not re-POST. Call `subscription.unsubscribe()`
    and `POST /webpush/unsubscribe` (in case the daemon still holds it), then show
    **Enable notifications** again, because a new subscribe needs a user gesture. An
    old-key subscription is therefore never stored again and never takes one of the 8
    slots.
  - **`options.applicationServerKey` missing or null** (not expected on iOS 18.7, but
    not verified in the spike): do not re-POST and do not unsubscribe. The drawer shows
    "notifications on". The only cost is that a daemon that lost `webpush.json` does not
    relearn this device until the user taps Turn off and Enable. Device acceptance
    checks which branch iOS takes.
  - `/webpush/key` fails or says disabled: do nothing this load.
- Safari tab (not standalone): a one-line hint "Add to Home Screen to get
  notifications", since iOS offers Web Push only to home-screen apps (spike's wrong-URL
  trap).

## Sender (`crates/nearpushd/src/webpush.rs`)

`WebPush` lives in `AppState` as `Option<Arc<WebPush>>` (`None` when disabled), holding
the signing key, `key_id`, config, a clone of `AppState.control`, the `SubSet` mutex,
`save_lock`, counters (atomics), the JWT cache, and the `mpsc::Sender<Job>` (capacity
64) to the dispatcher. `SubSet` holds both the subscription records and the lane
senders keyed by subscription id, so a record and its lane are added and removed under
one lock.

Job types (the padded payload is built before queueing, so neither the dispatcher nor
the lanes hold `Event`s):

- `Job` (dispatcher queue): `{ coalesce_key: String, state: AgentState, origin_ms: u64,
  payload: Arc<[u8]> }`. No reply channel.
- `LaneJob` (per-lane channel, 4 slots):
  `Event { state, origin_ms, payload: Arc<[u8]> }` from the dispatcher, or
  `Test { payload: Arc<[u8]>, reply: oneshot::Sender<TestResult> }` from `test_all()`.
  Each test job is built for one lane, so each has its own oneshot.

Dispatcher task: receive, wait 500 ms, drain, coalesce, drop jobs older than `ttl_secs`
(`dropped_stale`), then `try_send` a `LaneJob::Event` (cloning the `Arc` payload) into
every lane sender in a snapshot taken under the `SubSet` lock.

### Lane and record ownership

- **A lane owns only its subscription `id`.** It does not keep the endpoint or keys.
- **Lanes are created** by `WebPush::add_sub` (a new endpoint, new id) and at load, in
  the same critical section that inserts the record. An upsert of an existing endpoint
  does not create, replace or restart a lane.
- **Lanes are removed only by `WebPush::remove_sub(id, expect_rev: Option<u64>)`.**
  Under the `SubSet` lock it removes the record and the lane sender together, but only
  if the record exists and, when `expect_rev` is `Some`, its `rev` still equals it.
  Then it calls `persist()`. The unsubscribe and remove routes call it with `None`.
  A lane calls it with `Some(rev it sent with)` after 404/410 or an encryption error,
  so an old send cannot remove a record whose keys were replaced meanwhile. If the new
  keys hit the same 410, the next send removes the record with the new rev.
- **A lane exits** when `recv()` returns `None` (all senders dropped, i.e. it was
  removed by a route) or right after its own successful `remove_sub`. Jobs still in its
  channel are dropped; their test replies resolve as `removed`.

Lane task, per job:

1. Read the record under the `SubSet` lock, by `id`: clone its `endpoint`,
   `p256dh`, `auth` and `rev`. If the id is gone, drop the job (test reply `removed`)
   and exit the lane.
2. Checks, in order. For event jobs only: age (`dropped_stale`), then live pause and
   filter from `ControlState` (`dropped_paused`). For all jobs: an active `Retry-After`
   hold (drop; event `dropped_rate`, test `rate_limited`), then the 20/60 s rate limit
   (`dropped_rate` or `rate_limited`).
3. `WebPushBuilder::new(endpoint_uri, p256dh, auth).with_valid_duration(ttl).build(payload)`.
   An error here is class `encryption`.
4. Re-check the endpoint host against `endpoint_hosts` (the authoritative SSRF check:
   a hand-edited `webpush.json` must not bypass it).
5. Add `Authorization: vapid t=…, k=…` (JWT cached per endpoint origin, renewed when
   less than 1 h of its 12 h remains) and `Urgency: high`.
6. Convert to `reqwest::Request`, send with the dedicated client, classify the status,
   and count `sent` or `failed`. The response body is dropped unread.
7. On network error or 5xx, sleep 2 s and repeat once from step 1. The retry therefore
   re-reads the record and re-checks age, pause and filter.
8. Apply the result under the `SubSet` lock, by `id`:
   - id gone (removed during the send): change nothing, do not persist. A test reply
     carries the send's own status and class.
   - `rev` differs from the one read in step 1 (keys replaced during the send): change
     nothing, do not remove. A test reply is `superseded`.
   - otherwise: on 404/410 or an encryption error call `remove_sub(id, Some(rev))` and
     exit; on 429 set `hold_until`; update `last_ok_ms` / `last_error_class` and call
     `persist()` if the write rule says so. Reply on the oneshot if this is a test job.

## Error handling

| Operation | Failure | Recoverable? | Caller gets | Log |
|---|---|---|---|---|
| config load | invalid `[webpush]` key or value | fatal at start | process exits 1 with the message (existing behaviour) | error |
| VAPID key load/create | unreadable file, bad PEM, create fails | fatal at start when enabled (a silently disabled feature would look like Apple failing) | exit 1, path named, key content never printed | error |
| VAPID key file mode | group/other bits set | yes | chmod 0600 | warn |
| `webpush.json` load | missing | yes | empty set | none |
| `webpush.json` load | unparseable | yes | moved to `.bad-<ms>`, empty set | error |
| `webpush.json` load | `key_id` mismatch | yes | subscriptions discarded | warn |
| `webpush.json` save | I/O error | yes | subscribe/unsubscribe/remove return 500 `cannot save webpush.json`; background saves retry on the next change | error |
| `notify` | queue full | yes | nothing (push dropped), `dropped_queue` += 1 | warn, at most once per minute |
| dispatcher / lane | event older than `ttl_secs` | yes | dropped, `dropped_stale` += 1 | debug |
| lane | paused, or the state left the display filter, while the job waited | yes | event job dropped, `dropped_paused` += 1 (test jobs exempt) | debug |
| dispatcher | lane full | yes, that subscription only | dropped for it, `dropped_queue` += 1 | warn with id, at most once per minute |
| lane | rate limit hit, or a `Retry-After` hold is active | yes | job dropped at once, `dropped_rate` += 1; test reply `rate_limited` | warn with id, at most once per minute |
| lane: build request | encryption error (bad stored key) | per subscription | `remove_sub(id, Some(rev))`: removed and saved only if the keys did not change meanwhile; test class `encryption` | error with id |
| lane: send | network error, timeout, 5xx | one retry after 2 s, then drop | `failed` += 1, `last_error_class` = `network` / `http_5xx` | warn with id, host, status |
| lane: send | 404 or 410 | no: the subscription is gone | `remove_sub(id, Some(rev))`, then the lane exits; a no-op if the record was already removed or its keys changed | info with id |
| lane: send | 413 | no: payload builder bug | dropped, class `payload_too_large` | error with id, status |
| lane: send | 429 | yes | `hold_until = now + Retry-After` (seconds; default 60, capped 600); jobs arriving during the hold are dropped, not held; class `rate_limited` | warn with id, status |
| lane: send | 400/403 (bad JWT, key mismatch) | no automatic fix | dropped, class `rejected`; subscription kept (a config fix may cure it) | warn with id, host, status |
| lane: send | 201/200/202 | — | `sent` += 1, `last_ok_ms` | debug |
| `/webpush/subscribe` | see validation table | — | 400/403/409/413/415 JSON `{"error"}` | warn for Origin/login rejections, debug otherwise; never the submitted endpoint |
| `/webpush/test` | a lane does not reply in 25 s | yes | that entry has class `pending` | none |
| `/webpush/test` | lane full, or its lane ended before replying | yes | entry `lane_full` or `removed` | none |
| lane: apply result | record removed, or its `rev` changed, during the send | yes | result discarded, nothing removed or persisted; test reply carries the send's class, or `superseded` | debug |

**No push service response body is ever read into a log, a status field or a route
response.** Apple's `reason` field is not logged: the endpoint host is configurable, a
response could echo the request target, the JWT or the key headers, and nothing in the
spike shows what Apple puts there. Diagnosis uses the status and the local class; a
person who needs Apple's reason can reproduce with `/webpush/test` and a debugger.

## Invariants and who enforces them

- **Only tailnet viewers can subscribe; only loopback-with-token can test or remove.**
  Enforced in `server.rs` by the existing `authorize_viewer` / `authorize_local`. The
  daemon still binds loopback only (`Config::validate`).
- **The daemon only posts to allowed push hosts.** Enforced at subscribe time
  (validation) and again in the lane before each send. The lane check is the
  authoritative one.
- **`working`, generic, paused and stale events never push.** Enforced in
  `webpush::should_push`, the only caller of the queue, with unit tests; staleness,
  pause and filter are re-checked in the lane. Config validation rejects `working` as a second layer.
- **One bad device cannot delay another.** Enforced by the lane structure in
  `webpush.rs`: the dispatcher only uses `try_send`, and each lane sends on its own.
- **One writer for `webpush.json`.** Enforced by `WebPush::persist()` and `save_lock`;
  no other code writes the file.
- **A send result never undoes a newer change.** Enforced in the lane's result step and
  in `remove_sub`: results are applied by `id` and `rev`, so a send that started before
  an unsubscribe or a key change cannot re-add, update or remove the newer state.
  `remove_sub` is the only code that removes a record or a lane sender.
- **Pause stops queued pushes too.** Enforced in the lane, which reads the live
  `ControlState` before every send attempt.
- **Every push shows a notification.** Enforced in `sw.js` (unconditional
  `showNotification`).
- **Secrets never leave their files except as intended.** The VAPID private key, the
  endpoint path, the JWT and the subscription keys are never logged or returned;
  `/status` exposes only the endpoint host, `/webpush/test` not even that. Files are
  0600 in 0700 dirs (`ensure_private_dir`).
- **`/push` latency is unchanged.** `notify` is `try_send` only; all network work is in
  the lanes.

## Open decisions for pahud

### Q3: desk iPhone SE vs carried iPhone

Not decided here. The base design sets no default for the SE: it neither enrolls the SE
nor blocks it. Nothing is pushed to any device until someone taps **Enable
notifications** on it. The storage, sender and routes are the same under every option;
only the README wording and, for one variant of A, one config key differ.

| Option | What changes in this design | Trade-off |
|---|---|---|
| A. Carried iPhone only | A documented rule: do not tap Enable on the SE. Optionally a config key `[webpush] allowed_labels = ["pahud iPhone"]` that makes `subscribe` refuse other labels. | No duplicate chime at the desk, where the live card already shows the state. Relies on the label (self-declared by the page) or on discipline. |
| B. Every subscribed device | README says to enable on both phones. | Simplest. A push on the SE while its page is open may show a banner over the live display, which is noise beside it. Not tested. |
| C. Per-device opt-in, default off | README describes Enable and Turn off as a per-device choice. The per-device button exists in the base under every option, so C adds no code. | Most flexible; in effect it equals A if the SE is never enabled. |

Untested facts that bear on the choice: whether iOS shows a banner for a push while the
same web app is in the foreground on the SE, and whether that matters on an always-on
desk display. The implementation plan below runs without this decision; the README
wording (and `allowed_labels`, only if A with enforcement is picked) waits for it.

### D2: default payload text

The design defaults to `content = "summary"` for the reasons in "Payload and privacy".
pahud may pick `"state"` (no task text at all) or `"label"` (raw first prompt line until
a summary exists) instead. Only the default string changes.

## Testability

Unit tests (in `webpush.rs`, `relay.rs` and `config.rs`, run on macmini):

- `should_push` truth table: muted, no state, each state, `working`, filter excludes,
  `[webpush] states` excludes, age 0, age = `ttl_secs` (pushes), age = `ttl_secs` + 1
  (does not), origin in the future (age 0).
- `to_new_event` copies `received_ms` into `origin_ms`; a body cannot set `origin_ms`
  (unknown-field rejection, like `relay_id`); a page without `received_ms` gives `None`.
- payload builder: each `content` level; `"summary"` with and without `ctx.summary`
  (falls back to the tag, never the raw `session_label`); truncation with ellipsis on
  CJK text; output exactly 1024 bytes for short and maximum-length fields; absence of
  `prompt`/`body`/`host`/`url`.
- notification tag: `np-` + 16 hex; two sessions with the same last 4 characters get
  different tags; no session gives `np-<state>`.
- coalescing: a burst with two sessions and three events keeps one per session, newest.
- rate limiter: 21st push inside 60 s is dropped; one after the window passes.
- `Retry-After` hold: after a 429 with `Retry-After: 30`, jobs arriving at +1 s and
  +29 s are dropped immediately (`dropped_rate`, test reply `rate_limited`) and the lane
  channel is empty afterwards; a job at +31 s is sent. Missing header gives 60 s; 9999
  gives 600 s.
- `remove_sub`: `None` removes; `Some(rev)` with a matching rev removes the record and
  the lane sender together; `Some(old_rev)` after a key change removes nothing.
- result application: a result for a removed id changes nothing and does not persist;
  a result with an old `rev` changes nothing.
- VAPID key generation: `from_bytes` over `getrandom` output yields a key that
  round-trips through PKCS#8 PEM; a zero scalar is rejected and retried.
- response classification: 201 ok, 404/410 remove, 429 with and without `Retry-After`,
  403 keep, 5xx retry once.
- VAPID: create → file 0600, reload gives the same `key_id`; the JWT's `aud` equals the
  endpoint origin for `https://web.push.apple.com/abc` (no port) and for
  `http://127.0.0.1:41234/abc` (with port); `sub` and `exp` are right; it verifies with
  `p256::ecdsa::VerifyingKey`. This last check is self-consistent only, because the same
  crate signs and verifies; the independent checks are listed below.
- subscription validation: every row of the validation table, both outcomes.
- `webpush.json`: round trip, 0600, bad file moved aside, `key_id` mismatch discards;
  50 concurrent `persist()` calls interleaved with set changes leave a parseable file
  that equals the final in-memory set.
- config: section defaults, each validation rule, `deny_unknown_fields`.

Integration tests (`crates/nearpushd/tests/webpush.rs`, the `tests/api.rs` pattern with
a real listener):

- subscribe auth matrix: no login 401, wrong login 403, missing or cross Origin 403,
  `text/plain` 415, disallowed host 400, ninth endpoint 409, re-subscribe of the same
  endpoint 200 `created:false`.
- end-to-end send against a local mock push service: the test builds `WebPush` with an
  `EndpointPolicy` that also admits `http://127.0.0.1:<port>` (a constructor argument,
  not reachable from config), pushes a `your_turn` through `POST /push`, and the mock
  decrypts the body with `web_push_native::decrypt` and the test's UA private key,
  checks the JSON and its 1024-byte length, the `vapid` header (JWT `aud` includes the
  mock's port), `TTL` and `Urgency`. A second case returns 410 and checks the
  subscription is removed from `webpush.json`.
- head-of-line: two subscriptions, the first endpoint's mock never answers (the 10 s
  timeout fires), the second answers 201. The second mock receives the push within 2 s
  of `POST /push`, and a second `POST /push` 1 s later also reaches it promptly.
- stale relay event: the mock relay serves a page with `received_ms` = now − 1 h; the
  card appears through the stream and the push mock receives no request. A page with
  `received_ms` = now − 10 s is pushed.
- log hygiene: the push mock answers 400 and 500 with a body that contains the request
  path, the received `Authorization` header and a marker string. With a capturing
  tracing subscriber, no log line, `/status` field or `/webpush/test` result contains
  the marker, the endpoint path or the JWT.
- `/webpush/test`: one entry per subscription with `id`, `label`, `status` and `class`
  (one 201, one 410 → `gone`, one silent → `pending` within 25 s); the response
  contains no endpoint text. A test sent right after a coalescible event burst still
  gets its own reply (it does not pass through the dispatcher).
- paused and filtered events produce no request at the mock.
- pause while held: the mock answers the first attempt with 503; during the 2 s retry
  sleep the test sends `POST /control {"paused":true}`; the mock receives no second
  request and `dropped_paused` is 1. The same case with a filter change that removes
  the state also sends no retry. A `/webpush/test` while paused still reaches the mock.
- re-subscribe with new keys: subscribe endpoint E with UA keys K1, push, the mock
  decrypts with K1. Re-subscribe E with K2 and a new label: the response is
  `created:false` with the same id. The next push decrypts with K2 and not with K1, and
  the subscription is still stored. Race variant: the mock holds the K1 send open, the
  test re-subscribes with K2, then the mock answers 410; the subscription is not
  removed, and the next push (K2) is sent.
- unsubscribe during a send: the mock holds a send open, the test unsubscribes, then
  the mock answers 201; `webpush.json` does not contain the subscription afterwards and
  no further request reaches the mock.
- `/webpush/test` reply paths: two subscriptions, one lane already full (its mock holds
  one send open while four more jobs wait in that lane's channel) gives `lane_full` for that one and `ok` for the other; a subscription
  removed while its test send is open is still listed; one added after the call starts
  is not listed.
- `/status` lists the subscription with `endpoint_host` and no endpoint path or keys.

Independent checks of the VAPID and encryption wire format (the unit tests above cannot
catch a wrong `aud` format or a header encoding mistake, because they check the code
against itself):

1. **Gate before rollout:** the separate instance (implementation plan step 3) must get
   `201 Created` from `web.push.apple.com` for a `/webpush/test` push, and the push must
   show on the iPhone. A 400/403 there blocks rollout.
2. **Known-answer cross-check during implementation:** the mock test writes one captured
   request (headers and body, test keys only) to a file; a short script in the spike
   venv verifies the JWT signature and claims with `py_vapid`/`cryptography` and
   decrypts the body with `http_ece` using the test UA key. These are the libraries
   pywebpush used for the spike's three 201s. ECDSA signatures are not deterministic
   across libraries, so the check is verify-and-decrypt, not byte equality. It runs on
   the laptop because it is Python, not a Rust build.

Device acceptance (manual, real iPhone + Watch, after pahud approves touching the live
setup): repeat spike Q1 on the production origin for all three states, and record
whether the page's on-load check found `options.applicationServerKey` (the match
branch) or took the missing-key branch.

## Implementation plan (for after approval; not part of this run)

1. Worktree off the approved base. Add deps, set `rust-version = "1.87"` in
   `crates/nearpushd/Cargo.toml` only (workspace stays 1.85), `config.rs`
   `[webpush]`, `relay.rs` `received_ms` → `origin_ms`, `event.rs` transient field,
   `webpush.rs` (key, store, payload, dispatcher, lanes), routes and CSP in
   `server.rs`, `AppState` wiring in `main.rs`, `sw.js`, page drawer row, `assets.rs`
   hash. README sections: Files, config, HTTP API, iPhone (replace "There is no Web
   Push here"), privacy. docs/kiro.md diagram gets the push branch.
2. On macmini only: `cargo build --release && cargo test`. Then the relay gate:
   `rustup toolchain install 1.85` and `cargo +1.85 build --release --locked -p
   eventrelay`, and `~/.orbstack/bin/docker buildx build --platform linux/amd64 -f
   deploy/eventrelay/Dockerfile .` (the image is built, not deployed or imported to p1).
   Both must pass. On failure, pin the offending shared crate with `cargo update -p
   <crate> --precise <version>` and repeat; if web-push-native then no longer resolves,
   stop and return to review (see Technology stack).
3. Test on a separate instance: `--config-dir`/`--state-dir` under `/tmp`, `listen =
   127.0.0.1:8798`, served on a spare tailnet port such as 8450 (the spike's port),
   only with pahud's approval at that moment, removed afterwards. Run the Apple 201 gate
   here. The live daemon (`dev.nearpush.agent`, 8787) and its `/` mapping stay
   untouched.
4. Rolling out to `~/Applications/Nearpush.app` (production origin, so the existing
   home-screen app is the one that subscribes) is a separate step that needs pahud's go.
   Never funnel.

## Out of scope

- Native iOS or watchOS apps, Watch complications, actionable notification buttons
  (approve/deny from the Watch).
- Pushing `working` or generic level events.
- A push filter independent of the display filter, menu-bar UI for subscriptions, CLI
  subcommands for Web Push.
- Push services other than Apple's by default (configurable through
  `endpoint_hosts`, untested).
- VAPID key rotation tooling beyond "delete the file and restart; re-enable on each
  device".
- Storing `origin_ms` in `events.jsonl` or showing it on the card.
- The leftover spike state in `~/.local/share/nearpush-webpush-spike/` (pahud removes it).

## Assumptions

- iOS honours the notification `tag` for replacement as other browsers do; if not, each
  push is a separate notification, which is acceptable.
- Apple accepts a VAPID JWT with `exp` 12 h ahead; the spike's three 201s were sent with
  exactly that.
- An existing home-screen install of the page can register a service worker without
  being re-added, since the manifest and origin do not change.
- iOS 18.7 fills `PushSubscription.options.applicationServerKey`. If it does not, the
  page takes the no-re-POST branch, which is safe; device acceptance checks it.
- The laptop's clock and the relay host's clock agree within seconds (both NTP-synced);
  the freshness rule tolerates a relay clock that runs ahead.

## Review responses (round 1)

| # | Finding (source) | Response |
|---|---|---|
| 1 | Relay backlog pushes stale events as if new (opus5, MEDIUM) | Addressed. `RelayEvent.received_ms` → transient `NewEvent.origin_ms`; `should_push` rule 5 drops events older than `ttl_secs`, re-checked in the lane (`dropped_stale`). Unit tests and a mock relay integration case added; rule stated in "Which events push". |
| 2 | Privacy section misstates `label`/`detail` (opus5, MEDIUM) | Addressed. "Payload and privacy" now states what the code puts in `session_label`, `detail` and `project`. New default `content = "summary"` sends the summarized label only, else the session tag, so raw prompt text stays off the wrist; `"label"` and `"detail"` remain with accurate warnings. The default is argued and put to pahud as D2. |
| 3 | One stale subscription delays the others (sol, MEDIUM) | Addressed. One lane per subscription with its own 4-slot channel, rate limit, `Retry-After` and retry; the dispatcher only `try_send`s. Result updates go through the `SubSet` mutex and `persist()`. Head-of-line integration test added. |
| 4 | Logging the push service `reason` can leak secrets (sol, MEDIUM) | Addressed. Response bodies are dropped unread and never logged or returned; logs carry status, local class, id and validated host. Log-hygiene integration test with a hostile response added. |
| 5 | Wrong reason for skipping `vapid`; JWT test only checks itself (opus5, LOW) | Addressed. Reason corrected (size of the jwt-simple tree, not BoringSSL). The self-consistency limit is stated; independent checks named: the Apple 201 gate before rollout and a `py_vapid`/`http_ece` verify-and-decrypt cross-check. The 12 h `exp` is cited to pywebpush 2.5.0 and the spike's 201s. |
| 6 | VAPID `aud` must include a non-default port (opus5, LOW) | Addressed. `aud` defined as scheme, host and non-default port; unit and mock tests check both forms. |
| 7 | `webpush.json` needs a single writer (opus5, LOW) | Addressed. `persist()` holds `save_lock` from snapshot through rename; it is the only writer. Concurrent-persist unit test added. |
| 8 | Notification `tag` can collide (opus5, LOW) | Addressed. `np-` + first 8 hex of SHA-256 of the full session id; test with two sessions sharing a suffix. |
| 9 | Unpadded payload size shows label length (opus5, LOW) | Addressed. Payload padded to exactly 1024 bytes with a `"p"` filler; content capped at 960 bytes; tests check the length. |
| 10 | `/webpush/test` response not specified (opus5, LOW) | Addressed. One entry per subscription with `id`, `label`, `status`, `class`; never endpoint text; 25 s bound with `pending`; integration test added. |
| 11 | Q3 wording leans toward a choice (opus5, sol, LOW) | Addressed. "(Option A)" removed from the title; Q3 states that the base sets no default for the SE, and row C no longer reads as the base's pick. |

## Review responses (round 2)

| # | Finding (source) | Response |
|---|---|---|
| 1 | Workspace `rust-version` bump to 1.87 breaks the eventrelay Docker build on `rust:1.85-bookworm` with `--locked` (opus5, MEDIUM) | Addressed. `rust-version = "1.87"` is set in `crates/nearpushd/Cargo.toml` only; the workspace, eventrelay, the CLI and the Dockerfile stay at 1.85. The shared-lockfile risk (newer serde, serde_json, http) is named, and plan step 2 adds a gate: `cargo +1.85 build --locked -p eventrelay` plus a Dockerfile image build on macmini. A failure is fixed by pinning the shared crate with `cargo update --precise`, or goes back to review. |
| 2 | `/webpush/test` has no workable per-subscription reply path; one oneshot fanned out, coalescing may drop it (sol, MEDIUM) | Addressed. Tests bypass `notify` and the dispatcher. `test_all()` snapshots `(id, label, lane sender)` under the `SubSet` lock and `try_send`s an independent `LaneJob::Test` with its own oneshot into each lane. Tests skip freshness, pause and filter, and keep the rate limit, hold, rev check and send logic. The response lists exactly the snapshot; later additions are not listed; removals during the call show `removed` or `gone`; a full lane shows `lane_full`. `Job` no longer has a reply field. Integration tests added. |
| 3 | Upserting an existing subscription does not refresh its lane; stale keys can encrypt wrongly and remove a valid subscription (sol, MEDIUM) | Addressed with the "fetch by id before each send" rule. A lane owns only the id and reads endpoint, keys and an in-memory `rev` before every attempt, the retry included. An upsert that changes keys bumps `rev`. Results are applied only if the id still exists and `rev` is unchanged, and lane removals go through `remove_sub(id, Some(rev))`, so an old send cannot remove or overwrite a newer record. Integration test: re-subscribe the same endpoint with new keys, check the next push decrypts with the new keys, plus a race variant with a 410 on the old send. |
| 4 | Pause is not re-checked at send time (opus5, LOW) | Addressed. Lanes read the live `ControlState` before every send attempt and drop event jobs while paused or when the state left the filter (`dropped_paused`). Test jobs are exempt. Pause-while-held integration test added (503, pause during the 2 s retry sleep, no second request). |
| 5 | `Retry-After` hold behaviour is ambiguous (opus5, LOW) | Addressed. During a hold the lane drops jobs as they arrive (`dropped_rate`; test reply `rate_limited`) and never sleeps with jobs queued. Unit test added. |
| 6 | A late send result can arrive for a removed subscription; who removes the lane sender on 404/410 (opus5, LOW) | Addressed. The result step looks the record up by id and changes nothing if it is gone. `remove_sub` is the only code that removes a record and its lane sender, together under the `SubSet` lock; the lane calls it on 404/410 and exits. Unsubscribe-during-send integration test added. |
| 7 | Page re-POSTs before the `applicationServerKey` check, so an old-key subscription is stored again after rotation (opus5, LOW) | Addressed. On load the page compares `subscription.options.applicationServerKey` with `/webpush/key` first and re-POSTs only on a match. On a mismatch it unsubscribes locally and on the daemon. If iOS does not expose the key, the page does not re-POST; device acceptance records which branch iOS takes. |
| 8 | An 8-hex tag prefix does not guarantee uniqueness (sol, LOW) | Addressed. The tag uses 16 hex characters (64 bits). The "cannot collide" claim is removed, and the accepted risk is stated (about n²/2^65; the effect is one notification replacing another). |
| 9 | `p256` without `rand_core/getrandom` makes `SigningKey::random(&mut OsRng)` fail to compile (opus5, LOW note) | Addressed. Key generation fills 32 bytes with `getrandom`, calls `SigningKey::from_bytes`, and retries on an invalid scalar. Signing is RFC 6979 deterministic, so it needs no RNG. A unit test is added. |
