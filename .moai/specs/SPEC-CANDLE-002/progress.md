# SPEC-CANDLE-002 — Progress

Lifecycle: plan → run → sync. Status: **in-progress** (run-phase, TDD RED→GREEN→REFACTOR).

## §E.1 Plan-phase Audit-Ready Signal

Plan-phase artifacts complete: `spec.md`, `plan.md`, `acceptance.md`, `progress.md`.

- Tier: **M** (2 files under change — `src/collectors/rollup.rs`, `src/collectors/cycle_projection.rs`
  — + colocated pure and DB-gated tests; 3-file artifact set. Moderate risk: touches tested
  materializer reconcile semantics but preserves the pure `reconcile_window`/`candles_agg.rs` cores;
  no migration, no new dependency, no API/schema change).
- Requirements: **REQ-CANDLE-050..060** (GEARS), extending SPEC-CANDLE-001's REQ-CANDLE-0xx block.
  Mapped 1:1 to AC-CANDLE-050/052/055/058 + AC-CANDLE-QG.
- Findings: F-07 (High), F-08 (Medium), F-09 (Low) — Category B of `research/idiomatic-rust.md` §6.
- Decisions settled: **D1** collision = native-wins (`ON CONFLICT ... WHERE source LIKE 'rollup:%'`);
  **D2** watermark = query-derived `MIN(ts)` comparison, no migration; **D3** projection guard =
  filter `close <= 0` at series construction + guard `current_price <= 0` before `log10`.
- Open items: **0** — no `[NEEDS CLARIFICATION]` markers remain.
- Development mode: TDD (brownfield). Prerequisite-free vs Phase 1 (SPEC-SCHED-002); MUST land before
  Phase 7.
- plan-auditor verdict: **PASS-WITH-DEBT (0.86, Tier M cleared)** — 1 MAJOR + 2 MINOR delta applied
  (no scope change, no new REQs): (D1 MAJOR) F-09 guard ordering corrected in plan.md §4 +
  acceptance Scenario 4 (guard runs before the interior filter — `current_price` derives from the
  series); (D2 MINOR) REQ-CANDLE-052/059 relabeled to GEARS `(Event-driven)`; (D3 MINOR)
  backward-repair window end-bound handling stated explicitly in plan.md §3.
- plan_complete_at: 2026-07-24
- plan_status: audit-ready

## §E.2 Run-phase Evidence

TDD RED→GREEN→REFACTOR (brownfield). Commits: `9a96d22` (M1 RED F-09 + draft→in-progress),
`247e9b4` (M2 GREEN F-09), `7805800` (M3 F-07/F-08), `30cc987` (DB-gated fixture parent-seed fix).
Plan artifacts: `3c0b978`.

All three DB-gated (`#[ignore]`) scenarios were run green against a live Postgres 16 (ephemeral
container) with `--ignored --test-threads=1` → `test result: ok. 3 passed; 0 failed`. The initial
run failed at fixture setup (missing `tracked_coins` parent → FK `coin_candles_coin_id_fkey1`); the
fix (`30cc987`) seeds the parent and tears down in FK order — production F-07/F-08 SQL was confirmed
correct, no logic change. Pure scenarios + the full offline gate also green.

| AC | Scenario | Kind | Status | Verification (actual output) |
|----|----------|------|--------|------------------------------|
| AC-CANDLE-050 | 1 — mixed-source: native preserved, rollup reconciles | DB-gated | PASS | `db_mixed_source_preserves_native_and_reconciles_rollup ... ok` (live Postgres, `--test-threads=1`). Also `reconcile_window_over_rollup_only_slice_deletes_dropped_rollup_bucket ... ok` offline. |
| AC-CANDLE-052 | 2 — collision: native wins | DB-gated | PASS | `db_collision_native_wins ... ok` (live Postgres). Native-wins `WHERE coin_candles.source LIKE 'rollup:%'` in `batched_upsert_candles` exercised. |
| AC-CANDLE-055 | 3 — bounded backward history repair | DB-gated + pure | PASS | `db_history_repair_backward_pass_is_idempotent ... ok` (live Postgres). Pure core: `backward_repair_window_some_when_source_precedes_earliest ... ok`, `..._none_when_watermark_not_before_earliest ... ok`. |
| AC-CANDLE-058 | 4a/4b — non-positive projection guard | pure | PASS | `projection_filters_interior_nonpositive_closes_without_panic ... ok`, `projection_current_price_guard_fires_before_interior_filter ... ok` (both panicked "Unable to calculate log10 for zero" pre-fix → RED proven; green post-fix). |
| AC-CANDLE-QG | Quality gate | mechanical | PASS | `cargo test` exit 0 (offline 606+backtest 2 passed, 0 failed, 66 ignored); DB-gated `3 passed; 0 failed`; `cargo clippy --all-targets --all-features -- -D warnings` exit 0; `cargo fmt --check` exit 0; `tests/backtest_projection.rs` 2 passed unchanged; no migration, no dependency (`Cargo.toml` untouched); Decimal-only. |

Invariants:
- `reconcile_window` pure + unchanged (REQ-CANDLE-050): only its SQL feed was narrowed — 6 pre-existing
  reconcile/materialize pure tests still `ok`.
- Backtest-locked constants intact (REQ-CANDLE-060): `tests/backtest_projection.rs` 2/2 `ok` in 24s.
- Memory bound (REQ-CANDLE-056): backward repair reuses `materialize_window_walk` (week-aligned chunk
  walk, `@MX:WARN`), never a full-series load.

## §E.3 Run-phase Audit-Ready Signal

```yaml
run_complete_at: 2026-07-24
run_commit_sha: 30cc987            # final run-phase code commit; milestones 9a96d22, 247e9b4, 7805800, 30cc987
run_status: pass                   # all 5 ACs verified: pure + quality gate green; 3 DB-gated scenarios run green vs live Postgres 16 (3 passed, 0 failed)
ac_pass_count: 5                   # AC-CANDLE-050/052/055/058 + AC-CANDLE-QG all verified
ac_fail_count: 0
ac_pass_with_debt_count: 0         # DB-gated ACs promoted to PASS after the live-Postgres run (fixture parent-seed fix 30cc987)
preserve_list_post_run_count: 0    # PRESERVE intact — candles_agg.rs / candles.rs / migrations / Cargo.toml / backtest_projection.rs / collection_queue.rs all untouched
l44_pre_commit_fetch: n/a          # subagent does not push; orchestrator owns pre-spawn fetch + push (4 prior unpushed commits)
l44_post_push_fetch: n/a           # push deferred to orchestrator (user controls push/merge)
new_warnings_or_lints_introduced: 0   # clippy -D warnings exit 0
cross_platform_build:              # Rust project (not Go); aarch64 cross-compile is `make push-aarch64` — deployment-time, not run here
  cargo_build_debug: pass          # implied by `cargo test` compile (exit 0)
  aarch64_cross: deferred          # not run in sandbox; deploy-time via `make deploy`
total_run_phase_files: 2           # src/collectors/rollup.rs, src/collectors/cycle_projection.rs
m1_to_mN_commit_strategy: per-milestone separate commits (M1 RED / M2 GREEN F-09 / M3 F-07+F-08 / DB fixture parent-seed fix 30cc987); plan artifacts a separate commit; no push (user controls)
```

## §E.4 Sync-phase Audit-Ready Signal

_<pending sync-phase>_
