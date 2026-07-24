# SPEC-SCHED-002 — Progress

Lifecycle: plan → run → sync. Status: **in-progress** (run-phase implemented; working tree
uncommitted, pending user review + live-DB verification of the deferred `#[ignore]` tests).

## §E.1 Plan-phase Audit-Ready Signal

Plan-phase artifacts complete: `spec.md`, `plan.md`, `acceptance.md`, `progress.md`.

- Tier: **M** (3-5 files: `backfill.rs`, `collection_queue.rs`, `live_poller.rs`, `config.rs`,
  a new `thiserror` dispatch-error type + colocated tests; moderate risk — touches tested retry
  semantics but preserves the claim/lease/fencing machinery; no migration, no new dependency,
  no API change).
- Requirements: REQ-SCHED-060..065 (GEARS), mapped 1:1 to AC-SCHED-060..065 + AC-SCHED-QG.
- Schema investigation (cooldown-deferral): **resolved from evidence** — no migration-free
  timestamp-deferral is safe (no `next_eligible_at` column; `lease_expires_at` overload collides
  with the `backfill-stalled` alarm + un-indexed backfill reclaim). Decision D3: bounded fixed
  sleep raced against shutdown.
- Open items: **0** — OR-SCHED-1 (`LIVE_POLL_CLAIM_BATCH_LIMIT` default) resolved at
  Implementation Kickoff: default **50**, operator-overridable. No unresolved clarification
  markers remain.
- Development mode: TDD. Prerequisite-free (Phase 1; lands first).
- plan-auditor verdict: PASS-WITH-DEBT (0.86)
- plan_complete_at: 2026-07-23T18:34:58Z
- plan_status: audit-ready

## §E.2 Run-phase Evidence

Development mode: **TDD** (RED → GREEN → REFACTOR). Working tree **uncommitted** (per task).

Files changed (7):

| File | Summary |
|------|---------|
| `src/collectors/retry.rs` (NEW) | `DispatchError { Transient, Permanent }` classifier + `max_claims_in_window` retry-bound helper (REQ-SCHED-062/063) |
| `src/collectors/collection_queue.rs` | `RELEASE_QUEUE_SQL` + `FAIL_PERMANENT_QUEUE_SQL`; `dispatch_item` → `Result<DispatchOutcome, DispatchError>`; worker loop soft-skip-release / permanent-fail-fast / transient-retry + bounded shutdown-raced pause; @MX refresh |
| `src/collectors/backfill.rs` | `RELEASE_BACKFILL_SQL` + `FAIL_PERMANENT_BACKFILL_SQL`; `process_chunk` → `Result<ChunkOutcome, DispatchError>`; pacer `Cooldown`/`CreditExhausted` soft-skip via shared `pacer_should_skip_queue`; partial-release routed off `i32::MAX` onto the release SQL; @MX refresh |
| `src/collectors/live_poller.rs` | `LIVE_COIN_CLAIM_SQL` gains `ORDER BY last_polled_at ASC NULLS FIRST LIMIT $3`; `LIVE_COIN_DEFER_SQL` + `defer_coin_poll`; permanent per-coin error → marker-forward defer; 6 `let _ = clear_coin_poll_marker` → `warn!`/`defer`; shutdown-between-coins; guarded main `select!` arm |
| `src/collectors/mod.rs` | `WorkerConfig.live_poll_claim_batch_limit` + `from_env` + wired into `run_live_poller`; `pub mod retry;` |
| `src/config.rs` | `live_poll_claim_batch_limit()` reader — env `LIVE_POLL_CLAIM_BATCH_LIMIT`, default **50** (OR-SCHED-1) |
| (tests colocated in the above) | SQL-shape, classification, retry-bound, mechanical-grep, config-default tests + 5 `#[ignore]` DB-gated regression tests |

### AC status matrix (E1)

