VERDICT: CHANGES_REQUESTED

Reviewer: opus5. Round 1. Reviewed `design.md` against `spike-report.md` and the code in
`.worktrees/apple-watch-webpush` (server.rs, relay.rs, event.rs, config.rs, control.rs,
store.rs, assets.rs, assets/index.html, crates/nearpush/src/hook.rs, crates/eventrelay,
README.md). I also checked the published web-push-native 0.5.0 `Cargo.toml` and `src/lib.rs`
on docs.rs.

## Required coverage

All of these are present and match the code:

- Subscription storage: `webpush.json`, 0600, atomic write, 8-device cap, `key_id` binding, damaged file moved aside.
- VAPID key handling: PKCS#8 PEM, 0600, `create_new`, never printed, created only when enabled.
- States: only `your_turn`/`allow`/`all_set` push. `working` is rejected at config load and again in `should_push`. Generic events never push.
- Config keys: listed, with validation rules.
- Pause, filter and `/control`:
  - `ev.muted` is set in `accept_event` from `paused`, which I confirmed in server.rs:336.
  - Pushes follow the display filter, the same rule as `Event::visible`.
- Tailnet only, never funnel: kept, including the rollout on a separate port that needs approval.
- Secrets: never logged or returned. `/status` shows only the endpoint host.
- Q3: left open, with options A, B and C.
- No native iOS or watchOS app.

These claims hold against the code:

- `accept_event` is the only path for both `POST /push` and the relay.
- A restart sends nothing, because events loaded from `events.jsonl` do not pass through `accept_event`.
- The relay dedupe stops double pushes after a crash. `store.push` persists before `notify`, and on restart `run` seeds `seen` from the stored `relay_id`s.
- The CSP needs `worker-src 'self'`.
- The web-push-native 0.5 API the design uses exists: `WebPushBuilder::new`, `with_valid_duration` (sets `TTL`), `build`, and `decrypt` for the mock.
- web-push-native 0.5 declares MSRV 1.87.

## Findings

### MEDIUM 1: A relay backlog pushes stale events as if they were new

The design says: "A relay backlog after the laptop wakes therefore yields at most one push per session". It treats this as acceptable. It also says "A late 'your turn' is noise" and uses that to justify having no retry. The two statements conflict.

- `relay::to_new_event` calls `new.validate(now_ms())`. Every relay event is therefore stamped with the time it is pulled, not when it happened.
- `RelayEvent` parses only `id`, `source` and `body`. The relay does send `received_ms` (eventrelay `http.rs:156`), but nearpushd drops it.
- eventrelay keeps events for up to 7 days (`EVENTRELAY_MAX_AGE_DAYS` default 7).

Result: after a night with the lid closed, every remote session with a pending `your_turn`, `allow` or `all_set` buzzes the Watch on wake. That can be up to 20 pushes a minute per device. Each push reads as current ("now" on the Watch, as in spike photo IMG_6219), even if the event is hours old. The 500 ms coalescing and the rate limit do not help, because the events belong to different sessions.

Requested change:

- Carry the event's origin time to the push decision. Options are the relay `received_ms`, kept as a transient field that is not stored, or an explicit `age` passed to `notify`.
- Add a rule to `should_push`: do not push an event whose origin time is older than a maximum age. Defaulting to `ttl_secs` is a natural fit. It is the same freshness bound Apple applies.
- Add a unit test and an integration case: a relay page with an old `received_ms` produces no request at the mock.
- State this rule in the "Which events push" list.

### MEDIUM 2: The privacy section misstates what `label` and `detail` show on the lock screen and Watch

The task asks the design to state the payload's privacy correctly. Two descriptions do not match the code.

- **`label` is usually the raw first prompt line, not a summary.**
  - The design says the default "exposes the summarized session label (derived from prompts)".
  - In `accept_event`, the summarized label replaces the hook's label only if `sessions.label(s)` already has one. The summarizer runs after the event is stored (`sum.request(s)` at the end).
  - So the first push of a session, and every push until the summary lands, carries the hook's provisional label. That label is the user's prompt text, verbatim and truncated. hook.rs tests show `"修正登入轉址的問題，然後補測試"` and `"Add a hook subcommand to nearpush"`.
  - With the summarizer off, pushes never carry a summary.
