# SPEC-CANDLE-002 — Progress

Lifecycle: plan → run → sync. Status: **completed** (3-phase close; sync-auditor
PASS-WITH-DEBT — both findings remediated and verified green; see §E.4).

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
`247e9b4` (M2 GREEN F-09), `7805800` (M3 F-07/F-08), `30cc987` (DB-gated fixture parent-seed fix),
`b287979` (sync-audit remediation — F1 1d backward-repair termination + F2 native-survival DB test).
Plan artifacts: `3c0b978`.

Four DB-gated (`#[ignore]`) scenarios run green against a live Postgres 16 (ephemeral container)
with `--ignored --test-threads=1` → `test result: ok. 4 passed; 0 failed`. The first DB run failed
at fixture setup (missing `tracked_coins` parent → FK `coin_candles_coin_id_fkey1`); the fix
(`30cc987`) seeds the parent and tears down in FK order — production F-07/F-08 SQL confirmed correct,
no logic change. Sync-audit remediation (`b287979`): F1 — `backward_repair_window` trigger made
target-interval-aware (day bucket for 1d, week bucket for 1w) so the 1d pass self-terminates instead
of re-firing every recompute (walk start stays week-aligned for the memory bound); plan.md §3
self-termination claim corrected. F2 — added `db_native_row_without_source_in_window_survives_reconcile`
(native row at a no-source ts inside the window must survive the reconcile DELETE). Pure suite: 12
rollup + 10 projection tests green. Full offline gate also green.

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
run_commit_sha: b287979            # final run-phase code commit; milestones 9a96d22, 247e9b4, 7805800, 30cc987, b287979 (F1/F2 sync-audit remediation)
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

### sync-auditor verdict (independent quality review)

sync-auditor scored SPEC-CANDLE-002 **PASS-WITH-DEBT (~0.89 harmonic mean)** across the
4-dimension scoring (Functionality 92 / Security 95 / Craft 76 / Consistency 90). The must-pass
firewall held — the F-07 three-layer defense (source-filtered SELECT + source-filtered DELETE +
native-wins upsert) and the F-09 guard-ordering correctness both verified without qualification.
Two findings were recorded as debt:

- **F1 — 1d backward-repair non-termination.** The initial `backward_repair_window` trigger used a
  WEEK-aligned bucket comparison for both `1d` and `1w` targets. Because `week_bucket(source_min) <
  day_bucket(source_min)` holds for ~6/7 of coins (any non-epoch-Thursday day), the `1d` backward
  pass would re-fire on every recompute instead of self-terminating after the first repair.
- **F2 — missing native-survival regression at a no-source `ts`.** The DB-gated test suite proved
  native-wins on a `ts` that collides with an *emitted* rollup bucket (Scenario 2), but lacked a
  test proving a native row at a `ts` with **no** corresponding rollup emission also survives the
  reconcile DELETE inside the affected window.

### Debt closure — remediation + verification

Both findings are **REMEDIATED and verified green**, commit `b287979` (code) + `63d7dae` (§E
evidence):

- **F1 fix**: `backward_repair_window`'s trigger comparison is now **target-interval-aware** — it
  compares `bucket_start(source_min, target_secs)` (the DAY bucket for `1d`, the WEEK bucket for
  `1w`) against `earliest_materialized`, while the walk **start** stays WEEK-aligned (preserving
  the memory bound). After the first repair, the earliest materialized bucket moves back to the
  target-interval bucket of `source_min`, so the next run's core returns `None` for both `1d` and
  `1w` — no repeat. Proven by
  `backward_repair_window_some_when_source_precedes_earliest ... ok` and
  `..._none_when_watermark_not_before_earliest ... ok` (pure, offline).
- **F2 fix**: added
  `db_native_row_without_source_in_window_survives_reconcile` — a native row at a no-emission `ts`
  inside the reconcile window is asserted byte-identical after a full reconcile cycle. Green
  against live Postgres 16, `--test-threads=1`.

### Offline + DB-gated evidence (re-confirmed at sync)

```
$ cargo test                                                  → 606 passed, 0 failed (offline);
                                                                  backtest 2 passed unchanged
$ cargo clippy --all-targets --all-features -- -D warnings    → exit 0, clean
$ cargo fmt --check                                            → exit 0, clean
$ DATABASE_URL=postgres://... cargo test -- --ignored --test-threads=1
  db_mixed_source_preserves_native_and_reconciles_rollup ... ok
  db_collision_native_wins ... ok
  db_history_repair_backward_pass_is_idempotent ... ok
  db_native_row_without_source_in_window_survives_reconcile ... ok
  test result: ok. 4 passed; 0 failed
$ cargo test --test backtest_projection                        → 2 passed; 0 failed (unchanged)
```

No new migration (`Cargo.toml`/migrations untouched), no new dependency, Decimal-only monetary
paths preserved.

### @MX validation (sync sub-step)

All @MX Annotation Targets named in `plan.md` § MX Tag Targets are present and well-formed — no
additions required:

- `incremental_recompute_target` — `@MX:ANCHOR` + `@MX:REASON` (data-loss prevention) +
  `@MX:SPEC SPEC-CANDLE-002 REQ-CANDLE-050 REQ-CANDLE-051 REQ-CANDLE-053 REQ-CANDLE-054
  REQ-CANDLE-055` (rollup.rs:345-354).
- `batched_upsert_candles` — `@MX:ANCHOR` (native-wins collision contract) +
  `@MX:REASON` (fan_in >= 3 + collision policy D1) + `@MX:SPEC ... SPEC-CANDLE-002 REQ-CANDLE-052`
  (rollup.rs:158-174).
- `materialize_window_walk` (backward-repair reuse) — `@MX:WARN` (memory-bounded) +
  `@MX:REASON` (OOM prevention) + `@MX:SPEC ... SPEC-CANDLE-002 REQ-CANDLE-055 REQ-CANDLE-056`
  (rollup.rs:235-242).
- `project_composite` — pre-existing `@MX:ANCHOR` (continuity boundary), unaffected — the F-09
  guard did not require a new tag per plan.md §5 (no cycle_projection.rs entry in the MX Tag
  Targets table).

No missing tags found; no churn applied.

### Sync-phase close signal

```yaml
sync_complete_at: 2026-07-24
sync_status: completed
sync_auditor_verdict: pass-with-debt
sync_auditor_score: 0.89
sync_auditor_dimensions:
  functionality: 92
  security: 95
  craft: 76
  consistency: 90
debt_closed: true
debt_closure_evidence: "F1 target-interval-aware trigger + F2 native-survival DB test, both green (offline + live Postgres 16, --test-threads=1)"
remediation_commit: b287979
remediation_evidence_commit: 63d7dae
run_commit_sha: b287979
sync_commit_sha: e71d9baae255af614405636640c589d6c08bd89c
frontmatter_status_transitions:
  in-progress_to_implemented_to_completed: sync-commit (this commit)
mx_validation: pass — all plan.md § MX Tag Targets present and well-formed, no additions needed
```
