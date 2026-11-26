# Design: Apple Watch support through Web Push to the home-screen web app

Status: draft for council review, round 5. Base: openabdev/nearpush `main` @ baec237
(the code the implementation lands on; line references below are to that commit).
Evidence: [spike-report.md](spike-report.md) (Q1 PASS, Q2 PASS, both observed on
pahud's carried iPhone, iOS 18.7, with a paired Apple Watch). Nothing here is
implemented or merged.

Q3 is decided: both devices receive pushes, with no cross-device dedupe (see
Q3). Q3 affects README wording only; it adds no config key and no code. The default
payload text (D2) is decided in this revision: `content = "state"`,
which keeps all task text off the lock screen and the Watch; richer content is an
explicit opt-in. One verification item is open: whether Declarative Web Push supports a
per-push icon or a custom summary (O1).

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
nearpush hook / send ─▶ POST /push ──────────────┐
relay puller (origin_ms) ─▶ accept_event ────────┴▶ ingest
                                                     ├─ classifiable Stop ─▶ defer_verdict (hold) ─┐
                                                     └─ everything else ───────────────────────────┤
                                                                                                   ▼
                                       commit() ─▶ store.push ─▶ WS viewers (unchanged)
                                          └▶ webpush::notify (try_send only)
                                               │ queue (64) → dispatcher
                                               │   coalesce, age and control checks
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
    Key generation fills 32 bytes with the existing `getrandom` dependency, calls
    `SigningKey::from_bytes(&bytes.into())`, and retries on `Err` (a zero or
    out-of-range scalar; for P-256 a uniform 32-byte value is out of range with
    probability `(2^256 − n) / 2^256`, about 2^-32). In the built graph
    `rand_core/getrandom` is in fact enabled: web-push-native's `encrypt()` calls
    `SecretKey::random(&mut OsRng)` and pulls aes-gcm with its default features, which
    include `getrandom`, and Cargo unifies features. `SigningKey::random(&mut OsRng)`
    would therefore compile too. The design keeps `from_bytes` over `getrandom` so the
    VAPID key path depends only on nearpushd's own direct dependency and not on a
    feature that another crate happens to turn on. ES256 signing uses
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
- **The shared lockfile is the remaining risk.** web-push-native 0.5.0 can raise the
  locked versions of crates that eventrelay also uses. Its normal dependencies that can
  move shared lock entries are `http ^1.4.2` and the trees under `aes-gcm`, `hkdf 0.13`
  and `sha2 0.11` (for example `rand_core`, `getrandom`, `generic-array`, `subtle`,
  `cpufeatures`) wherever eventrelay locks the same crate. `serde_json` is only a
  dev-dependency of web-push-native, and `serde` is optional and off under
  `default-features = false`, so neither is moved by it. If one of the moved crates declares an MSRV above 1.85, the relay's `--locked`
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

**Where the hook sits: `server::commit()`.** In the base, events reach the store by
three routes, and all three end in `commit()` (server.rs:575):

- direct `POST /push` → `ingest()` → `commit()` (server.rs:436–444, :492);
- the relay puller → `accept_event()` → `ingest()` → `commit()` (relay.rs:496,
  server.rs:691; `accept_event` returns `()` and is used by the relay only);
- a classifiable Stop when `[classifier]` is enabled → `ingest()` →
  `defer_verdict()`, which answers 202 at once and calls `commit()` later from a
  spawned task, after the verdict replaced the rules state (server.rs:476–483, :552).
  With the classifier on, these deferred Stops are the main source of `your_turn` and
  `all_set`.

`commit()` is therefore the one write point for every stateful event, and the push
decision is made there, right after `store.push` returns the stored `Event` and after
`track_main`. It calls `state.webpush.notify(&ev, ctx)`, which never blocks and never
fails the request: it only does a `try_send` on a bounded channel. `accept_event` and
`ingest` get no push code. `ctx` carries three transient values that are not stored
with the event:

- `origin_ms` (see "Event origin time"), taken from `new.origin_ms` before `new` is
  moved into `spawn_blocking`. `NewEvent` is moved whole through `ingest` and
  `defer_verdict`, so the field survives the direct, relay and deferred paths alike.
- `summary`: the result of the `state.sessions.label(s)` lookup that `commit()` already
  does (server.rs:582) to replace the provisional `session_label`. The same lookup
  value is used, not a second call, so the payload and the stored event agree.
- `control_epoch`: the control epoch read in the same `state.control` read that sets
  `new.muted` (see "Pause and filter changes cancel queued jobs").

A deferred Stop is pushed with its post-verdict state, at commit time. The classifier
hold is a few seconds (`runtime.hold`), well inside the 300 s freshness bound, and
freshness is measured when the lane sends, so the hold costs nothing but its own delay.

The one other `store.push` caller in the base is `record_dismissal` (server.rs:1128).
It stores a stateless `dismissed` record, which rule 2 below never pushes, so it needs
no hook. Rule for later code: any new path that stores an event with a state must go
through `commit()`. Events loaded from `events.jsonl` at start never pass through
`commit()`, so a restart sends nothing. The relay's dedupe (cursor plus `relay_id` in
the ring) already stops one relay event from being committed twice, so a relay replay
after a crash does not push twice.

**Event origin time.** Relay events are stamped with the time they are pulled
(`relay::to_new_event` calls `validate(now_ms())`), and eventrelay keeps events for up to
7 days, so the stored `ts` cannot tell an old relay event from a new one. The change:

- `relay::RelayEvent` gains `#[serde(default)] received_ms: Option<u64>`. eventrelay
  already sends it (`crates/eventrelay/src/http.rs:156`).
- `NewEvent` gains `#[serde(skip)] pub origin_ms: Option<u64>`, set only by the relay
  puller: `relay::run` sets `new.origin_ms = ev.received_ms` after either converter
  succeeds (`to_new_event` for push-shaped bodies, `kiro_to_new_event` for Kiro adapter
  bodies), so the freshness check applies to every relay-sourced event regardless of
  converter. Clients cannot send it.
  `Event` and `events.jsonl` do not change.
- In `commit()`, `origin_ms = new.origin_ms.unwrap_or(ev.ts)`. A direct `POST /push`
  is therefore "now". A relay event without `received_ms` (an older relay) is also
  treated as "now"; the eventrelay in this repo always sends it. A deferred Stop keeps
  its relay `origin_ms` if it came from the relay; a direct deferred Stop gets the
  `ts` of its commit, which is after the hold.
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

**Pause and filter changes cancel queued jobs.** `muted` is fixed when the event is
committed, but a job can wait in the dispatcher's 500 ms window or in a lane behind a
10 s timeout and the 2 s retry. Reading only the live flags at send time is not enough:
a pause followed by an unpause, or a state removed from the filter and added back,
while the job waits would let the old job through. The rule chosen is: **any pause or
filter change made after an event was committed cancels that event's push.**

- `ControlState` gains `epoch: u64`, in memory only (not in `control.json`; it starts
  at 0 and no job survives a restart). `POST /control` increments it, under
  `control_write` and in the same write that stores the new value, whenever the
  request changes `paused` or the effective `states` set (compared as sets, so
  re-sending the same filter is not a change). A classifier-only change does not touch
  it.
- `commit()` reads `paused` and `epoch` in one `state.control` read; `epoch` goes into
  `ctx.control_epoch` and from there into `Job` and `LaneJob::Event`.
- Before every send attempt, the retry included, the lane copies `paused`, `states` and
  `epoch` out of the live `ControlState` (`AppState.control`, an
  `Arc<std::sync::RwLock<ControlState>>` that `WebPush` holds a clone of) and releases
  the lock. The event job is dropped (`dropped_paused` += 1) if `paused` is true, the
  job's state is not in `states`, or `epoch` differs from the job's `control_epoch`.
- The dispatcher applies the same check when it drains, so a change during the 500 ms
  window drops the job before it reaches any lane.