- **`detail` for `allow` is the tool and its input, not "the question line".**
  - hook.rs:521 sets `detail = tool_detail(tool, input)`. Examples are `"execute_bash: GITHUB_TOKEN=… gh pr list"`, `"execute_bash: cargo test"` and `"read_file: src/main.rs"`.
  - Env values are redacted, but command lines and file paths still show on the wrist and the lock screen.
  - `your_turn` can also carry `"N wf waiting"`.

Requested change:

- Rewrite the `label`/`detail` bullets and the "What third parties see → lock screen and Watch" paragraph to state these facts.
- Make the default an explicit, argued choice. Options are `label` with an accurate warning, or `state`. Given the corrected facts, consider presenting the default to pahud rather than settling it silently.
- If `label` stays the default, consider sending only a summarized label when one exists and the session tag `#xxxx` otherwise. That would keep raw prompt text off the wrist by default.

### LOW 3: The reason for skipping web-push-native's `vapid` feature is wrong

The design says the `vapid` feature "pulls `jwt-simple`, whose default backend is BoringSSL". web-push-native 0.5.0's `Cargo.toml` declares `jwt-simple = { version = "0.12.17", default-features = false, features = ["pure-rust"], optional = true }`. So the feature pulls no BoringSSL.

Hand-rolling the JWT may still be the better choice, because jwt-simple pure-rust brings a large multi-algorithm tree. Please:

- correct the reason, and
- note that the planned JWT test is self-consistent only. It verifies with the same `p256` key that signed. Per the measurement rule, that cannot catch a wrong `aud` format or a header encoding mistake.

Name the independent check explicitly. The cheapest is to require a 201 from `web.push.apple.com` in the separate-instance test before rollout. A second option is a known-answer comparison with pywebpush, which is still in the spike venv. Also cite the spike as the evidence that a 12 h `exp` is accepted: pywebpush's default `exp` is now + 12 h.

### LOW 4: The VAPID `aud` should be the endpoint origin, including any non-default port

The claim is written as `"<scheme>://<endpoint host>"`. The mock end-to-end test uses `http://127.0.0.1:<port>`, so the claim must include the port. Write it as "the endpoint's origin (scheme, host, port if not default)".

### LOW 5: Writes to `webpush.json` need one writer at a time

Writes come from route handlers (subscribe, unsubscribe, remove) and from the worker (send results, 404/410 removals). They all go through the same tmp path. `control.json` is protected by `control_write`, but the design gives `webpush.json` no equivalent.

A torn write would make the next start move the file aside and drop every device. Pushes would then stop until each phone reopens the app and re-POSTs its subscription. State that one lock is held from snapshot through tmp write and rename. Alternatively, all saves could go through the worker.

### LOW 6: The notification `tag` can collide between sessions

`np-` plus the last 4 characters of the session leaves only a 4-character space. Relay sessions are `"<source>/<id>"`, and some session id schemes share suffixes. A collision would make iOS replace one session's notification with another's. Use a short hash of the full session id instead, for example the first 8 hex characters of its SHA-256.

### LOW 7: Payload size leaks label length to Apple

`web_push_native::encrypt` writes one record of length `message.len() + 17` with no padding. Apple therefore sees each push's size, and the size tracks the label and detail length. The design lists size as metadata, which is accurate. Padding the JSON to a fixed size would remove most of the signal at no real cost, for example a `"p"` filler field up to a fixed 512 or 1024 bytes. Either add the padding or say plainly that it was left out on purpose.

### LOW 8: The `/webpush/test` response is not specified

State that it returns one entry per subscription with the `id`, label, HTTP status and error class, and never the endpoint path. This keeps the "never returned" invariant checkable.

### LOW 9: The Q3 framing leans toward C

The row for C says "The base design already is this". The behaviour is accurate: nothing pushes until Enable is tapped. Still, add one sentence saying the base implements no default for the SE, so that it does not read as a pick for C. The decision stays open as required, so this is wording only.

## Summary

The design is sound and covers every required area. It fits the existing auth, storage and pipeline code closely. Two MEDIUM items block approval:

- relay backlog events can buzz the Watch hours late, as if they were new;
- the privacy section understates what the default payload shows on the wrist and lock screen.

Both are small design edits.