| AC | Status | Proving test / verification |
|----|--------|------------------------------|
| AC-SCHED-060a (page-count must not fail a chunk) | PASS (sandbox) + DB-gated | `retry::max_claims_in_window` + release-SQL neutralize tests (sandbox); `backfill::…::db_partial_release_page_walk_does_not_fail_chunk` (DB, `#[ignore]`, deferred) |
| AC-SCHED-060b (long chunk survives mid-run transient) | PASS-WITH-DEBT | covered by 060a page-walk + 060c bound; full 20-page survive→done drive is a live-DB scenario (deferred) |
| AC-SCHED-060c (genuine failures still bound retries) | PASS-WITH-DEBT | `backfill::…::db_genuine_failures_still_bound_retries` (DB, `#[ignore]`, deferred); FAIL_OR_RETRY shape test (sandbox) |
| AC-SCHED-061 (backfill cooldown = backpressure) | PASS | `backfill::…::backfill_classifies_cooldown_as_soft_skip` / `…_credit_exhausted_as_soft_skip` / `…_does_not_soft_skip_not_found` (sandbox; backfill calls the shared `pacer_should_skip_queue`) |
| AC-SCHED-062 (no busy loop; claim ≤ ⌈cooldown/pause⌉+1) | PASS | `retry::…::bound_is_ceil_div_plus_one` / `…_clamps_zero_pause_to_one` / `…_zero_window_is_one_claim` (sandbox) |
| AC-SCHED-063a (permanent fails fast, 1st attempt) | PASS (sandbox) + DB-gated | `collection_queue::…::permanent_fail_sql_is_unconditional_failed` (sandbox); `…::db_permanent_dispatch_fails_fast` (DB, `#[ignore]`, deferred) |
| AC-SCHED-063b (transient retries) | PASS (sandbox) + DB-gated | FAIL_OR_RETRY conditional-status shape (sandbox); `…::db_transient_failure_retries_not_fails` (DB, `#[ignore]`, deferred) |
| AC-SCHED-063c (live_poller permanent consequence) | PASS | `live_poller::…::defer_sql_sets_marker_forward_not_last_polled_at` (sandbox) |
| AC-SCHED-064a (LIMIT present + default 50) | PASS | `live_poller::…::claim_sql_has_limit_clause` / `…::claim_sql_limit_binds_batch_parameter`; `config::…::live_poll_claim_batch_limit_default_is_50` (sandbox) |
| AC-SCHED-064b (shutdown between coins) | PASS-WITH-DEBT | `poll_cycle` checks `*shutdown.borrow()` between coins (code present + guarded); prompt-stop drive is a live-DB scenario (deferred) |
| AC-SCHED-065a (last_error NULL on non-error release) | PASS | `release_sql_resets_pending_and_clears_last_error` (both workers, sandbox) |
| AC-SCHED-065b (no `let _ = clear_coin_poll_marker`) | PASS | `live_poller::…::no_silent_marker_clear_discards_remain` (sandbox); raw grep returns 0 matches |
| AC-SCHED-065c (dropped-sender break, is_err guard) | PASS | `worker_select_arms_guard_dropped_sender` (all 3 worker files, sandbox) |
| AC-SCHED-QG (quality gates clean) | PASS | see §E.3 gate tails |

## §E.3 Run-phase Audit-Ready Signal

```yaml
run_complete_at: 2026-07-24
run_commit_sha: pending-backfill-uncommitted   # working tree left uncommitted per task
run_status: pass-with-debt
ac_pass_count: 9        # AC-060a,061,062,063a,063c,064a,065a,065b,065c fully sandbox-proven
ac_pass_with_debt_count: 4   # AC-060b,060c,063b,064b — live-DB drive deferred (#[ignore])
ac_fail_count: 0
preserve_list_post_run_count: 0   # claim/lease/fencing SQL preserved verbatim; no restructure
l44_pre_commit_fetch: n/a-uncommitted
l44_post_push_fetch: n/a-uncommitted
new_warnings_or_lints_introduced: 0
cross_platform_build:
  note: single-target aarch64/x86_64 Rust service; no cross-OS build tags in scope
quality_gates:
  cargo_test: "ok — 601 lib passed, 0 failed, 63 ignored (5 new DB-gated); all integration binaries pass (0 failed, 15 db_integration ignored)"
  cargo_clippy: "exit 0 — --all-targets --all-features -- -D warnings clean"
  cargo_fmt_check: "exit 0 — clean"
total_run_phase_files: 7
m1_to_mN_commit_strategy: "uncommitted — user reviews before any commit (per task CRITICAL)"
deferred_db_gated_tests:
  - collectors::backfill::tests::db_partial_release_page_walk_does_not_fail_chunk
  - collectors::backfill::tests::db_genuine_failures_still_bound_retries
  - collectors::collection_queue::tests::db_soft_skip_release_does_not_consume_retry_budget
  - collectors::collection_queue::tests::db_permanent_dispatch_fails_fast
  - collectors::collection_queue::tests::db_transient_failure_retries_not_fails
  db_run_command: "DATABASE_URL=postgres://... cargo test -- --ignored"
```

## §E.4 Sync-phase Audit-Ready Signal

_<pending sync-phase — owned by manager-docs>_