The cost: any filter edit, even one that adds an unrelated state, cancels pushes that
are in flight at that moment. They are at most a few seconds old, so this is accepted
for one simple rule. Test jobs skip this check (see `/webpush/test`).

Rule 4 makes the menu's existing *Show on iPhone* checkboxes and `POST /control
{"states"}` also govern pushes, so there is one switch per state for "what my iPhone
tells me". The alternative, an independent runtime push filter in `/control`, needs new
menu UI and a second mental model; it is backlogged. Consequence to document: unchecking
`all_set` in the menu stops both the card and the push.

**Unpausing does not push.** Events stored while paused stay muted forever (existing
rule), and they are never pushed later either. An event committed before a pause whose
job is still queued when the pause happens is cancelled by the epoch change, so it does
not send after the unpause either.

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
{"v":1,"id":123,"state":"your_turn","title":"✋ your turn","body":"","tag":"np-3f9a0c1e7b2d4a58","url":"/","p":"000…"}
```

(The default `content = "state"` sends an empty `body`. Under `"summary"` the same push
would carry e.g. `"body":"#ab12 · frontend"`.)

What the code puts in each field today (checked against the base):

- `session_label` on an event is the summarizer's label only if `Sessions::label`
  already had one when `commit()` ran (server.rs:582). Otherwise it is the hook's provisional
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

- `title`: fixed per state, a fixed symbol followed by a short state word:

  | State | Title |
  |---|---|
  | `your_turn` | `✋ your turn` |
  | `all_set` | `✅ all set` |
  | `allow` | `❓ allow?` |

  Never the event title, and never task text. These three strings were tested to render
  on the Watch (docs/apple-watch.md, "Title convention"). The reason for the rule: every
  Watch summary shows the same manifest icon and the title, so the title is the only
  per-push text that tells the states apart at a glance. The strings live in one table
  in `webpush.rs` that the payload builder uses; `sw.js` shows the payload title as it
  is and adds no prefix of its own. The docs page and the payload use the same three
  strings. The page's live card headline (`your turn.` etc.) does not change. The detail
  stays in `body` and is governed by `content` below.
- `body`, by `[webpush] content`:
  - `"state"` (default): empty. The Watch shows only the title, e.g. `✋ your turn`.
    No session label, summary, project name, tool detail or other task text leaves
    the daemon in the payload.
  - `"summary"` (opt-in): the summarized label if `ctx.summary` is set, otherwise the
    session tag `#xxxx` (the page's own `sessionTag` rule: last 4 alphanumerics of the
    session), then `project`, joined with ` · `, cut to 120 characters with an
    ellipsis. The code never selects the stored `session_label` (the hook's raw first
    prompt line) in this mode. That is the only guarantee: the summary is generated
    from the user's prompts by the summarizer, and nothing sanitizes it, so it can
    repeat names, issue text or parts of the prompt verbatim. The project name is the
    working directory's name.
  - `"label"` (opt-in): as `"summary"`, but uses the event's `session_label` as stored, which
    may be the raw first prompt line.
  - `"detail"` (opt-in): as `"label"`, then the `detail` line, cut to 160 characters total. This
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

**Why `"state"` is the default (D2, decided).** The lock screen and the Watch face are
readable by anyone nearby. Every other mode puts text derived from the task on them:
`"label"` the raw first prompt line, `"detail"` commands and file paths, and
`"summary"` a generated summary that no code checks for sensitive content, plus the
project directory name. Only `"state"` gives a guarantee that holds by construction:
the payload carries the fixed title and nothing from the event's text fields. The cost
is that two waiting sessions look the same on the Watch; the user opens the page to see
which one. `"summary"`, `"label"` and `"detail"` stay available as explicit opt-ins in
`config.toml`, each with the warning above in the README. Changing the default later
is a one-string change, but the implementation ships with `"state"`.

What third parties see:

- **Apple's push service** carries the payload as RFC 8291 ciphertext encrypted to the
  subscription's `p256dh`/`auth` keys; by the protocol it cannot read the title or body.
  It does see metadata: the endpoint (a device-scoped token), the laptop's public IP,
  the time of each push, the constant size, `TTL`, `Urgency`, and the VAPID `sub` claim
  (the configured contact, e.g. a mailto address). This is protocol behaviour; the spike
  did not inspect Apple's side.
- **The iPhone lock screen and the Watch face** show the decrypted title and body to
  anyone who looks. Under the default `"state"` that is only the fixed title. Under
  `"summary"` it adds the generated summary or session tag and the project name, under
  `"label"` it can be the raw first prompt line, and under `"detail"` it adds commands
  and file paths. iOS *Show Previews: When Unlocked* hides
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
  does not rewrite the file on every push. In-memory state is authoritative; see "Save
  failures" below for what happens when a write fails.

**One writer at a time.** The in-memory set lives in a `std::sync::Mutex<SubSet>` that is
only held for short in-memory updates, never across I/O. Every save, from route
handlers and from lanes, goes through `WebPush::persist()`:

1. acquire `save_lock` (a `tokio::sync::Mutex<()>`, the same pattern as `control_write`);
2. under the `SubSet` lock, serialize a snapshot to bytes, then release that lock;
3. write tmp, fsync, rename inside `spawn_blocking`, through the `write_atomic` function
   that `WebPush` holds (the real file writer in production; integration tests
   construct `WebPush` with one that fails on demand, which is how save failures are
   tested);
4. release `save_lock`.

Because the snapshot is taken after `save_lock` is held, the last save to finish writes
the newest state, and no two writers ever share the tmp path. Route handlers await
`persist()` so they can report a failed save; lanes await it too (a short disk write
inside one lane does not delay the others).

**Lock order.** There are three locks: `save_lock` (tokio), the `SubSet` mutex (std) and
the `ControlState` `RwLock`. The rules, enforced in `webpush.rs` by code shape and by the
review checklist of the implementation PR:

1. `save_lock` is only ever taken while no `SubSet` guard is held, and it is the only
   lock held across an `.await`. Order, when both are held: `save_lock` first, then
   `SubSet`, and only inside `persist()` step 2.
2. A `SubSet` guard is never held across an `.await`, never held while taking
   `save_lock`, and never held while calling `persist()`, `remove_sub()`, `add_sub()`,
   `test_all()` or any other `WebPush` method that takes `SubSet` itself. Code that holds
   the guard only reads and writes fields, clones values out, and returns a plain value.
   Each such section is a small synchronous function that receives `&mut SubSet` and no
   `&WebPush` (`fn …(&mut SubSet, …) -> …`), so it cannot `.await` and has no handle to
   any other lock.
3. `ControlState` is read, copied out and released before the `SubSet` lock is taken;
   the two are never held together.

**Save failures (one rule for routes and lanes).** A change is applied in memory first
and kept when the save fails; it is never rolled back. A failed save sets a `dirty` flag
in `WebPush`, logs an error, and wakes a flush task that calls `persist()` again after
30 s, then every 30 s while `dirty` is set (each failure logged at most once per minute).
Any later successful `persist()`, from a route, a lane or the flush task, clears `dirty`.
`/status` shows `webpush.dirty`. For the routes this means:

- `persist()` returns `Result<(), PersistError>`. Every route-facing mutator returns
  that result to its caller next to what it changed, so the route knows whether *its
  own* save failed without reading the shared `dirty` flag (which another save or the
  flush task may set or clear at any moment):
  - `add_sub(..) -> (UpsertResult { id, created }, Result<(), PersistError>)`;
  - `remove_sub(..) -> RemoveOutcome`, defined under "Lane and record ownership".
- `subscribe`, `unsubscribe` and `remove` return 500
  `{"error":"cannot save webpush.json","applied":true}` exactly when their own call
  reports `Err(PersistError)`. A request that changed nothing attempts no save and
  never returns this 500 (see the per-route results in "Input validation"). The 500
  says: the change is live now, and it is lost if the daemon stops before a later save
  succeeds.
