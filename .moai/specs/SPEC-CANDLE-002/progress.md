# SPEC-CANDLE-002 — Progress

Lifecycle: plan → run → sync. Status: **draft** (plan-phase artifacts authored).

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

_<pending run-phase>_

## §E.3 Run-phase Audit-Ready Signal

_<pending run-phase>_

## §E.4 Sync-phase Audit-Ready Signal

_<pending sync-phase>_
