# Progress — SPEC-REFACTOR-001 (Phase 7: Batching & Structural Debt Reduction)

## §E.1 Plan-phase Audit-Ready Signal

- **Status**: `draft` — plan-phase artifacts authored by manager-spec (2026-07-27); revised
  post-Implementation-Kickoff-Approval (open items resolved) 2026-07-27.
- **Artifacts created (full Tier-L 5-artifact set)**: `spec.md`, `plan.md`, `acceptance.md`,
  `design.md`, `research.md` (+ `progress.md` for the §E lifecycle skeleton) under
  `.moai/specs/SPEC-REFACTOR-001/`. Matches Tier L = 5 files.
- **SPEC ID self-check**: `SPEC-REFACTOR-001` — decomposition `SPEC ✓ | REFACTOR ✓ | 001 ✓ → PASS`
  (regex `^SPEC(-[A-Z][A-Z0-9]*)+-[0-9]{3}$` → PASS).
- **Requirements**: 33 GEARS requirements (REQ-REFACTOR-010..083) across 6 milestones + cross-cutting.
- **Acceptance**: 34 scenarios (AC-REFACTOR-010..083, incl. AC-REFACTOR-062b) + edge cases + quality
  gates + DoD.
- **Two intended behavior changes isolated**: REQ-REFACTOR-021 (per-provider chain pacing, F-16),
  REQ-REFACTOR-042 (no NOTIFY on backfill, F-51). All else behavior-preserving (REQ-REFACTOR-080),
  including the literal-anchor interval work (REQ-REFACTOR-060..064), which is characterization-
  preserving (identical seconds, identical API 400) — NOT an intended change.
- **Constraints encoded**: trait object-safety (REQ-REFACTOR-011), no new deps + Decimal + env-only
  (REQ-REFACTOR-081), quality gates (REQ-REFACTOR-082).
- **Open items resolved at Kickoff**: OR-REFACTOR-1 → **Opt A** (Opt B deferred → SPEC-COINDIR-001);
  OR-REFACTOR-2 → **literal-anchor** (both `SUPPORTED_INTERVALS` + `interval_to_seconds` fold into
  `ApiInterval`, full-vocab total `secs()` + `is_api_facing()` 400 guard). OR-REFACTOR-3
  (shared-batcher shape) is a run-phase implementation choice. No unresolved clarification markers remain.
- **Next gate**: plan-audit (plan-auditor) → Implementation Kickoff Approval already obtained → run-phase.

## §E.2 Run-phase Evidence

_<pending run-phase — owned by manager-develop>_

## §E.3 Run-phase Audit-Ready Signal

_<pending run-phase — owned by manager-develop>_

## §E.4 Sync-phase Audit-Ready Signal

_<pending sync-phase — owned by manager-docs>_