- Why not roll back: undoing a removal would have to recreate a lane, and undoing an
  upsert would have to restore the old keys and `rev` while other changes may have
  happened in between. Keeping the change gives one simple meaning. What happens to a
  change lost by a restart depends on the route:
  - **subscribe** heals: the device's next page load re-POSTs it (the on-load re-POST,
    match branch).
  - **unsubscribe** is expected to heal: the page calls `subscription.unsubscribe()`
    before posting, so the old record comes back pointing at an endpoint the browser
    has dropped, and its next push should get 404/410 and be removed by the lane.
    Until then at most one push goes to a dead endpoint, which shows nothing. This is
    an untested assumption: the spike never observed Apple's answer to a push after a
    browser-side unsubscribe. If Apple answers 201 instead, the record stays and keeps
    receiving silent pushes until it is removed with `/webpush/remove`.
  - **remove does not heal.** `/webpush/remove` is called by a local operator while the
    device may still be subscribed, so a removal lost by a restart brings back a live
    subscription that keeps receiving pushes. For `remove`, a 500 with
    `applied:true` therefore means: the removal holds only while the daemon runs;
    check `GET /status` until `webpush.dirty` is `false` (the flush task saved it), and
    if the daemon restarted first and the id is listed again, repeat the remove. The
    README states this next to the route.
  - Independent of save failures: removing a device that still has the page installed
    and a live subscription lasts only until that page's next load, which re-POSTs the
    subscription (new id). `/webpush/remove` is for devices that are gone; to stop a
    device that is still in use, tap **Turn off** on it.
- The page treats a subscribe 500 with `applied:true` as "notifications on (not saved
  yet)" and does not retry; the next load re-POSTs anyway.

## HTTP routes

Page-facing routes use `authorize_viewer` (loopback peer plus allowlisted
`Tailscale-User-Login`, or the local-testing flag). State-changing page routes also
require a same-origin browser `Origin`, which is the CSRF defence: a page on another
site opened in Safari on the iPhone could otherwise send a request through the tailnet
that carries pahud's login header.

| Route | Auth | Purpose |
|---|---|---|
| `GET /sw.js` | viewer | service worker, served with `page_headers("text/javascript", "no-cache")` (see below) |
| `GET /webpush/key` | viewer | `{"enabled":bool,"key":"<b64url public key>"|null,"key_id":…}` |
| `POST /webpush/subscribe` | viewer + `require_same_origin_json` | upsert a subscription |
| `POST /webpush/unsubscribe` | viewer + `require_same_origin_json` | `{"endpoint":…}` removes it |
| `POST /webpush/test` | local (loopback, token, no proxy headers) | sends a test push to all subscriptions, ignoring pause, filter and freshness |
| `POST /webpush/remove` | local | `{"id":…}` removes a subscription |

**The Origin check is the "present" variant.** The existing `check_origin`
(server.rs:396) lets a request with no `Origin` header pass, because it was written for
WebSocket clients such as websocat. Used alone it would let any non-browser caller with
a login header through, and it is not the CSRF rule these routes need. The routes use a
new helper in `server.rs`, `require_same_origin_json(headers) -> Result<(), ApiError>`,
that follows the checks `authorize_dismiss` already does (server.rs:978–991), in this
order:

1. no `Origin` header → 403 `missing Origin`;
2. `check_origin(headers)` fails (host differs from `Host` and `X-Forwarded-Host`) →
   403 `cross-origin request`;
3. `Content-Type` does not start with `application/json` (trimmed, case-insensitive) →
   415 `send application/json`.

`subscribe` and `unsubscribe` both call it right after `authorize_viewer`, so both
require a present, matching `Origin` and a JSON content type. `authorize_dismiss` is not
changed in this work. `check_origin` alone is never used for a `/webpush/*` route.
Because `check_origin` logs `rejected cross-origin websocket`, the helper does not rely
on that line: on a step-2 failure it logs its own warn `rejected cross-origin webpush
request` (path only, no headers). The check is unconditional, unlike dismiss's
login-only branch, so local-testing callers (login `"loopback"`) must also send
`Origin` with a matching `Host` on subscribe and unsubscribe, and the integration tests
set both headers.

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

`GET /status` gains a `webpush` object: `enabled`, `key_id`, `dirty`, `sent`, `failed`,
`dropped_rate`, `dropped_queue`, `dropped_stale`, `dropped_paused`, `last_ok_ms`, `last_error_class`, and
`subscriptions` as `[{id, label, login, endpoint_host, created_ms, last_ok_ms,
last_error_class}]`. No endpoint path, no keys. The Swift `DaemonStatus` decoder ignores
unknown keys, so the menu app needs no change in this phase.

CSP (`page_headers`) adds `worker-src 'self'`. Without it `default-src 'none'` blocks
the service worker registration, because `script-src` allows only inline scripts.
`/sw.js` is served through the same `page_headers("text/javascript", "no-cache")` as the
page, not with a separate header set. A service worker runs under the CSP of its own
script response, and that CSP already contains `img-src 'self'`, so the notification
`icon: /icon-192.png` may load. A bare `default-src 'none'` on `/sw.js` would block it.
An integration test checks that the `/sw.js` response carries `img-src 'self'` and
`Cache-Control: no-cache`.
`assets::ui_version()` hashes `sw.js` too, so a changed worker also reloads the page.

When `[webpush] enabled = false`: `/sw.js` is still served (the page may already have a
registered worker; a 404 would leave a stale one), `/webpush/key` returns
`enabled:false`, `subscribe`, `unsubscribe`, `remove` and `test` return 409 `{"error":"web push is disabled"}`,
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
| `Origin` | present and its host equals `Host` (or `X-Forwarded-Host`), via `require_same_origin_json` | 403 |
| `Content-Type` | `application/json`, via `require_same_origin_json` (a non-simple type, so a cross-site form cannot send it without a preflight, which the daemon never answers) | 415 |

The endpoint allowlist is the SSRF defence: the daemon only ever posts to hosts in
`endpoint_hosts`, with redirects off. Error responses from this route never echo the
submitted endpoint.

**One `EndpointPolicy` for both checks.** The endpoint rule in the table (scheme, no
userinfo, port, host) is implemented once, as `EndpointPolicy::check(&Url) ->
Result<(), EndpointRejected>`. `WebPush` holds exactly one `EndpointPolicy`, built at
construction: in production from `endpoint_hosts` (https only, port 443 only). The
integration tests pass a policy built with `EndpointPolicy::with_loopback_http(port)`,
which additionally admits `http://127.0.0.1:<port>`. That constructor is a public
function of the `nearpushd` library (integration tests in `tests/` link the library and
cannot see `#[cfg(test)]` items), and it is reachable only by Rust code: `main.rs`
builds the policy from `endpoint_hosts` and has no path to it, and config cannot name
it. The
subscribe route and lane step 4 both call `webpush.endpoint_policy().check(..)` on that
same object, so the mock tests and production go through the same code path with
different data, and a hand-edited `webpush.json` is held to the same rule as a new
subscription.

Success: 200 `{"id":…,"created":bool}`. `login` is taken from the request's
`Tailscale-User-Login` header (or `"loopback"` with the local-testing flag), not from
the body. A save failure in `add_sub` gives 500 `applied:true` (see "Save failures").
An upsert that changes no stored field still saves (its `updated_ms` changes), so it
can also report that 500.

`POST /webpush/unsubscribe`: auth as in the routes table (including
`require_same_origin_json`), body `{"endpoint": string ≤ 1024}` with
`deny_unknown_fields`, route limit 4 KiB. It calls `remove_sub(Endpoint(e), None)`:

| `RemoveOutcome` | Response |
|---|---|
| `Removed { persisted: Ok }` | 200 `{"removed":true}` |
| `Removed { persisted: Err }` | 500 `{"error":"cannot save webpush.json","applied":true}` |
| `Absent` (no save attempted) | 200 `{"removed":false}`, so the page cannot probe |

`POST /webpush/remove`: local auth, body `{"id": 16 lowercase hex}` with
`deny_unknown_fields` (400 otherwise). It calls `remove_sub(Id(id), None)`:

| `RemoveOutcome` | Response |
|---|---|
| `Removed { persisted: Ok }` | 200 `{"removed":true}` |
| `Removed { persisted: Err }` | 500 `{"error":"cannot save webpush.json","applied":true}`; repeat rule in "Save failures" |
| `Absent` (no save attempted) | 404 `{"error":"no such subscription"}` |

`RevMismatch` cannot occur on either route, because both pass `None`. Both routes
return 409 `web push is disabled` when disabled, like `subscribe`.

## Config keys

```toml
[webpush]                       # every key optional; the section needs this nearpushd version
enabled = false                 # true: create the VAPID key and send pushes
subject = "mailto:you@example.com"  # VAPID contact sent to Apple; required when enabled
states = ["your_turn", "allow", "all_set"]  # subset of these three; "working" is rejected
content = "state"               # "state" (default, title only) | opt-in: "summary" | "label" | "detail" (see Payload and privacy)
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
  `data: {url}`. The `p` filler is ignored. The title is the payload's `title` as is (see
  the title table under "Payload and privacy"); the parse-failure fallback `nearpush` is
  the only title not from that table. On the Watch the summary shows the manifest icon;
  a per-push `icon` is reported not to be honoured (docs/apple-watch.md), so `icon` is
  kept for the iPhone banner only and nothing relies on it. Whether Declarative Web Push
  changes this is open item O1.
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
  control_epoch: u64, payload: Arc<[u8]> }`. No reply channel.
- `LaneJob` (per-lane channel, 4 slots):
  `Event { state, origin_ms, control_epoch, payload: Arc<[u8]> }` from the dispatcher, or
  `Test { payload: Arc<[u8]>, reply: oneshot::Sender<TestResult> }` from `test_all()`.
  Each test job is built for one lane, so each has its own oneshot.

Dispatcher task: receive, wait 500 ms, drain, coalesce, drop jobs older than `ttl_secs`
(`dropped_stale`), drop jobs that fail the control check (paused, state not in the
filter, or `epoch` changed; `dropped_paused`), then `try_send` a `LaneJob::Event` (cloning the `Arc` payload) into
every lane sender in a snapshot taken under the `SubSet` lock.

### Lane and record ownership

- **A lane owns only its subscription `id`.** It does not keep the endpoint or keys.
- **Lanes are created** by `WebPush::add_sub` (a new endpoint, new id) and at load, in
  the same critical section that inserts the record. An upsert of an existing endpoint
  does not create, replace or restart a lane.
