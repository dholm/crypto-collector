---
id: SPEC-SCHED-002
title: "Collector Worker Retry & Backpressure Correctness"
version: "0.1.0"
status: in-progress
created: 2026-07-23
updated: 2026-07-24
author: manager-spec
priority: High
phase: "v0.2.0"
module: "src/collectors"
lifecycle: spec-anchored
tags: "collectors, retry, backpressure, pacer, backfill, tdd"
issue_number: null
related_specs: [SPEC-SCHED-001, SPEC-DB-001, SPEC-PROV-001]
tier: M
---

# SPEC-SCHED-002 — Collector Worker Retry & Backpressure Correctness

Behavioral-correctness hardening of the three background collection workers defined in
[SPEC-SCHED-001](../SPEC-SCHED-001/spec.md) (live-quote poller, collection-queue worker,
historical backfill worker). The lease / heartbeat / `FOR UPDATE SKIP LOCKED` / zombie-fencing
machinery is correct and tested; the **policy layered on top of it** — attempt counting,
backpressure classification, inter-cycle pacing, and error classification — carries the
highest-operational-risk defect cluster in the worker layer: a routine upstream cooldown can
silently and permanently kill deep backfill.

Findings addressed: **F-01, F-02, F-03, F-04, F-05, F-06** (and the collector-worker arms of
F-47), Category A of `research/idiomatic-rust.md` §6. This is Phase 1 of the implementation-
improvement sequence and is prerequisite-free.

Schema contract: [SPEC-DB-001](../SPEC-DB-001/spec.md) (`collection_queue`, `backfill_*`).
Upstream contract: [SPEC-PROV-001](../SPEC-PROV-001/spec.md) (the `Provider` chain + pacer).

## HISTORY

- 2026-07-23 (v0.1.0): Initial draft. Establishes REQ-SCHED-060..065 — the worker retry /
  backpressure correctness block — refining REQ-SCHED-027's retry-bound semantics (the bound
  counts genuine failures, not pages walked).

## Goal

Make the collector workers' retry and backpressure behavior correct so that:

1. Releasing a backfill chunk or queue item for a **non-failure** reason (a multi-page partial
   release, a pacer soft-skip) never consumes the item's retry budget.
2. Pacer cooldown / credit exhaustion is treated as **backpressure**, never as a failure, in
   every worker (backfill is currently the outlier).
3. Workers **sleep between claim cycles** (raced against shutdown) instead of busy-looping the
   database during a cooldown.
4. Dispatch errors are classified **transient vs permanent** and handled differently —
   permanent errors fail fast, transient errors retry.

Externally visible behavior changes are limited to the retry / pacing semantics above. API
behavior, persisted data shapes, and the claim/lease/fencing machinery are untouched.

## Scope

In scope: `src/collectors/backfill.rs`, `src/collectors/collection_queue.rs`,
`src/collectors/live_poller.rs`, the collector-related env config in `src/config.rs`, a new
`thiserror` dispatch-error type, and the regression / SQL-shape tests colocated in those files.

## Decisions Restated (from SPEC-SCHED-001 — preserved invariants)

- **D-R1 — Claim/lease/fencing preserved exactly.** The `FOR UPDATE SKIP LOCKED` claim, the
  lease + heartbeat renewal, and the `AND claimed_by = $self` zombie-fencing guard
  (REQ-SCHED-011/014/015/021/022) are correct and tested. This SPEC preserves them verbatim
  and MUST NOT restructure them.
- **D-R2 — REQ-SCHED-027 retry-bound clarified.** REQ-SCHED-027 bounds *retries*. The current
  implementation increments `attempts` at claim time, so multi-page partial re-claims count
  *pages* toward the bound (F-01). This SPEC clarifies that the bound counts **genuine
  failures**, and REQ-SCHED-060 governs how that is achieved.

## Decisions This SPEC Makes

- **D1 — Non-failure release does not consume the retry budget.** Administrative releases
  (partial multi-page release, pacer soft-skip) route through a path that leaves the effective
  attempt count unchanged; only genuine failures count toward `max_attempts`. (Mechanism —
  dedicated release SQL vs. relocating the claim-time increment — is a run-phase choice
  recorded in plan.md; the observable contract is that non-failure releases never advance the
  budget.)
- **D2 — Pacer backpressure is uniform across all three workers.** `Cooldown` and
  `CreditExhausted` are soft-skips (release without an attempt, then idle) in the backfill
  worker, matching `pacer_should_skip_queue` / `pacer_should_skip`.
- **D3 — Inter-cycle pacing uses a bounded sleep raced against shutdown, NOT timestamp
  deferral.** Grounded in the schema investigation (plan.md § Schema Investigation): the
  claim tables carry no `next_eligible_at` column, and overloading `lease_expires_at` for
  cooldown deferral collides with the existing `backfill-stalled` alarm and an un-indexed
  reclaim path. Timestamp-deferral is deferred to a future migration-bearing phase.
