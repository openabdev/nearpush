# Council review: apple-watch-webpush design, round 3

**Overall verdict: CHANGES_REQUESTED.** sol requested changes, and opus5 approved.

## Reviewer verdicts

- opus5 (`council/round-3/opus.md`): `VERDICT: LGTM`
- sol (`council/round-3/sol.md`): `VERDICT: CHANGES_REQUESTED`

Both reviewers say the round-2 findings are resolved: the MSRV split, the `/webpush/test` path, stale lanes, pause, `Retry-After`, late results, the page key-check order and the tag. Both also confirm that the required coverage is complete.

## Findings (deduplicated)

### F1. MEDIUM (sol): the lane result step can lock `SubSet` twice and deadlock
Step 8 applies the send result while holding the `SubSet` lock. From inside that step it calls `remove_sub(id, Some(rev))` and the async `persist()`, and both of those take `SubSet` themselves. Holding a `std::sync::Mutex` guard across `.await` is also wrong.
**Fix:** apply the result in two phases. Under the lock, validate `(id, rev)`, update the in-memory fields and compute an action (`None`, `Persist` or `Remove(expect_rev)`). Release the guard, then run `persist()` or `remove_sub(id, Some(rev))`. `remove_sub` stays the final compare-and-remove. Add a concurrency test where a result that needs removal or persistence races an upsert that changes the keys.

### F2. LOW (sol): unclear meaning of a 500 when a route save fails
Subscribe, unsubscribe and remove change the in-memory set first and return 500 if `persist()` then fails. The caller cannot tell whether the change was not applied or was applied only until the next restart. A failed unsubscribe save can bring the old subscription back after a restart.
**Fix:** pick one rule. Either roll the change back when the save fails, or keep it, mark the set dirty, retry the flush and document what the 500 means. Add tests that force a write failure, restart, and check the result for both subscribe and unsubscribe.

### F3. LOW (sol): wrong invalid-scalar probability
For P-256, a random 32-byte value is invalid with probability about 2^-32, not 2^-128. Retrying is still sound. Correct the number.

### F4. LOW (opus5): wrong claim that p256 does not enable `rand_core/getrandom`
web-push-native's `encrypt()` calls `SecretKey::random(&mut OsRng)`, and aes-gcm's default features turn on `getrandom`. Feature unification therefore enables it. Keep the approach, but reword the reason or use `SigningKey::random(&mut OsRng)`.

### F5. LOW (opus5): wrong list of shared lockfile risks
In web-push-native 0.5.0, `serde_json` is a dev-dependency only, and `serde` is optional and off. The real shared risks are `http ^1.4.2` and the dependencies under aes-gcm, hkdf 0.13 and sha2 0.11. The plan step 2 gate already catches these, so only the list needs correcting.

### F6. LOW (opus5): `/sw.js` response headers not specified
The service worker runs under the CSP of its own script response. State that `/sw.js` is served with `page_headers("text/javascript", "no-cache")`, or an equivalent that includes `img-src 'self'`, so the notification icon loads.

### F7. LOW, note (opus5): lane host check must use the same `EndpointPolicy` as subscribe
Lane step 4 re-checks the host against `endpoint_hosts`, while the mock test admits `127.0.0.1` through an `EndpointPolicy` set in the constructor. Both checks must call one policy object, so the test and production follow the same rule.

## What round 4 needs
F1 is the blocking finding. F2–F7 are wording fixes or small clarifications, and they should be made in the same revision. Q3 and D2 are still open decisions for pahud.