- **Lanes are removed only by `WebPush::remove_sub(sel, expect_rev: Option<u64>) ->
  RemoveOutcome`**, where `sel` is the id (lanes, `/webpush/remove`) or the endpoint
  (`/webpush/unsubscribe`). Under the `SubSet` lock it looks the record up and decides:

  ```rust
  enum RemoveOutcome {
      Absent,       // no such record: nothing removed, no save attempted
      RevMismatch,  // expect_rev is Some and the record's rev differs: nothing removed, no save
      Removed { persisted: Result<(), PersistError> }, // record and lane sender removed, then persist()
  }
  ```

  Only in the `Removed` case does it remove the record and the lane sender together;
  it then releases the `SubSet` guard and awaits `persist()`, whose result becomes
  `persisted`. An `Err` there means the removal is applied in memory and `dirty` is
  set. It is an `async fn` and must be called with no `SubSet` guard held (see "Lock
  order"). `RevMismatch` is only possible when `expect_rev` is `Some`. The unsubscribe
  and remove routes call it with `None`; their mapping of each outcome to a response
  is in "Input validation". A lane calls it with `Some(rev it sent with)` after 404/410
  or an encryption error, so an old send cannot remove a record whose keys were
  replaced meanwhile. If the new keys hit the same 410, the next send removes the
  record with the new rev.
- **A lane exits** when `recv()` returns `None` (all senders dropped, i.e. it was
  removed by a route), or right after a `remove_sub` that returned `Removed` or
  `Absent`. Jobs still in its channel are dropped; their test replies resolve as
  `removed`.

Lane task, per job:

1. Read the record under the `SubSet` lock, by `id`: clone its `endpoint`,
   `p256dh`, `auth` and `rev`. If the id is gone, drop the job (test reply `removed`)
   and exit the lane.
2. Checks, in order. For event jobs only: age (`dropped_stale`), then live pause and
   filter and the control `epoch` from `ControlState` (`dropped_paused`). For all jobs: an active `Retry-After`
   hold (drop; event `dropped_rate`, test `rate_limited`), then the 20/60 s rate limit
   (`dropped_rate` or `rate_limited`).
3. `WebPushBuilder::new(endpoint_uri, p256dh, auth).with_valid_duration(ttl).build(payload)`.
   An error here is class `encryption`.
4. Re-check the endpoint with the same `EndpointPolicy::check` the subscribe route uses
   (the authoritative SSRF check: a hand-edited `webpush.json` must not bypass it). A
   rejected endpoint is class `rejected`, the job is dropped and the record is kept,
   logged at error with the id and host.
5. Add `Authorization: vapid t=…, k=…` (JWT cached per endpoint origin, renewed when
   less than 1 h of its 12 h remains) and `Urgency: high`.
6. Convert to `reqwest::Request`, send with the dedicated client, classify the status,
   and count `sent` or `failed`. The response body is dropped unread.
7. On network error or 5xx, sleep 2 s and repeat once from step 1. The retry therefore
   re-reads the record and re-checks age, pause and filter.
8. Apply the result in two phases, so no lock is held across an `.await` and no
   `WebPush` method is called with the `SubSet` guard held (see "Lock order").

   **Phase A, under the `SubSet` lock, synchronous.** A function
   `apply_result(&mut SubSet, id, rev, &SendOutcome, now) -> ResultAction` looks the
   record up by `id`:
   - id gone (removed during the send): change nothing, return `None`. A test reply
     carries the send's own status and class.
   - `rev` differs from the one read in step 1 (keys replaced during the send): change
     nothing, return `None`. A test reply is `superseded`.
   - otherwise, on 404/410 or an encryption error: change nothing, return
     `Remove(rev)`.
   - otherwise: on 429 set `hold_until` (lane-local state, not in `SubSet`, so it is set
     after phase A); update `last_ok_ms` / `last_error_class`; return `Persist` if the
     write rule says so, else `None`.

   The guard is dropped when the function returns.

   **Phase B, no lock held.**
   - `None`: nothing.
   - `Persist`: await `persist()`. A failure follows "Save failures" (sets `dirty`).
   - `Remove(rev)`: await `remove_sub(id, Some(rev))`. It repeats the compare under the
     lock, so it stays the final compare-and-remove. Its three outcomes:
     - `Removed { persisted }`: the lane removed its own record. Test reply `gone`
       (404/410) or `encryption`. A `persisted` error follows "Save failures" and does
       not change the reply. The lane exits after replying.
     - `RevMismatch`: an upsert changed the keys between phase A and phase B. Nothing
       is removed and the lane keeps running for the new keys. Test reply
       `superseded`.
     - `Absent`: a route removed the record between phase A and phase B. Test reply
       `gone` for a 404/410 and `encryption` for an encryption error, because the
       subscription is gone either way. The route already dropped the lane sender, so
       the lane exits after replying (jobs still in its channel resolve as
       `removed`).

   The test reply, if this is a test job, is sent after phase B, so its class reflects
   what phase B did.

## Error handling

| Operation | Failure | Recoverable? | Caller gets | Log |
|---|---|---|---|---|
| config load | invalid `[webpush]` key or value | fatal at start | process exits 1 with the message (existing behaviour) | error |
| VAPID key load/create | unreadable file, bad PEM, create fails | fatal at start when enabled (a silently disabled feature would look like Apple failing) | exit 1, path named, key content never printed | error |
| VAPID key file mode | group/other bits set | yes | chmod 0600 | warn |
| `webpush.json` load | missing | yes | empty set | none |
| `webpush.json` load | unparseable | yes | moved to `.bad-<ms>`, empty set | error |
| `webpush.json` load | `key_id` mismatch | yes | subscriptions discarded | warn |
| `webpush.json` save | I/O error | yes | the change stays applied in memory; `dirty` set; flush task retries every 30 s. subscribe/unsubscribe/remove return 500 `{"error":"cannot save webpush.json","applied":true}` (live now, lost if the daemon stops before a later save succeeds); lanes continue | error, at most once per minute while `dirty` |
| lane: endpoint check | `EndpointPolicy::check` rejects a stored endpoint (hand-edited file) | no automatic fix | job dropped, class `rejected`, record kept | error with id and host |
| `notify` | queue full | yes | nothing (push dropped), `dropped_queue` += 1 | warn, at most once per minute |
| dispatcher / lane | event older than `ttl_secs` | yes | dropped, `dropped_stale` += 1 | debug |
| dispatcher / lane | paused, the state is not in the display filter, or the control `epoch` changed since commit (any pause or filter change, including one that was undone) | yes | event job dropped, `dropped_paused` += 1 (test jobs exempt) | debug |
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
| `/webpush/unsubscribe` | missing or cross `Origin`, wrong content type, bad body | — | 403/415/400 | warn for Origin/login rejections, debug otherwise; never the submitted endpoint |
| `/webpush/unsubscribe` | unknown endpoint (`Absent`) | — | 200 `{"removed":false}`, no save attempted | debug |
| `/webpush/remove` | unknown id (`Absent`) | — | 404, no save attempted | debug |
| `/webpush/unsubscribe`, `/webpush/remove` | `Removed { persisted: Err }` | yes | 500 `applied:true`; for `remove`, the operator repeats it if a restart came first (see "Save failures") | error (from `persist()`) |
| lane: phase B `Remove(rev)` | `remove_sub` returns `Absent` (a route removed the record first) | yes | lane exits; test reply `gone` or `encryption` | debug |
| lane: phase B `Remove(rev)` | `remove_sub` returns `RevMismatch` | yes | nothing removed, lane keeps running; test reply `superseded` | debug |
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
  (validation) and again in the lane before each send, both through the one
  `EndpointPolicy` that `WebPush` holds. The lane check is the authoritative one.
- **No deadlock between `save_lock`, `SubSet` and `ControlState`.** Enforced in
  `webpush.rs` by the "Lock order" rules: `SubSet` guards live only inside synchronous
  functions over `&mut SubSet`, and `persist()` / `remove_sub()` are called with no
  guard held. Covered by the two-phase race test.
- **Every stored stateful event is considered for a push exactly once.** Enforced by
  placing the only `notify` call in `server::commit()`, the one function that stores
  stateful events (direct `POST /push`, relay, classifier-deferred Stops). An
  integration test per path, including the deferred one, covers it.
- **`working`, generic, paused and stale events never push.** Enforced in
  `webpush::should_push`, the only caller of the queue, with unit tests; staleness,
  pause and filter are re-checked in the dispatcher and the lane. Config validation rejects `working` as a second layer.
- **One bad device cannot delay another.** Enforced by the lane structure in
  `webpush.rs`: the dispatcher only uses `try_send`, and each lane sends on its own.
- **One writer for `webpush.json`.** Enforced by `WebPush::persist()` and `save_lock`;
  no other code writes the file.
- **A send result never undoes a newer change.** Enforced in the lane's result step and
  in `remove_sub`: results are applied by `id` and `rev`, so a send that started before
  an unsubscribe or a key change cannot re-add, update or remove the newer state.
  `remove_sub` is the only code that removes a record or a lane sender.
- **A pause or filter change cancels pushes already queued.** Enforced by the
  `ControlState.epoch` that `POST /control` bumps (in `server.rs`, under
  `control_write`) and that the dispatcher and the lane compare with the job's
  `control_epoch` before every send attempt (in `webpush.rs`). A pause that is undone
  before the job is checked still cancels it.
- **The default payload carries no task text.** Enforced by `WebPushConfig`'s default
  `content = "state"` and by the payload builder, which for `"state"` reads only the
  event's `state`, `id` and session id (for the tag hash); a unit test checks that no
  event text field appears in the output.
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

**Decided by pahud: option B, both devices receive pushes.** The carried iPhone and the
desk iPhone SE each tap **Enable notifications** and each get their own subscription.
Each subscription is delivered independently, through its own upsert and lane, so a
failure or delay on one device does not affect the other. The same notification may
therefore ring on both devices; the design does not dedupe across devices, and pahud
accepts that. Nothing is pushed to a device until someone taps **Enable notifications**
on it.

**Boundary, decided in this revision:** Q3 changes enrollment documentation only (the
README's iPhone section). It adds no config key, no validation and no code under any
option. There is no `allowed_labels` key: a device label is typed into the page and
sent by the client, so it is not an identity and cannot be a security boundary, and a
check on it would only look like enforcement. Every viewer allowlisted for the page can
already subscribe any device it controls; that is the trust boundary. If pahud later
wants an enforced per-device restriction, it needs a real device identity and is a
separate design change. The config schema in "Config keys" is therefore final for this
implementation, and the implementation can proceed before Q3 is decided.

| Option | What changes in this design | Trade-off |
|---|---|---|
| A. Carried iPhone only | A documented rule: do not tap Enable on the SE. | No duplicate chime at the desk, where the live card already shows the state. Relies on discipline; nothing in the daemon enforces it. |
| B. Every subscribed device | README says to enable on both phones. | Simplest. A push on the SE while its page is open may show a banner over the live display, which is noise beside it. Not tested. |
| C. Per-device opt-in, default off | README describes Enable and Turn off as a per-device choice. The per-device button exists in the base under every option, so C adds no code. | Most flexible; in effect it equals A if the SE is never enabled. |

Untested facts that bear on the choice: whether iOS shows a banner for a push while the
same web app is in the foreground on the SE, and whether that matters on an always-on
desk display. With option B chosen, the README says to enable notifications on both
phones.

### D2: default payload text (decided)

Decided in round 5: `content = "state"`, for the reasons in "Payload and privacy". The
lock screen and the Watch show only the fixed state title; no prompt text, summary,
project name or tool detail. `"summary"`, `"label"` and `"detail"` are explicit opt-ins
in `config.toml`, documented with what each one exposes. pahud can still choose a
richer default before implementation starts; that would be a one-string change plus the
matching README and test edits, and it is not assumed here.

## Open items (verification, not decisions)

### O1: Declarative Web Push icon and summary support is unverified

Declarative Web Push lets Safari on iOS show a notification straight from a JSON payload
without running the service worker. Whether it supports a per-push icon or
a custom Watch summary (instead of the manifest icon and the fallback text
`Notification` the spike saw) is not verified. This design does not use Declarative Web
Push: it sends a classic encrypted payload that `sw.js` turns into a notification, which
is what the spike proved on the Watch. Nothing in the implementation depends on O1. If a
later check shows Declarative Web Push gives a per-state icon or summary on the Watch,
adopting it is a separate design change (a different payload shape and a different iOS
code path), not a tweak to this one. Until then the title table above is the only
per-state signal on the Watch.

## Testability

Unit tests (in `webpush.rs`, `relay.rs` and `config.rs`, run on macmini):

- `should_push` truth table: muted, no state, each state, `working`, filter excludes,
  `[webpush] states` excludes, age 0, age = `ttl_secs` (pushes), age = `ttl_secs` + 1
  (does not), origin in the future (age 0).
- `relay::run` sets `origin_ms` from `received_ms` for both converters (a push-shaped
  body and a Kiro adapter `stop` body with an `event` key); a body cannot set `origin_ms`
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
- `remove_sub` outcomes, with an injected `write_atomic` that counts calls: an unknown
  id or endpoint gives `Absent` and makes no write call; `Some(old_rev)` after a key
  change gives `RevMismatch`, removes nothing and makes no write call; `None`, or a
  matching `Some(rev)`, gives `Removed { persisted: Ok(()) }` and removes the record and
  the lane sender together; the same with a failing writer gives
  `Removed { persisted: Err(_) }`, the record is gone from memory and `dirty` is set.
- control epoch: `POST /control` with a `paused` change bumps `epoch`; a `states`
  change bumps it; re-sending the same set (in another order or with duplicates) does
  not; a classifier-only change does not.
- default payload: with `WebPushConfig::default()` the content is `"state"`; for an
  event whose `session_label`, `detail`, `project`, `title`, `body` and `url` all
  contain a marker string, the JSON contains no marker and `body` is empty.
- result application: a result for a removed id changes nothing and does not persist;
  a result with an old `rev` changes nothing.
- VAPID key generation: `from_bytes` over `getrandom` output yields a key that
  round-trips through PKCS#8 PEM; a zero scalar is rejected and retried.
- payload titles: each state's title is exactly `✋ your turn`, `✅ all set` or
  `❓ allow?`, under every `content` level; no task text ever appears in `title`; the
  1024-byte length holds with the multi-byte prefixes.
- lock order: `apply_result` over a plain `SubSet` (no `WebPush`, no runtime) returns
  `None` for a removed id, `None` for a changed `rev`, `Remove(rev)` for 410 and for an
  encryption error, and `Persist` only when the write rule says so.
- two-phase race (tokio test with a paused clock and a test hook between phase A and
  phase B): (1) a 410 result whose phase A returned `Remove(rev)` races an upsert that
  changes the keys before phase B; `remove_sub` returns `RevMismatch`, the record keeps
  the new keys, the lane keeps running, and a test reply is `superseded`. (2) A result
  that returned `Persist` races an upsert plus its route `persist()`; both saves finish,
  the file equals the final in-memory set. (3) A 410 result whose phase A returned
  `Remove(rev)` races a `/webpush/remove` of the same id before phase B; `remove_sub`
  returns `Absent`, the test reply is `gone` (not `superseded`), and the lane exits.
  Both cases run under a 5 s `tokio::time::timeout`, so a reintroduced self-deadlock fails
  the test instead of hanging it.
- save failures: with an injected writer fault, subscribe, unsubscribe and remove each
  return 500 with `applied:true`, the in-memory set reflects the change and `/status`
  shows `dirty:true`; after the fault clears, the flush task writes the file within 30 s
  and `dirty` clears. With the fault still active, an unsubscribe of an unknown
  endpoint returns 200 `{"removed":false}` and a remove of an unknown id returns 404:
  no save is attempted, so neither reports 500.
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
- unsubscribe auth matrix: the same login, missing Origin (403 `missing Origin`),
  cross Origin (403) and `text/plain` (415) cases; a valid request for a stored
  endpoint gives 200 `removed:true`, for an unknown one 200 `removed:false`.
- remove: local caller only (a tailnet viewer gets the local-auth rejection); a stored
  id gives 200 `removed:true`; an unknown id 404; a malformed id 400.
- one push per store path: (1) direct `POST /push` of a `your_turn` without
  `classify` reaches the mock; (2) a relay event reaches the mock (the fresh case of
  the stale-relay test below); (3) classifier-deferred: with `classifier_state` (the
  existing `tests/api.rs` helper, scripted kiro provider answering `NO`, hold 5 s) a
  Stop with rules state `your_turn` and a reply tail is posted, `POST /push` answers
  202 `deferred:true`, and after the verdict the mock receives exactly one push whose
  decrypted `state` and `title` are the post-verdict ones (`all_set`, `✅ all set`),
  not the rules state. A variant whose provider is unavailable (rules fallback) also
  pushes exactly once, with the rules state.
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
  card appears through the stream and the push mock receives no request. Run once with a
  push-shaped body and once with a Kiro adapter `stop` body (`event` key). A page with
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
- undone pause while held: same setup, but during the retry sleep the test sends
  `{"paused":true}` and then `{"paused":false}`; the mock receives no second request
  and `dropped_paused` is 1. The same with a filter that removes the state and then
  restores it. A new event committed after the unpause is pushed normally.
- held behind a send: the mock holds the first push open for 3 s; a second event for
  another session is queued in the same lane; during the hold the test pauses and
  unpauses; the second event is never sent.
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
- `/sw.js` response headers: `Content-Type: text/javascript`, `Cache-Control: no-cache`,
  and a CSP containing `img-src 'self'` and `worker-src 'self'`.
- one `EndpointPolicy`: with the production policy, a subscribe for
  `http://127.0.0.1:<port>` is 400; with the loopback test policy it is accepted and the
  push reaches the mock; a record hand-edited into `webpush.json` with a host outside the
  policy is never posted to (class `rejected`, the mock sees no request).
- save failure then restart: the state dir is made unwritable (or a writer fault is
  injected), a subscribe returns 500 `applied:true`, the daemon is restarted before the
  fault clears, and the subscription is absent; the page's next on-load re-POST (simulated)
  stores it again. Same for unsubscribe: after restart the old record is back, and its
  next push gets 410 from the mock and removes it. Same for remove: the restart brings
  the record back, it is listed again in `/status`, the mock keeps receiving its pushes
  (201), and a repeated remove after the fault clears removes it for good. A further
  case clears the fault before the restart and checks that all three changes survived.

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
setup): repeat spike Q1 on the production origin for all three states, check that the
Watch shows each title exactly as in the title table, and record
whether the page's on-load check found `options.applicationServerKey` (the match
branch) or took the missing-key branch.

