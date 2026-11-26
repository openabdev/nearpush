VERDICT: CHANGES_REQUESTED

## Findings

### MEDIUM — The lane result algorithm can recursively lock `SubSet` and deadlock

The design says step 8 applies a send result "under the `SubSet` lock" and, from that locked result step, calls `remove_sub(id, Some(rev))` for 404/410 or encryption errors and calls async `persist()` for metadata changes. Both helpers acquire `SubSet` themselves: `remove_sub` removes the record and lane sender under that mutex, while `persist()` takes `save_lock` and then locks `SubSet` to serialize a snapshot. A direct implementation of the specified sequence therefore self-deadlocks; holding the `std::sync::Mutex` guard across an awaited `persist()` is also incompatible with the intended short, non-async critical sections.

Specify a two-phase result application. Under `SubSet`, validate `(id, rev)`, update only in-memory fields that can be updated there, and compute an action such as `None`, `Persist`, or `Remove(expect_rev)`. Release the guard before awaiting `persist()` or invoking `remove_sub`. For removal, `remove_sub(id, Some(rev))` must remain the final compare-and-remove operation so a key upsert between the two phases is preserved. Add a focused concurrency test where a result requiring removal or persistence races a key-changing upsert; the existing race tests cover outcomes but the design should explicitly prevent the lock-order deadlock.

### LOW — Route persistence failures leave subscription state with ambiguous commit semantics

Subscribe, unsubscribe, and remove mutate the in-memory set before `persist()`, then return 500 if the save fails, while the design also says the in-memory set remains authoritative and only background saves retry on a later change. The caller cannot tell whether a 500 means "not applied" or "applied only until restart." In particular, a failed unsubscribe save can resurrect the old subscription after restart after the browser has already unsubscribed locally.

Define one consistent rule: either roll back the route mutation when persistence fails, or retain it and arrange an explicit retry/dirty-state flush while documenting that the 500 means the durable commit is pending. Tests should cover a forced write failure followed by restart for subscribe and unsubscribe.

### LOW — The stated invalid-scalar probability is wrong

For P-256, uniformly random 32-byte values are invalid with probability roughly `(2^256 - n) / 2^256`, about `2^-32`, not `2^-128`. Retrying `SigningKey::from_bytes` is still sound, but the design should correct the probability so the security reasoning is accurate.

## Coverage assessment

The design otherwise covers the required subscription storage, VAPID key handling and 0600 secret files, allowed push states (`your_turn`, `allow`, `all_set`, never `working`), payload privacy and Apple-visible metadata, config, pause/filter/control interaction, tailnet-only serving, secret-safe logging, open Q3 options, and the exclusion of native iOS/watchOS work. The round-2 test-path, lane-refresh, pause, hold, late-result, key-rotation, and tag findings are substantively addressed.