- **D4 — Dispatch errors are classified transient vs permanent** via a small `thiserror` type;
  permanent conditions fail immediately, transient conditions retry; the live poller gains a
  per-coin consequence for permanent errors beyond log level.
- **D5 — The live claim batch is bounded by a configurable LIMIT**, and the per-coin loop
  honors the shutdown signal between coins.
- **D6 — Diagnostics hygiene.** Administrative releases write a non-error marker (`NULL`) to
  `last_error`; silent marker-clear failures are logged at `warn!`; `tokio::select!` shutdown
  arms break on a dropped sender rather than busy-spin.

## Domain Model

- **backfill chunk** (`backfill_chunks`): the claimable unit of a historical backfill. A
  multi-page chunk is claimed once per page and released back between pages until its range is
  exhausted. `attempts` currently counts claims; this SPEC makes it count genuine failures.
- **queue item** (`collection_queue`): a claimable coin/market collection task. Soft-skipped on
  pacer backpressure; retried on genuine failure.
- **due coin** (`tracked_coins`): a coin eligible for a live-quote poll, claimed via a
  self-expiring `live_poll_claimed_until` marker.
- **pacer outcome** (`AcquireSlotError`): `Cooldown` / `CreditExhausted` = backpressure (soft);
  `NotFound` = a genuine misconfiguration error (hard).
- **dispatch outcome**: today `Result<bool, String>` (queue) or `Result<_, String>` (backfill),
  which erases the transient/permanent distinction; this SPEC introduces a classified error.

## Requirements (GEARS)

### Release ≠ failure (F-01)

- **REQ-SCHED-060.1** — When the backfill worker releases a claimed chunk for a non-failure
  reason (a partial multi-page release or a forward-skip of an empty page), the backfill worker
  **shall not** consume the chunk's retry budget.
- **REQ-SCHED-060.2** — When the collection-queue worker releases a claimed item for a
  non-failure reason (a pacer soft-skip), the collection-queue worker **shall not** consume the
  item's retry budget.
- **REQ-SCHED-060.3** — The backfill worker **shall** mark a chunk `failed` only after
  `max_attempts` genuine **retryable/transient** dispatch or persistence failures, and the
  collection-queue worker **shall** mark an item `failed` only after `max_attempts` genuine
  **retryable/transient** dispatch failures (permanent failures fail-fast per REQ-SCHED-063.2,
  without consuming the retry budget).

### Backfill pacer backpressure classification (F-02)

- **REQ-SCHED-061.1** — When `acquire_slot` returns `Cooldown` or `CreditExhausted` inside the
  backfill worker, the backfill worker **shall** treat it as backpressure: release the chunk
  without consuming an attempt, then idle — mirroring the collection-queue worker's
  `pacer_should_skip_queue` classification.
- **REQ-SCHED-061.2** — The backfill worker **shall not** classify pacer cooldown or credit
  exhaustion as a genuine failure.

### No busy loops (F-03)

- **REQ-SCHED-062.1** — When the backfill worker or the collection-queue worker performs a
  soft-skip or retryable-failure release, the worker **shall** sleep for a bounded pause, raced
  against the shutdown watch channel via `tokio::select!`, before issuing the next claim.
- **REQ-SCHED-062.2** — While a provider is in pacer cooldown, each queue worker **shall** issue
  at most approximately one claim per pause interval (no tight loop against the database).

### Error classification (F-04)

- **REQ-SCHED-063.1** — The dispatch layer **shall** classify each dispatch failure as either
  transient or permanent via a dedicated `thiserror` error type.
- **REQ-SCHED-063.2** — When a dispatch failure is permanent (coin not found, no provider
  supports the required capability, or an unknown dispatch kind), the worker **shall** mark the
  item `failed` immediately on the first attempt — a permanent failure is terminal and does not
  consume or rely on the `max_attempts` retry budget (self-consistent with REQ-SCHED-060.3,
  whose bound counts only genuine retryable/transient failures).
- **REQ-SCHED-063.3** — When a dispatch failure is transient, the worker **shall** follow the
  retry-with-backoff path.
- **REQ-SCHED-063.4** — When the live poller encounters a permanent per-coin error, the live
  poller **shall** apply a consequence beyond log level — at minimum, skip re-claiming that
  coin until a widened interval.

### Live-poller bounds (F-05)

- **REQ-SCHED-064.1** — The live-poller claim SQL **shall** bound each claim batch with a
  `LIMIT` sized so a full batch completes within the claim TTL.