## Implementation plan (for after approval; not part of this run)

1. Worktree off the approved base. Add deps, set `rust-version = "1.87"` in
   `crates/nearpushd/Cargo.toml` only (workspace stays 1.85), `config.rs`
   `[webpush]`, `relay.rs` `received_ms` → `origin_ms`, `event.rs` transient field,
   `webpush.rs` (key, store, payload, dispatcher, lanes), in `server.rs` the
   `notify` call in `commit()`, `ControlState.epoch` and its bump in `POST /control`,
   `require_same_origin_json`, the routes and the CSP, `AppState` wiring in `main.rs`, `sw.js`, page drawer row, `assets.rs`
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
| 2 | Privacy section misstates `label`/`detail` (opus5, MEDIUM) | Addressed. "Payload and privacy" now states what the code puts in `session_label`, `detail` and `project`. Round 1 added `content = "summary"` (summarized label, else the session tag) with `"label"` and `"detail"` as warned alternatives, and put the default to pahud as D2. *Superseded in round 5:* the default is now `content = "state"` (title only) and `"summary"`, `"label"`, `"detail"` are opt-ins, because a generated summary is not guaranteed free of prompt text (round-4 response S3). |
| 3 | One stale subscription delays the others (sol, MEDIUM) | Addressed. One lane per subscription with its own 4-slot channel, rate limit, `Retry-After` and retry; the dispatcher only `try_send`s. Result updates go through the `SubSet` mutex and `persist()`. Head-of-line integration test added. |
| 4 | Logging the push service `reason` can leak secrets (sol, MEDIUM) | Addressed. Response bodies are dropped unread and never logged or returned; logs carry status, local class, id and validated host. Log-hygiene integration test with a hostile response added. |
| 5 | Wrong reason for skipping `vapid`; JWT test only checks itself (opus5, LOW) | Addressed. Reason corrected (size of the jwt-simple tree, not BoringSSL). The self-consistency limit is stated; independent checks named: the Apple 201 gate before rollout and a `py_vapid`/`http_ece` verify-and-decrypt cross-check. The 12 h `exp` is cited to pywebpush 2.5.0 and the spike's 201s. |
| 6 | VAPID `aud` must include a non-default port (opus5, LOW) | Addressed. `aud` defined as scheme, host and non-default port; unit and mock tests check both forms. |
| 7 | `webpush.json` needs a single writer (opus5, LOW) | Addressed. `persist()` holds `save_lock` from snapshot through rename; it is the only writer. Concurrent-persist unit test added. |
| 8 | Notification `tag` can collide (opus5, LOW) | Addressed. `np-` + a SHA-256 prefix of the full session id instead of the session suffix; test with two sessions sharing a suffix. The prefix is 16 hex characters (64 bits), as set in round 2 response 8; the 8-hex length first written here was replaced then. |
| 9 | Unpadded payload size shows label length (opus5, LOW) | Addressed. Payload padded to exactly 1024 bytes with a `"p"` filler; content capped at 960 bytes; tests check the length. |
| 10 | `/webpush/test` response not specified (opus5, LOW) | Addressed. One entry per subscription with `id`, `label`, `status`, `class`; never endpoint text; 25 s bound with `pending`; integration test added. |
| 11 | Q3 wording leans toward a choice (opus5, sol, LOW) | Addressed. "(Option A)" removed from the title; Q3 states that the base sets no default for the SE, and row C no longer reads as the base's pick. |

