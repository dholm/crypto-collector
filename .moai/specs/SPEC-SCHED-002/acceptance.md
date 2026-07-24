# SPEC-SCHED-002 — Acceptance Criteria

Development mode: TDD. Each scenario below maps to one or more REQ-SCHED-06x requirements and to
a required regression test. AC sub-IDs use a trailing lowercase suffix for paired sub-criteria
of one logical AC (this suffix convention applies to acceptance criteria only, never SPEC IDs).

## Scenarios (Given / When / Then)

### AC-SCHED-060 — Non-failure release does not consume the retry budget (REQ-SCHED-060, F-01)

- **AC-SCHED-060a — page-count must not fail a chunk.**
  - **Given** a backfill chunk with `max_attempts = 5`,
  - **When** it is claimed and partial-released more than `max_attempts` times (walking pages),
    and then exactly one genuine failure occurs,
  - **Then** its `status` is `pending` (still retryable), **not** `failed`.
- **AC-SCHED-060b — long chunk survives a mid-run transient failure.**
  - **Given** a simulated 20-page backfill chunk with `max_attempts = 5`,
  - **When** a single transient failure occurs at page 15,
  - **Then** the chunk survives (remains `pending`) and completes on retry (reaches `done`).
- **AC-SCHED-060c — genuine failures still bound retries.**
  - **Given** a chunk/item with `max_attempts = 5`,
  - **When** `max_attempts` genuine failures occur,
  - **Then** the row is marked `failed` (the bound still applies to real failures).

### AC-SCHED-061 — Backfill pacer cooldown is backpressure, not failure (REQ-SCHED-061, F-02)

- **Given** the backfill worker about to process a chunk,
- **When** `acquire_slot` returns `Cooldown` (or `CreditExhausted`),
- **Then** the chunk is released **without** an attempt consumed and the worker idles — matching
  the collection-queue soft-skip behavior (mirror the existing collection_queue soft-skip test).

### AC-SCHED-062 — No busy loop during cooldown (REQ-SCHED-062, F-03)

- **Given** a simulated pacer cooldown affecting a queue worker,
- **When** the worker cannot make progress for the duration of the cooldown,
- **Then** the claim count over the cooldown window is bounded by
  **≤ ⌈cooldown_secs / pause_secs⌉ + 1** (the regression test asserts this concrete bound by
  counting claim invocations — no tight loop), and the pause is raced against the shutdown
  channel so shutdown is still prompt.

### AC-SCHED-063 — Transient vs permanent classification (REQ-SCHED-063, F-04)

- **AC-SCHED-063a — permanent fails fast.**
  - **Given** a queue item dispatching to an unknown coin (permanent condition),
  - **When** it is dispatched,
  - **Then** it is marked `failed` on the **first** attempt (retries are not exhausted), with a
    descriptive `last_error`.
- **AC-SCHED-063b — transient retries.**
  - **Given** a queue item hitting a transient dispatch failure,
  - **When** it is dispatched and fails transiently,
  - **Then** it is reset to `pending` for retry (not failed on the first transient error).
- **AC-SCHED-063c — live_poller permanent consequence.**
  - **Given** a coin that returns a permanent provider error in the live poller,
  - **When** the poll cycle handles it,
  - **Then** the poller applies a consequence beyond log level (the coin is not immediately
    re-due — its re-claim is deferred to a widened interval).

### AC-SCHED-064 — Live-poller claim is bounded and shutdown-aware (REQ-SCHED-064, F-05)

- **AC-SCHED-064a — LIMIT present + configurable (default 50).**
  - **Given** the `LIVE_COIN_CLAIM_SQL` statement,
  - **When** the SQL-shape test inspects it,
  - **Then** it contains a `LIMIT` clause bound to the configured batch size; the
    `live_poll_claim_batch_limit()` config reader returns **50** when `LIVE_POLL_CLAIM_BATCH_LIMIT`
    is unset and returns the override when the env var is set (config-reader unit test).
- **AC-SCHED-064b — shutdown between coins.**
  - **Given** a claimed batch mid-processing,
  - **When** the shutdown signal is set,
  - **Then** the per-coin loop stops promptly (does not process the remainder of the batch).

### AC-SCHED-065 — Diagnostics hygiene (REQ-SCHED-065, F-06 / collector F-47 arms)

