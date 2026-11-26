VERDICT: CHANGES_REQUESTED

## Findings

### MEDIUM — `/webpush/test` has no implementable per-subscription reply path

The route description says a test job is put into every lane with a oneshot reply and must return one result per subscription. The sender design, however, defines one global `Job` with a single `Option<oneshot::Sender<TestResult>>`, sends that job through the dispatcher, and has the dispatcher fan it out to every lane. A oneshot sender is not cloneable, so that job cannot be copied into multiple lanes while preserving one reply per subscription. The dispatcher’s coalescing also has no defined behavior for test jobs and could discard a reply-bearing job.

Specify a separate path for tests. For example, `/webpush/test` can snapshot the lanes and directly `try_send` one independently constructed test job, with its own oneshot sender, to each lane. State that this bypasses global coalescing and freshness while still using each lane’s rate limit and send logic. Define how lanes removed or added during the snapshot appear in the response.

### MEDIUM — Existing-subscription upserts do not update the lane that owns its endpoint and keys

Subscriptions are upserted by endpoint, and the page re-posts an existing subscription on every load. The lane lifecycle only says lanes are created when a subscription is “added or loaded” and end when it is removed. The send steps then use the lane’s endpoint, `p256dh`, and `auth`, but the design does not say whether a lane reads the current record for each send or retains the values it had when its task was created. An upsert can update at least the label and keys while keeping the endpoint and id, leaving the active lane stale. A stale cryptographic snapshot can encrypt to the wrong key and then remove an otherwise valid subscription as an encryption failure.

Define the ownership and refresh rule. Either restart and replace the lane atomically whenever an existing record’s send-relevant fields change, or make the lane fetch a current immutable subscription snapshot by id before each request. Ensure an old lane cannot remove or overwrite a newer record after replacement. Add an integration test that re-subscribes the same endpoint with changed metadata/key material and verifies that the next test push uses the updated record.

### LOW — An eight-hex SHA-256 prefix does not guarantee tag uniqueness

The design repeatedly says two sessions “cannot collide,” but eight hexadecimal characters provide only 32 bits. Collisions are unlikely at this scale, not impossible, and the birthday probability grows much faster than the number of sessions. Use a longer prefix, such as 16 or 32 hex characters, or change the claim and explicitly accept the collision risk.
