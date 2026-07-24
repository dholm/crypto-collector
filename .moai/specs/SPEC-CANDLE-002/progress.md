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
`247e9b4` (M2 GREEN F-09), `7805800` (M3 F-07/F-08). Plan artifacts: `3c0b978`.

Sandbox has no PostgreSQL, so DB-gated (`#[ignore]`) scenarios are written and compiled but
deferred to the orchestrator's live-DB run (`DATABASE_URL=... cargo test -p crypto-collector --
--ignored --test-threads=1`). Pure scenarios and the full offline gate ran here.

| AC | Scenario | Kind | Status | Verification (actual output) |
|----|----------|------|--------|------------------------------|
| AC-CANDLE-050 | 1 — mixed-source: native preserved, rollup reconciles | DB-gated | PASS-WITH-DEBT | `db_mixed_source_preserves_native_and_reconciles_rollup` written + compiles (`... ignored`); NOT run (no DATABASE_URL) — deferred to orchestrator. Backed offline by `reconcile_window_over_rollup_only_slice_deletes_dropped_rollup_bucket ... ok` + source-filter SQL. |
| AC-CANDLE-052 | 2 — collision: native wins | DB-gated | PASS-WITH-DEBT | `db_collision_native_wins` written + compiles (`... ignored`); deferred. Native-wins `WHERE coin_candles.source LIKE 'rollup:%'` present in `batched_upsert_candles`. |
| AC-CANDLE-055 | 3 — bounded backward history repair | DB-gated + pure | PASS-WITH-DEBT | Pure core: `backward_repair_window_some_when_source_precedes_earliest ... ok`, `backward_repair_window_none_when_watermark_not_before_earliest ... ok`. DB: `db_history_repair_backward_pass_is_idempotent` written + compiles (`... ignored`); deferred. |
| AC-CANDLE-058 | 4a/4b — non-positive projection guard | pure | PASS | `projection_filters_interior_nonpositive_closes_without_panic ... ok`, `projection_current_price_guard_fires_before_interior_filter ... ok` (both panicked "Unable to calculate log10 for zero" pre-fix → RED proven; green post-fix). |
| AC-CANDLE-QG | Quality gate | mechanical | PASS | `cargo test` exit 0 (651 passed, 0 failed, 81 ignored); `cargo clippy --all-targets --all-features -- -D warnings` exit 0; `cargo fmt --check` exit 0; `tests/backtest_projection.rs` 2 passed unchanged; no migration, no dependency (`Cargo.toml` untouched); Decimal-only. |

Invariants:
- `reconcile_window` pure + unchanged (REQ-CANDLE-050): only its SQL feed was narrowed — 6 pre-existing
  reconcile/materialize pure tests still `ok`.
- Backtest-locked constants intact (REQ-CANDLE-060): `tests/backtest_projection.rs` 2/2 `ok` in 24s.
- Memory bound (REQ-CANDLE-056): backward repair reuses `materialize_window_walk` (week-aligned chunk
  walk, `@MX:WARN`), never a full-series load.

## §E.3 Run-phase Audit-Ready Signal

```yaml
run_complete_at: 2026-07-24
run_commit_sha: 7805800            # M3 (final implementation commit); milestones 9a96d22, 247e9b4, 7805800
run_status: pass-with-debt         # pure ACs + quality gate PASS; 3 DB-gated scenarios written+compiled, deferred to orchestrator live-DB run
ac_pass_count: 2                   # AC-CANDLE-058, AC-CANDLE-QG (fully verified here)
ac_fail_count: 0
ac_pass_with_debt_count: 3         # AC-CANDLE-050/052/055 (DB-gated tests written + compiled + #[ignore], not run in sandbox)
preserve_list_post_run_count: 0    # PRESERVE intact — candles_agg.rs / candles.rs / migrations / Cargo.toml / backtest_projection.rs / collection_queue.rs all untouched
l44_pre_commit_fetch: n/a          # subagent does not push; orchestrator owns pre-spawn fetch + push (4 prior unpushed commits)
l44_post_push_fetch: n/a           # push deferred to orchestrator (user controls push/merge)
new_warnings_or_lints_introduced: 0   # clippy -D warnings exit 0
cross_platform_build:              # Rust project (not Go); aarch64 cross-compile is `make push-aarch64` — deployment-time, not run here
  cargo_build_debug: pass          # implied by `cargo test` compile (exit 0)
  aarch64_cross: deferred          # not run in sandbox; deploy-time via `make deploy`
total_run_phase_files: 2           # src/collectors/rollup.rs, src/collectors/cycle_projection.rs
m1_to_mN_commit_strategy: per-milestone separate commits (M1 RED / M2 GREEN F-09 / M3 F-07+F-08); plan artifacts a separate commit; no push (user controls)
```

## §E.4 Sync-phase Audit-Ready Signal

_<pending sync-phase>_