## Review responses (round 2)

| # | Finding (source) | Response |
|---|---|---|
| 1 | Workspace `rust-version` bump to 1.87 breaks the eventrelay Docker build on `rust:1.85-bookworm` with `--locked` (opus5, MEDIUM) | Addressed. `rust-version = "1.87"` is set in `crates/nearpushd/Cargo.toml` only; the workspace, eventrelay, the CLI and the Dockerfile stay at 1.85. The shared-lockfile risk is named (the list was corrected in round 3, response 5: `http ^1.4.2` and the trees under aes-gcm, hkdf 0.13 and sha2 0.11; `serde` and `serde_json` are not moved), and plan step 2 adds a gate: `cargo +1.85 build --locked -p eventrelay` plus a Dockerfile image build on macmini. A failure is fixed by pinning the shared crate with `cargo update --precise`, or goes back to review. |
| 2 | `/webpush/test` has no workable per-subscription reply path; one oneshot fanned out, coalescing may drop it (sol, MEDIUM) | Addressed. Tests bypass `notify` and the dispatcher. `test_all()` snapshots `(id, label, lane sender)` under the `SubSet` lock and `try_send`s an independent `LaneJob::Test` with its own oneshot into each lane. Tests skip freshness, pause and filter, and keep the rate limit, hold, rev check and send logic. The response lists exactly the snapshot; later additions are not listed; removals during the call show `removed` or `gone`; a full lane shows `lane_full`. `Job` no longer has a reply field. Integration tests added. |
| 3 | Upserting an existing subscription does not refresh its lane; stale keys can encrypt wrongly and remove a valid subscription (sol, MEDIUM) | Addressed with the "fetch by id before each send" rule. A lane owns only the id and reads endpoint, keys and an in-memory `rev` before every attempt, the retry included. An upsert that changes keys bumps `rev`. Results are applied only if the id still exists and `rev` is unchanged, and lane removals go through `remove_sub(id, Some(rev))`, so an old send cannot remove or overwrite a newer record. Integration test: re-subscribe the same endpoint with new keys, check the next push decrypts with the new keys, plus a race variant with a 410 on the old send. |
| 4 | Pause is not re-checked at send time (opus5, LOW) | Addressed. Lanes read the live `ControlState` before every send attempt and drop event jobs while paused or when the state left the filter (`dropped_paused`). Test jobs are exempt. Pause-while-held integration test added (503, pause during the 2 s retry sleep, no second request). *Extended in round 5 (round-4 response S2):* the live check alone let a pause that was undone before the check through, so jobs now also carry a control epoch and any pause or filter change after commit cancels them. |
| 5 | `Retry-After` hold behaviour is ambiguous (opus5, LOW) | Addressed. During a hold the lane drops jobs as they arrive (`dropped_rate`; test reply `rate_limited`) and never sleeps with jobs queued. Unit test added. |
| 6 | A late send result can arrive for a removed subscription; who removes the lane sender on 404/410 (opus5, LOW) | Addressed. The result step looks the record up by id and changes nothing if it is gone. `remove_sub` is the only code that removes a record and its lane sender, together under the `SubSet` lock; the lane calls it on 404/410 and exits. Unsubscribe-during-send integration test added. |
| 7 | Page re-POSTs before the `applicationServerKey` check, so an old-key subscription is stored again after rotation (opus5, LOW) | Addressed. On load the page compares `subscription.options.applicationServerKey` with `/webpush/key` first and re-POSTs only on a match. On a mismatch it unsubscribes locally and on the daemon. If iOS does not expose the key, the page does not re-POST; device acceptance records which branch iOS takes. |
| 8 | An 8-hex tag prefix does not guarantee uniqueness (sol, LOW) | Addressed. The tag uses 16 hex characters (64 bits). The "cannot collide" claim is removed, and the accepted risk is stated (about n²/2^65; the effect is one notification replacing another). |
| 9 | `p256` without `rand_core/getrandom` makes `SigningKey::random(&mut OsRng)` fail to compile (opus5, LOW note) | Addressed. Key generation fills 32 bytes with `getrandom`, calls `SigningKey::from_bytes`, and retries on an invalid scalar. Signing is RFC 6979 deterministic, so it needs no RNG. A unit test is added. |

## Review responses (round 3)

| # | Finding (source) | Response |
|---|---|---|
| 1 | Lane result step holds the `SubSet` lock while calling `remove_sub` and the async `persist()`, which take `SubSet` themselves: self-deadlock, and a std guard held across `.await` (sol, MEDIUM; council F1) | Addressed. Lane step 8 is now two-phase. Phase A is a synchronous `apply_result(&mut SubSet, …) -> ResultAction` that validates `(id, rev)`, updates in-memory fields and returns `None`, `Persist` or `Remove(rev)`; the guard drops on return. Phase B, with no lock held, awaits `persist()` or `remove_sub(id, Some(rev))`, which repeats the compare and stays the final compare-and-remove. A new "Lock order" section fixes the order (`save_lock` before `SubSet`, only inside `persist()`; no `SubSet` guard across `.await` or around any `WebPush` call; `ControlState` never held with `SubSet`), and a new invariant names who enforces it. Tests added: `apply_result` unit table, and a two-phase race test (Remove and Persist each racing a key-changing upsert) under a 5 s timeout so a deadlock fails instead of hanging. |
| 2 | A 500 from subscribe/unsubscribe/remove after a failed save has no defined meaning; a lost unsubscribe can come back after restart (sol, LOW; council F2) | Addressed with one rule: keep the change, never roll back. A failed save sets `dirty`, a flush task retries every 30 s, `/status` shows `dirty`, and the routes return 500 `{"error":"cannot save webpush.json","applied":true}`, documented as "live now, lost if the daemon stops before a later save succeeds". Rollback was rejected (it would have to recreate lanes and restore old keys and `rev`). A lost subscribe is re-POSTed on the next page load, and a resurrected unsubscribed record is expected to be removed by its first 410. *Corrected in round 5 (round-4 response O-L1):* a lost `/webpush/remove` does not heal, because the device is still subscribed; the operator checks `/status` `dirty` and repeats the remove, and the 410-after-unsubscribe behaviour is marked untested. `persist()` writes through an injectable `write_atomic`; integration tests force a write failure, restart, and check subscribe and unsubscribe, plus a case where the fault clears before restart. |
| 3 | Invalid-scalar probability stated as 2^-128 (sol, LOW; council F3) | Addressed. Corrected to `(2^256 − n) / 2^256`, about 2^-32. Retrying stays. |
| 4 | Claim that p256's features do not enable `rand_core/getrandom` is wrong in the built graph (opus5, LOW; council F4) | Addressed. The text now says feature unification turns it on (web-push-native's `encrypt()` uses `OsRng`, aes-gcm default features include `getrandom`). The `from_bytes`-over-`getrandom` approach is kept, with the reason reworded: the VAPID key path should depend only on nearpushd's own direct dependency, not on a feature another crate enables. |
| 5 | Shared lockfile risk list names crates that are not shared (opus5, LOW; council F5) | Addressed. The list is now `http ^1.4.2` and the trees under aes-gcm, hkdf 0.13 and sha2 0.11; `serde_json` (dev-dependency only) and `serde` (optional, off) are named as not moved. The plan step 2 gate is unchanged. |
| 6 | `/sw.js` response headers not specified; the icon needs `img-src 'self'` in the worker's own CSP (opus5, LOW; council F6) | Addressed. `/sw.js` is served with `page_headers("text/javascript", "no-cache")`, whose CSP includes `img-src 'self'` and the new `worker-src 'self'`. Integration test on the response headers added. |
| 7 | Lane host check and subscribe validation must use one `EndpointPolicy` (opus5, LOW note; council F7) | Addressed. `EndpointPolicy::check` implements the endpoint rule once; `WebPush` holds one policy (from `endpoint_hosts` in production, `with_loopback_http(port)` in tests, unreachable from config), and both the subscribe route and lane step 4 call it. A stored endpoint the policy rejects is class `rejected` and is never posted to. Integration test covers both policies and a hand-edited record. |
| 8 | Title convention from the Watch test (pahud, docs/apple-watch.md) | Added. Titles are a fixed symbol plus a short state word, `✋ your turn`, `✅ all set`, `❓ allow?`, from one table in `webpush.rs`; `sw.js` shows them as is; the body keeps the detail under `content`. Unit test and a device-acceptance check added. |
| 9 | Declarative Web Push icon/summary support (pahud, docs/apple-watch.md open item) | Added as open item O1: unverified. The design does not use Declarative Web Push and nothing depends on it; adopting it later is a separate design change. |
| 10 | Q3 (desk SE) | Unchanged: stays open with options A, B and C and no default. D2 stayed open in round 3. *Updated in round 5:* D2 is decided (`content = "state"`), and Q3 is limited to README wording with no `allowed_labels` key (round-4 responses S3 and S5). |

