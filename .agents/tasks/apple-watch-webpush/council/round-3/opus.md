VERDICT: LGTM

Reviewer: opus5, round 3. Reviewed `design.md` (round 3) against `spike-report.md` and the
code at `.worktrees/apple-watch-webpush` (base 89c8b67 plus the two spike commits).

## Round-2 findings: verification

All of my round-2 findings are resolved, and sol's three MEDIUMs as summarized in the
design are addressed in a way I can follow through the text:

- **MSRV / relay build (my MEDIUM):** fixed. `rust-version = "1.87"` moves to
  `crates/nearpushd/Cargo.toml` only. The workspace (`Cargo.toml:8`, 1.85), eventrelay and
  `deploy/eventrelay/Dockerfile` (`rust:1.85-bookworm`, `--locked -p eventrelay`) stay as
  they are, which matches the code. The shared-lockfile risk now has an explicit gate on
  macmini (plan step 2), a fallback (`cargo update --precise`), and a stop condition.
  I checked crates.io: web-push-native 0.5.0 declares `rust_version = 1.87.0`.
- **Pause re-check, `Retry-After` drop, late results, page key-check order (my LOWs):**
  each one is specified in the lane steps and backed by a named test.
- **`/webpush/test` reply path, stale lanes after upsert (sol's MEDIUMs):** the
  per-lane `LaneJob::Test` with its own oneshot avoids the dispatcher. The
  "lane owns only the id, reads the record and `rev` before every attempt" rule, together
  with `remove_sub(id, expect_rev)` as the only remover, closes the races I could
  construct. Those are: an old 410 against new keys, unsubscribe during a send, a route
  removal while an idle lane holds no jobs (the lane exits when the last sender clone
  drops), and re-subscribe after removal (new id, new lane, the old lane finds its id
  gone and exits).

## Required coverage check

- Subscription storage: `webpush.json`, 0600, atomic, single writer, cap 8, `key_id` binding. Covered.
- VAPID key: PKCS#8 PEM, `create_new` + 0600, 0700 dir, chmod-and-warn, never printed. Covered.
- States: only `your_turn`, `allow`, `all_set`; `working` is rejected at config load and in `should_push`. Covered.
- Payload and privacy: fixed titles, `content` levels with accurate descriptions of
  `session_label`/`detail` (verified against `accept_event`, server.rs:334-367), 1024-byte
  padding, Apple-side metadata listed. Covered.
- Config keys and validation. Covered.
- Pause/filter/control interaction: checked at accept time and again in the lane; unpause never pushes. Covered.
- Tailnet only, never funnel: stated in invariants and in plan steps 3 and 4. Covered.
- Q3 stays open with options A/B/C and no default. Covered. D2 is also put to pahud.
- No native iOS/watchOS app. Out of scope, as stated.

Code claims I spot-checked: `received_ms` in the relay page (`crates/eventrelay/src/http.rs:156`),
`AppState.control: Arc<RwLock<ControlState>>` (server.rs:95), `control_write` pattern,
existing `check_origin` Host/X-Forwarded-Host rule (server.rs:289), CSP lacking
`worker-src` (server.rs:545), `ui_version` asset list (assets.rs:86), and the web-push-native
0.5.0 source (`WebPushBuilder::new/with_valid_duration/build`, `decrypt`, the TTL and
`Content-Encoding` headers, the `message.len() + 17` record length). All match the design.

## Findings

1. **LOW: the claim that p256's features "do not enable rand_core/getrandom" is wrong in the built graph.**
   web-push-native's `encrypt()` itself calls `p256::SecretKey::random(&mut OsRng)`, using
   `aes_gcm::aead::OsRng`. aes-gcm is pulled with default features, which include `getrandom`. Feature
   unification therefore turns `rand_core/getrandom` on for the shared rand_core 0.6. The
   `getrandom` + `SigningKey::from_bytes` approach still works and is fine to keep. Only the
   stated reason is inaccurate. Reword it, or use `SigningKey::random(&mut OsRng)`.

2. **LOW: the shared-lockfile risk list names a crate that is not shared.**
   In web-push-native 0.5.0, `serde_json` is a dev-dependency only, and `serde` is optional
   (off under `default-features = false`). The real normal deps that can move shared lock
   entries are `http ^1.4.2`, plus anything below aes-gcm, hkdf 0.13 and sha2 0.11 that
   eventrelay also locks. The plan-step-2 gate catches all of these anyway, so no design
   change is needed beyond correcting the list.

3. **LOW: the response headers of `/sw.js` are not fully specified.**
   A service worker runs under the CSP of its own script response. If `/sw.js` gets the
   `page_headers` CSP, `img-src 'self'` covers the notification `icon: /icon-192.png`. If it
   is served with a bare `default-src 'none'` or a different header set, the icon may fail
   to load. State that `/sw.js` uses `page_headers("text/javascript", "no-cache")`, or an
   equivalent that includes `img-src 'self'`.

4. **LOW (note): the lane's authoritative host check must use the same `EndpointPolicy` as subscribe.**
   The mock test admits `http://127.0.0.1:<port>` through a constructor-only
   `EndpointPolicy`, but lane step 4 says it re-checks "against `endpoint_hosts`". Make
   both checks call one policy object, so that the test and production use the same rule.
   Otherwise the mock test either fails or passes through a path production never takes.

None of these blocks implementation. All are wording fixes or small clarifications that
the implementer can make.
