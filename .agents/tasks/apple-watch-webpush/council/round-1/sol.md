VERDICT: CHANGES_REQUESTED

## Findings

### MEDIUM — A failed subscription can delay delivery to the carried iPhone by minutes

The sender design says subscriptions are processed one after another. Each request has a 10-second total timeout, and a network error or 5xx waits 2 seconds and retries once before moving on. Therefore, one stale subscription ahead of the carried iPhone can delay its Watch notification by about 22 seconds; seven stale subscriptions can delay it by roughly 154 seconds. During that time the single worker also stops draining its 64-job queue, making queue drops more likely. This conflicts with the purpose of a timely `your_turn` or `allow` notification and with the stated rationale that a late notification is noise.

Revise the worker design so one subscription cannot head-of-line block the others. For example, fan out each coalesced job with a small bounded concurrency limit and keep retry/backoff state per subscription. The design should specify how concurrent result updates and `webpush.json` saves are serialized, then add a test where one endpoint times out while another receives the push promptly.

### MEDIUM — Logging the push service's response body breaks the no-secret logging invariant

The error table permits logging Apple's `reason` response field and asserts that it “holds no secrets.” The spike did not establish that property, and `endpoint_hosts` is configurable, so the response is not necessarily from Apple. A push endpoint has received the full endpoint URL in the request target plus authorization and encryption headers; an error response can echo any of that data. Truncation does not make such output safe. This conflicts with the design invariant that endpoint paths, JWTs and subscription keys never enter logs.

Do not log push-service response bodies. Log only locally derived classifications, HTTP status, subscription id and the already-validated endpoint host. If a response field is retained for diagnostics, the design needs a strict parser/allowlist and tests proving endpoint paths, JWTs, key material and payload text are redacted for adversarial responses.

### LOW — The document title appears to resolve Q3 even though the body correctly leaves it open

The title says “(Option A),” while the Q3 section correctly presents A, B and C as an open choice for pahud. Rename the document/title terminology so readers do not mistake the Web Push approach for a decision to notify only the carried iPhone.

## Coverage notes

The design otherwise covers the required state policy (`your_turn`, `allow`, `all_set`, never `working`), pause/filter behavior, subscription and VAPID storage with 0600 files, encrypted payload contents and lock-screen/Watch exposure, configuration, tailnet-only serving, the open Q3 options, and the exclusion of native iOS/watchOS work. The spike supports the stated feasibility gate: tailnet-origin subscription and Watch mirroring were both observed for `your_turn`.