## Review responses (round 4)

Opus findings are O-M1 and O-L1 to O-L3; sol findings are S1 to S5, in the order of
`council/round-4/sol.md`.

| # | Finding (source) | Response |
|---|---|---|
| O-M1 | The push hook sits at `accept_event`, which on openabdev `main` is the relay path only; direct `POST /push` and classifier-deferred Stops would never push (opus, MEDIUM, blocking) | Addressed. The hook moved to `server::commit()`, the one write point for every stateful event (direct `POST /push`, relay via `accept_event` → `ingest`, and `defer_verdict` after the hold). "Where the hook sits" lists all three paths with line references, `ctx.summary` reuses the `sessions.label` lookup `commit()` already does, and `origin_ms` comes from `new.origin_ms`, which survives every path. `record_dismissal`, the only other `store.push` caller, stores stateless records and needs no hook. Deferred Stops push their post-verdict state; the hold is a few seconds and freshness is measured at send time, well inside 300 s. The stated base is now openabdev `main` @ baec237. New invariant and an integration test per path, including a classifier-deferred case built on the existing `classifier_state` helper. |
| O-L1 | A lost `/webpush/remove` does not heal through a 410 (opus, LOW) | Addressed. "Save failures" now treats each route separately. For `remove`, a 500 with `applied:true` means the removal holds only while the daemon runs: check `/status` until `dirty` is false, and if a restart came first and the id is listed again, repeat the remove. The 404/410 after a browser-side unsubscribe is marked as an untested assumption, with what happens if Apple answers 201. Also stated: `/webpush/remove` is for gone devices; a live page re-subscribes on its next load. Integration test for the remove case added. |
| O-L2 | Phase B test-reply classes are incomplete: a route removal between phase A and B reads as `superseded` (opus, LOW) | Addressed. `remove_sub` returns `RemoveOutcome { Absent, RevMismatch, Removed { persisted } }`. Phase B maps `Absent` to `gone` (or `encryption`) and exits the lane, `RevMismatch` to `superseded` with the lane kept, and `Removed` to `gone`/`encryption`. Unit tests for each outcome and a third two-phase race case (route remove between the phases) added. |
| O-L3 | The Origin check needs the "present" variant; unsubscribe's Content-Type rule is open (opus, LOW) | Addressed. New helper `require_same_origin_json` in `server.rs`: missing `Origin` 403, mismatch 403 via `check_origin`, non-JSON `Content-Type` 415, the same order `authorize_dismiss` uses. Both `subscribe` and `unsubscribe` call it; `check_origin` alone is never used on a `/webpush/*` route. Unsubscribe auth-matrix test added. |
| S1 | `remove_sub` returning a bool cannot report a failed save to the routes (sol, MEDIUM) | Addressed by the same `RemoveOutcome` as O-L2: `Removed { persisted: Result<(), PersistError> }` carries the call's own save result, so routes never read the shared `dirty` flag. `add_sub` likewise returns its own persist result. Per-route outcome tables are in "Input validation": unknown endpoint 200 `removed:false` and unknown id 404, both with no save attempted. Tests: unit outcomes with a counting writer, and route tests that a failing save gives 500 `applied:true` on subscribe, unsubscribe and remove while unknown targets do not. |
| S2 | A pause or filter change that is undone before the lane checks lets a queued job send (sol, MEDIUM) | Addressed by choosing "any pause or filter change after commit cancels the push". `ControlState` gains an in-memory `epoch` bumped by `POST /control` when `paused` or the effective `states` set changes; `commit()` captures it with `muted`; the dispatcher and the lane drop event jobs whose epoch differs, as well as paused or filtered ones. "Unpausing does not push" now holds for queued jobs too; the invariant is reworded. Tests: epoch bump rules, undone pause and undone filter change during a held retry, and pause/unpause while a job waits behind another send in the same lane. |
| S3 | The `"summary"` default does not guarantee that no sensitive text reaches the Watch (sol, MEDIUM) | Addressed. D2 is decided: the default is `content = "state"` (fixed title, empty body, no event text fields). `"summary"`, `"label"` and `"detail"` are explicit opt-ins. The `"summary"` description now states only what the code guarantees (it never selects the raw `session_label`) and says the generated summary is unsanitized and may repeat prompt text. New invariant and a unit test that a marker in every event text field never appears in the default payload. |
| S4 | Older response rows contradict the current design (8-hex tag; serde/serde_json as lockfile risks) (sol, LOW) | Addressed. Round-1 response 8 now states the 16-hex prefix and notes the change; round-2 response 1 names the corrected crate list. Rows that later rounds changed are also annotated: round-1 response 2 (default now `"state"`), round-2 response 4 (control epoch), round-3 response 2 (remove does not heal) and round-3 response 10 (D2 decided, Q3 docs-only). |
| S5 | Q3 may add `allowed_labels` outside the otherwise final config schema (sol, LOW) | Addressed. Decided: Q3 changes README enrollment wording only. There is no `allowed_labels` key in this implementation, because a label is client-supplied and is neither an identity nor a security boundary. An enforced per-device rule would need a real device identity and a separate design. Option A's row and the Q3 text no longer mention the key, and the config schema is final. |

## Review responses (round 5)

| # | Finding (source) | Response |
|---|---|---|
| M1 | `origin_ms` is set only in `to_new_event`, so Kiro adapter relay events (`kiro_to_new_event`) bypass the freshness bound (opus, MEDIUM, blocking) | Addressed. "Event origin time" now states that `relay::run` sets `new.origin_ms = ev.received_ms` after either converter, so the freshness check runs on every relay-sourced event. The unit test and the stale-relay integration test cover both body shapes. |
| L1 | Reusing `check_origin` logs `rejected cross-origin websocket` for `/webpush/*` (opus, LOW) | Addressed. `require_same_origin_json` logs its own warn `rejected cross-origin webpush request` on an Origin mismatch. |
| L2 | Local-testing callers must send `Origin` on subscribe and unsubscribe (opus, LOW) | Addressed. One sentence under the Origin check: the check is unconditional, so loopback callers send `Origin` and a matching `Host`; integration tests set both. |
| Q3 | Q3 (desk SE) decision (pahud) | Decided: option B. Both the carried iPhone and the desk SE receive pushes; each subscription is delivered independently, the same notification may ring on both, and there is no cross-device dedupe (accepted). README wording only; no new config key or code. |