- **REQ-SCHED-064.2** — Where a `LIVE_POLL_CLAIM_BATCH_LIMIT` environment variable is set, the
  live poller **shall** use it as the claim-batch bound; otherwise the live poller **shall** use
  the default batch limit of **50**, consistent with the env-only config convention (no config
  files). (Default derivation: 50 coins × ~2 s per-coin cost ≈ 100 s stays within the 120 s
  claim TTL.)
- **REQ-SCHED-064.3** — When processing a claimed batch, the live poller **shall** check the
  shutdown signal between coins and stop promptly on shutdown.

### Diagnostics hygiene (F-06, collector-worker arms of F-47)

- **REQ-SCHED-065.1** — When a worker releases a claimed row for a non-failure reason, the
  worker **shall** write `NULL` (or a clearly-non-error marker) to `last_error` rather than
  overwrite a prior genuine error with `"pacer_skip"` / `"partial"`.
- **REQ-SCHED-065.2** — When a marker-clear operation fails, the live poller **shall** log the
  failure at `warn!` rather than discard it silently via `let _ =`.
- **REQ-SCHED-065.3** — When a collector worker's `tokio::select!` shutdown arm observes
  `changed()` returning `Err` (the shutdown sender was dropped), the worker **shall** break out
  of its loop rather than busy-spin on the immediate error.

## Exclusions

This SPEC is deliberately narrow. The following are **out of scope** for this phase and are
tracked elsewhere in `research/idiomatic-rust.md`.

### Out of Scope — schema migrations

- No new migration is introduced this phase. A dedicated `next_eligible_at` deferral column
  and its `backfill-stalled` alarm-predicate update are deferred to a later phase (see plan.md
  § Schema Investigation for why timestamp-deferral is not migration-free today).

### Out of Scope — pacer internals

- F-13 (60 s wait clamp), F-16 (slot charged to the first capable provider, not the serving
  one), F-17 (`signal_cooldown` shortening a longer cooldown), and F-18 (`Contended` vs
  `NotFound`) are separate pacer findings, not this phase.

### Out of Scope — provider transport & data correctness

- F-10..F-28 (Categories C and D: HTTP timeouts, `is_transient` HTTP-status discrimination,
  tier/header config, interval-string canonicalization, scientific-notation parsing) are not
  in scope. Note: F-12 (`is_transient` classifies all `Http{..}` as transient) is adjacent to
  REQ-SCHED-063 but is a provider-layer fix owned by a later phase; this SPEC classifies at the
  dispatch layer only.

### Out of Scope — lifecycle, supervisor & non-collector shutdown arms

- F-46 (supervisor consolidation + capped-exponential restart backoff) and the non-collector
  F-47 arms (`src/listener.rs:80-85`, `src/alarm/reconciler.rs` ticker/select arms) belong to
  the lifecycle/observability phase. Only the collector-worker select arms are in scope here.

### Out of Scope — duplication consolidation

- The shared claim / heartbeat / fallback-chain scaffolding duplicated between the two queue
  workers (F-47 architecture arm / F-53) is not consolidated here. Fixes are applied in-place to
  each worker, matching the existing structure.

### Out of Scope — API and externally visible contracts

- The `/v1` REST API behavior, OpenAPI contract, and persisted row shapes are untouched.

## @MX Annotation Targets (high fan_in)

- `CLAIM_BACKFILL_SQL` / `CLAIM_QUEUE_SQL` (existing `@MX:ANCHOR`, fan_in ≥ 3): the `@MX:REASON`
  / `@MX:NOTE` sub-lines that today say "attempts incremented at claim time to bound retries"
  MUST be updated to describe the new semantics (non-failure releases do not count toward the
  bound). Reference SPEC-SCHED-002 REQ-SCHED-060 alongside the existing REQ-SCHED-013/027.
- The new administrative-release SQL (partial / soft-skip path) warrants an `@MX:WARN` with an
  `@MX:REASON` stating it neutralizes the claim-time increment and writes a non-error
  `last_error`, so a future editor does not "restore" the old failure-SQL reuse.
- `FAIL_OR_RETRY_BACKFILL_SQL` / `FAIL_OR_RETRY_QUEUE_SQL`: update the `@MX:NOTE` to record that
  this is now the genuine-failure-only path.
- `LIVE_COIN_CLAIM_SQL` (existing `@MX:ANCHOR`): update to note the new `LIMIT` bound and its
  relationship to the claim TTL.

## Open Items (do not guess — resolve at Implementation Kickoff)

- **OR-SCHED-1 (RESOLVED)** — The `LIVE_POLL_CLAIM_BATCH_LIMIT` default is **50** (resolved at
  Implementation Kickoff by user decision; derivation in plan.md § 5). Operator-overridable via
  the env var. No open clarification remains for this SPEC.