- **AC-SCHED-065a — last_error preserved across non-error releases.**
  - **Given** a row carrying a prior genuine `last_error`,
  - **When** it is administratively released (partial / soft-skip),
  - **Then** `last_error` is `NULL` (or a clearly-non-error marker) — the real prior error is
    never overwritten by `"partial"` / `"pacer_skip"`.
- **AC-SCHED-065b — marker-clear failures are logged (mechanical).**
  - A structural/grep assertion confirms **no** `let _ = clear_coin_poll_marker(` call remains in
    `live_poller.rs` — the 6 sites (`live_poller.rs:285/296/301/319/339/346`) each become a
    `warn!`-logged `if let Err(e) = clear_coin_poll_marker(...)` branch.
  - Verification: `grep -n 'let _ = clear_coin_poll_marker(' src/collectors/live_poller.rs`
    returns **0 matches**.
- **AC-SCHED-065c — dropped-sender break (mechanical).**
  - Each collector-worker `tokio::select!` shutdown arm contains an `is_err()`-guarded `break`
    (e.g. `res = shutdown.changed() => { if res.is_err() || *shutdown.borrow() { break } }`), so a
    dropped shutdown sender breaks the loop rather than busy-spinning on the immediate `Err`.
  - Verification: each changed `select!` arm in the worker loops (`backfill.rs`,
    `collection_queue.rs`, `live_poller.rs`) asserts an `is_err()` guard on the `changed()` result.

### AC-SCHED-QG — Quality gates

- **Given** the completed implementation,
- **When** the quality gate runs,
- **Then** `cargo test`, `cargo clippy --all-targets --all-features -- -D warnings`, and
  `cargo fmt --check` are all clean.

## Edge Cases

- A cooldown that outlasts many claim cycles must **not** accumulate attempts (interaction of
  REQ-SCHED-060 + REQ-SCHED-061).
- An empty / fully-out-of-range page (forward-skip) is a non-failure release and must not
  consume the budget (REQ-SCHED-060.1).
- `AcquireSlotError::NotFound` (a genuine pacer misconfiguration) is **not** a soft-skip — it
  must still surface as an error, not be swallowed as backpressure.
- Shutdown during the soft-skip pause must break promptly (the `select!` race), not wait out the
  full pause.
- The `LIVE_POLL_CLAIM_BATCH_LIMIT` default is **50** (OR-SCHED-1 resolved); the LIMIT mechanism
  and shutdown-between-coins behavior are testable independently of the numeric default.

## Required Regression Tests (from the task)

- F-01: claim → partial-release > `max_attempts` times → one genuine failure → assert `pending`,
  not `failed` (AC-SCHED-060a).
- Backfill pacer cooldown → release without attempt consumption, mirroring the existing
  collection_queue soft-skip tests (AC-SCHED-061).
- Permanent dispatch error → item failed on first attempt (AC-SCHED-063a).
- SQL-shape tests updated for every changed statement (claim SQL, new release SQL, live claim
  LIMIT) (AC-SCHED-060, AC-SCHED-064a, AC-SCHED-065a).

## Definition of Done

- [ ] REQ-SCHED-060: non-failure releases do not consume the retry budget (both workers); a row
      is `failed` only after `max_attempts` genuine failures.
- [ ] REQ-SCHED-061: backfill classifies `Cooldown`/`CreditExhausted` as soft-skip (no attempt).
- [ ] REQ-SCHED-062: bounded sleep raced against shutdown after every soft-skip / retryable
      release; no tight DB loop during cooldown.
- [ ] REQ-SCHED-063: `DispatchError` transient/permanent classification; permanent fails fast;
      live_poller per-coin permanent consequence.
- [ ] REQ-SCHED-064: `LIVE_COIN_CLAIM_SQL` `LIMIT` + `LIVE_POLL_CLAIM_BATCH_LIMIT` env +
      shutdown-between-coins (default resolved from OR-SCHED-1).
- [ ] REQ-SCHED-065: `last_error` NULL on non-error release; `warn!` on marker-clear failure;
      dropped-sender break in collector select arms.
- [ ] `@MX` comments on the changed claim/release SQL updated to the new attempt semantics.
- [ ] All required regression tests present and passing.
- [ ] Claim/lease/fencing machinery preserved verbatim; no new migration; no new dependency.
- [ ] `cargo test`, `cargo clippy --all-targets --all-features -- -D warnings`,
      `cargo fmt --check` clean.
