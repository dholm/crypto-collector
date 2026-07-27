# SPEC-OBS-002 Progress

- Started: 2026-07-27
- Tier: M (3 artifacts: spec.md + plan.md + acceptance.md, + progress.md skeleton)
- Methodology: per quality.yaml (brownfield — characterize existing lifecycle/observability behavior first)
- Language: Rust / moai-lang-rust
- Branch strategy: commit to main (no feature branches), Route A (Hybrid Trunk main-direct)
- Scope: lifecycle, shutdown & observability integrity (F-38..F-49). No new endpoint, no migration,
  no new dependency.

## §E.1 Plan-phase Audit-Ready Signal

- plan_status: audit-ready
- plan_complete_at: 2026-07-27
- Artifacts: spec.md + plan.md + acceptance.md + progress.md (4 files; Tier M)
- REQ IDs allocated: REQ-OBS-060..074 (15 REQs) + REQ-ALARM-080..082 (3 REQs) = 18 total.
  - Metric integrity: REQ-OBS-060 (shared const SSOT), 061 (describe/emit parity), 062 (drop tracked_markets ghost).
  - Supervision/durability: REQ-OBS-063 (generic supervisor + capped backoff), 064 (relay initial-connect retry), 065 (single run_supervised).
  - Bounded shutdown: REQ-OBS-066 (timeout(drain_secs, supervisor) + always pool.close/telemetry.shutdown), 067 (broadcast-before-drain + 15s grace preserved), 068 (airtight shutdown arms).
  - Readiness: REQ-OBS-069 (bind-before-ready), 070 (flags-before-cache).
  - Config diagnostics: REQ-OBS-071 (warn on unparseable), 072 (fail-fast where dangerous).
  - Credential-safe connection: REQ-OBS-073 (PgConnectOptions from parts).
  - Dead-surface: REQ-OBS-074 (single HeaderExtractor/OtelMakeSpan; delete start_api_server).
  - Alarm: REQ-ALARM-080 (sustained all_providers_down), 081 (observe_chain_records doc/behavior per REQ-ALARM-020), 082 (reconciler MissedTickBehavior::Skip).
- AC IDs: AC-OBS-060..074 (1:1 with REQs) + AC-ALARM-080/081/082 + G1 (quality gates) + G2 (rename operator-visibility). Nearly all unit-testable (no live DB); AC-OBS-064 and AC-OBS-073 have optional DB-gated variants but a non-DB-gated primary path.
- LOCKED decisions restated D1..D10 (metric rename, supervised relays, PgConnectOptions, bounded shutdown, bind-before-ready + flags-before-cache, warn+fail-fast config, generic supervisor+backoff, airtight arms, alarm signal quality, dead-surface cleanup).
- Milestones (decision-reversibility order — highest-change-likelihood first): M1 metric SSOT + parity + ghost removal (F-38/F-49) → M2 alarm signal quality (F-42/F-48) → M3 bounded shutdown + airtight arms (F-41/F-47) → M4 exact readiness (F-43/F-44) → M5 config diagnostics (F-45) → M6 credential-safe connection (F-40) → M7 generic supervisor + relay supervision (F-39/F-46) → M8 dead-surface cleanup (F-49). Execution-dependency note: within M7, run_supervised lands before the folded relay supervision.
- Cross-SPEC reconciliations: REQ-OBS-062 removes the tracked_markets half of SPEC-OBS-001 REQ-OBS-013 (table dropped by migration 0011); REQ-OBS-060 makes emitters match REQ-OBS-015's already-specified names. New numbered SPEC (not an in-place amendment); parent SPECs unedited.
- Open items for run: OR-OBS2-1 (run_supervised backoff constants), OR-OBS2-2 (all_providers_down sustained-window duration), OR-OBS2-3 (REQ-ALARM-081 doc-vs-code authoritative + non-Network streak decision per REQ-ALARM-020), OR-OBS2-4 (live_poller.rs:665 tracked_markets emit-vs-describe), OR-OBS2-5 (OtelMakeSpan placement + canonical test set).
- @MX targets: metrics name consts @MX:ANCHOR + @MX:NOTE (operator-visible rename); run_supervised @MX:ANCHOR + @MX:WARN (backoff-reset window); main.rs shutdown sequence @MX:ANCHOR + @MX:WARN (ordering); health check_readiness @MX:WARN (flags-before-cache); config.rs @MX:NOTE (warn-vs-fail-fast split).

## §E.2 Run-phase Evidence

_<pending run-phase — owned by manager-develop>_

## §E.3 Run-phase Audit-Ready Signal

_<pending run-phase — owned by manager-develop>_

## §E.4 Sync-phase Audit-Ready Signal

_<pending sync-phase — owned by manager-docs>_

## §F Phase 4 Mode Selection

- Input parameters: tier=M; scope≈11 source files; domains=observability/lifecycle/config/alarm (single language: Rust); file language mix=100% Rust; concurrency benefit=LOW (coding-heavy, not research).
- Mode evaluation:
  - Mode 1 trivial — not selected (multi-file semantic change).
  - Mode 2 background — not selected (write work, must block).
  - Mode 3 agent-team — RETIRED, never selected.
  - Mode 4 parallel — not selected (coding-heavy, not multi-domain research; Anthropic coding-task parallelism caveat).
  - Mode 5 sub-agent — SELECTED (default for coding-heavy work; sequential per-milestone manager-develop).
  - Mode 6 workflow — not selected (not ≥30 files, not a single uniform mechanical transform; semantic brownfield fixes).
- Decision: sub-agent
- Justification: Coding-heavy brownfield defect/maintainability work across a single language with milestone dependencies (M7 run_supervised before folded relay supervision) is the canonical Mode 5 case; Anthropic's coding-task parallelism caveat directs sequential sub-agent over parallel/workflow fan-out. Implementation Kickoff Approval passed (autonomous progression) before this selection.
- Kickoff: Implementation Kickoff Approval = APPROVED (run now, autonomous), 2026-07-27.
