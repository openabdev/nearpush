VERDICT: CHANGES_REQUESTED

Reviewer: opus5. Round 2. Reviewed design.md (round 2) against spike-report.md and the
code in `.worktrees/apple-watch-webpush` (base 89c8b67 plus the two spike commits).

## Round 1 findings: verified

All of my round 1 findings are addressed, and the fixes match the code:

- Stale relay pushes. `crates/eventrelay/src/http.rs:156` does send `received_ms`.
  `relay::RelayEvent` (relay.rs:121) currently drops it. `NewEvent` is
  `deny_unknown_fields` with `#[serde(skip)] relay_id` (event.rs:105, and the test at
  relay.rs:484), so a skipped `origin_ms` is unreachable from clients, as the design says.
  `to_new_event` needs one more argument or a post-set, which is a plain implementation
  detail.
- Privacy text. `accept_event` (server.rs:334-344) replaces `session_label` only when
  `Sessions::label` has a summarized label (sessions.rs:124). Otherwise the hook's
  provisional label stays, so `ctx.summary` = "the `label()` result at accept time" is a
  correct definition of "summarized".
- web-push-native 0.5.0 (MSRV 1.87, published 2026-07-26). I checked its source:
  `WebPushBuilder::new(Uri, p256::PublicKey, Auth)`, `with_valid_duration`, `build()`
  sets `TTL`, `Content-Encoding: aes128gcm` and `Content-Length`; it writes one record of
  `message.len() + 17`; `decrypt` is public; `vapid` is the optional feature that pulls
  `jwt-simple` with `pure-rust`. The design's claims about it are accurate.
- The CSP change is needed and correct: with `script-src 'unsafe-inline'` and no
  `worker-src`/`child-src`, `/sw.js` registration would be blocked (server.rs:536).
- The subscribe Origin rule reuses the Host / X-Forwarded-Host match that `check_origin`
  already uses in production for `/ws`, and additionally requires that Origin be present.

Required coverage is all present: subscription storage, VAPID key handling, the three
pushing states with `working` rejected at config load and in `should_push`, payload and
privacy (including Apple's metadata view), config keys, pause/filter/control interaction,
loopback bind with tailscale serve and never funnel, no secrets printed, 0600 files in
0700 dirs. Q3 stays open with options A/B/C, and the base explicitly picks none. No native
app.

## Findings

### MEDIUM 1: the `rust-version` bump breaks the eventrelay image build

The design changes the workspace `rust-version` from 1.85 to 1.87 (Technology stack, and
Implementation plan step 1). `rust-version` is set in `[workspace.package]` (Cargo.toml:8)
and inherited by all three crates, eventrelay included (`rust-version.workspace = true`).
`deploy/eventrelay/Dockerfile` builds with `FROM rust:1.85-bookworm` and runs
`cargo build --release --locked -p eventrelay`. After the bump, Cargo refuses that build
because eventrelay would then declare 1.87. The design does not mention this, and the
breakage would show up only at the next relay image build and p1 import (the deploy path
recorded in base commit 89c8b67), well after this feature ships.

The fix is one decision; choose one and state it in the design:
- set `rust-version = "1.87"` only in `crates/nearpushd/Cargo.toml` and leave the
  workspace and eventrelay at 1.85; or
- bump the workspace and also change the Dockerfile to `rust:1.87-bookworm` or later, and
  list a rebuild of the relay image as a check in the plan.

Also add a step to the plan: run `cargo build --locked -p eventrelay` on the old toolchain,
or rebuild the image. The new lock entries (`serde ^1.0.229`, `serde_json ^1.0.151`,
`http ^1.4.2` from web-push-native) raise versions that eventrelay shares, so
`--locked` builds of eventrelay will pick up the newer serde/http too.

### LOW 2: pause is not re-checked at send time

`muted` is fixed in `accept_event`. Lanes re-check age before a send, but they do not
re-check the pause flag. A job can wait in a lane for up to `ttl_secs`: behind a 10 s
timeout plus the 2 s retry, or during a `Retry-After` hold. If the user pauses during
that wait, the job still buzzes the Watch. The user expects pause to stop pushes at once.
Fix: in lane step 1, skip non-test jobs while `ControlState.paused` is true. Add a case to
the "paused" integration test that pauses while a job is held.

### LOW 3: "Retry-After hold" behaviour is ambiguous

The error table says "that lane holds until `Retry-After`". Lane step 1 says "`Retry-After`
hold check". The design does not say whether a held lane sleeps, leaving jobs in its
4 slots, or drops jobs that arrive during the hold. With a hold of up to 600 s and a
300 s freshness bound, either choice is safe, but the two behave differently and should
be tested differently. Pick one: dropping and counting as `dropped_rate` is simpler and
consistent with "no retry queue". Then state it.

### LOW 4: a lane result for a subscription that was already removed

An unsubscribe or remove can run while that subscription's lane is mid-send. Lane step 7
then updates the record and may persist. The design should say that step 7 looks the
record up by id and does nothing if it is gone, so a removal is never undone by a late
send result. The 404/410 path should also drop the lane's sender from the map. "Lanes
end when it is removed" implies this but does not say who removes the map entry when
the lane itself detects the 410.

### LOW 5: order of the key check and the re-POST on page load

"On every load … re-POST it" comes before "If the subscription's
`applicationServerKey` differs … the page unsubscribes". The server cannot tell which
key a subscription was made with. If the page re-POSTs first, an old-key subscription is
stored again after a key rotation. It then gets 403 (`rejected`), which by design keeps
the subscription, so it occupies one of the 8 slots indefinitely. State that the page
compares `subscription.options.applicationServerKey` with `/webpush/key` before any
re-POST, and only re-POSTs on a match.

### LOW 6 (note): VAPID key generation entropy

`p256` with the listed features does not enable `rand_core/getrandom`, so
`SigningKey::random(&mut OsRng)` will not compile as is. Generating 32 bytes with the
existing `getrandom` dependency and using `SigningKey::from_bytes` (retry on the
negligible invalid-scalar case) avoids a new feature. This is an implementation note,
not a design gap.

## Summary

The round 2 design is sound and matches the code and the spike. One MEDIUM remains. The
`rust-version` bump silently breaks the eventrelay Docker build, which pins rustc 1.85.
Scope the bump to nearpushd, or bump the Dockerfile and add a check. After that the
design is LGTM from me; the LOW items are clarifications the implementer can take.
