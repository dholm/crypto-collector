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

- **Status**: `implemented` (up from `draft`) — sync-phase artifacts authored by manager-docs
  (2026-07-27). `completed` is HELD pending a live-Postgres DB-gated `--test-threads=1` run
  (per the SPEC-PROV-002/003 / SPEC-API-005 / SPEC-OBS-002 close pattern — no live Postgres
  instance is available in this environment).
- **Milestone commits** (all pushed to `main`, direct-to-main Hybrid Trunk, no PR):
  - `572962d` — plan-phase artifacts (spec/plan/acceptance/design/research)
  - `7053241` — M1 provider trait capability-derived defaults (F-50)
  - `5449edf` — M2 chain_try + per-provider fallback pacing (F-53a, F-16)
  - `9d71e74` — M3 shared lease-queue scaffold (F-53b)
  - `721d677` — M4 batched candle writes + NOTIFY policy (F-51, F-52)
  - `26bd9c1` — M5 API dedup (F-53c, F-55)
  - `f5d111f` — M6 domain typing — ApiInterval fold + keyed query + F-56 (F-54)
- **Quality gates (this sync session, re-verified independently of run-phase claims)**:
  - `cargo fmt --check` → exit 0
  - `cargo clippy --all-targets --all-features -- -D warnings` → exit 0
  - `cargo test` → exit 0; **748 non-DB-gated tests pass** (699 lib + 9 alarm_docs_parity + 5
    backtest_projection + 2 db_integration-non-ignored + 21 migration_files + 12 model_serde),
    **100 DB-gated tests remain `#[ignore]`d** (84 lib-embedded DB tests + 16
    `tests/db_integration.rs` scenarios) — none run in this environment (no live Postgres).
  - `grep -c NotSupported src/providers/{coinbase,kraken}.rs` → both `0` (AC-REFACTOR-012a)
  - `grep -rn 'SUPPORTED_INTERVALS\|fn interval_to_seconds'` → no matches (AC-REFACTOR-061a)
  - `grep -rn 'fn ensure_coin_exists' src/api/` → exactly one definition (AC-REFACTOR-050a)
- **@MX validation**: all targets named in spec.md `@MX Annotation Targets` confirmed present and
  correctly placed — `chain_try` `@MX:ANCHOR` (src/providers/mod.rs:753) carries the moved pacer
  `@MX:WARN` (line 759, REQ-REFACTOR-021); `run_lease_worker` `@MX:ANCHOR`
  (src/collectors/lease_worker.rs:119) + `spawn_heartbeat` `@MX:NOTE` (watch-based stop,
  REQ-REFACTOR-031); `batched_upsert_coin_candles` `@MX:ANCHOR` (src/db/upserts.rs:255, notes the
  D1 two-policy split + F-51 NOTIFY policy); `ApiInterval::secs()` `@MX:ANCHOR`
  (src/models/interval.rs:102, total/no-`.expect`); `paginate` `@MX:NOTE`
  (src/api/cursor.rs:115). No dangling or duplicate anchors found; no missing anchors on any
  named shared helper.
- **CHANGELOG**: `[Unreleased] § Changed` entry added (`CHANGELOG.md`), summarizing all 6
  milestones and both intended behavior changes (a: per-provider chain-fallback pacing; b: no
  NOTIFY on backfill writes) plus the operator-relevant per-page-commit / dropped-histogram
  consequence. Pre-emission `grep -c 'SPEC-REFACTOR-001' CHANGELOG.md` returned `0` before
  emission (no duplicate-entry risk from a parallel session).
- **Module docs**: provider/collector/db/api module-level rustdoc already reflects the new shared
  helpers via the @MX annotations verified above (chain_try, lease-queue scaffold, batched
  upsert, ApiInterval, paginate) — no additional narrative module-doc edit was required beyond
  the @MX blocks already landed in the M1-M6 commits.
- **Status transition applied**: `spec.md` frontmatter `status: draft → implemented`
  (`updated: 2026-07-27` unchanged — same day). `completed` intentionally NOT set.
- **Scope discipline confirmed**: spec.md / plan.md / acceptance.md BODY content untouched this
  session (frontmatter `status:` only); no git push performed by this agent (staged for the
  orchestrator's independent verification + push per author-email discipline).
