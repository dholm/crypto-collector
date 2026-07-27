# SPEC-OBS-002 — Implementation Plan

> Plan-phase artifact. WHAT/WHY lives in `spec.md`; this file records the approach,
> the LOCKED design decisions verbatim, milestones (decision-reversibility order),
> the file-touch map, and @MX planning. HOW-level detail (function signatures,
> exact backoff constants) is deferred to run-phase per the Open Items in spec.md.

## §A Context

- **Project**: crypto-collector — Rust microservice (tokio / axum 0.8 / sqlx 0.9 / PostgreSQL), deployed to the `finance` K8s namespace on aarch64.
- **Roadmap position**: Phase 6 of 7 (`research/idiomatic-rust.md` §7). Phases 1-5 complete or implemented-pending-db.
- **Branch strategy**: commit directly to `main` (no feature branches — project convention; Route A Hybrid Trunk main-direct, Tier M).
- **Methodology**: per `quality.yaml` (brownfield — TDD/DDD; characterize existing lifecycle behavior first, then fix). Most ACs are unit-testable without a live DB.
- **Tier**: **M (standard)** — confirmed. Rationale: ~11 source files across observability/lifecycle/config/alarm domains; behavioral-defect + maintainability; follows established patterns (`migrate_with_retry` backoff, `sustained_*` alarm helpers, `HealthState::for_test`) with no architecture redesign. Not Tier S (>5 files, cross-cutting lifecycle behavior); not Tier L (<15 files, no new subsystem, no constitutional change).
- **SPEC artifacts**: `.moai/specs/SPEC-OBS-002/{spec,plan,acceptance,progress}.md` (4 files, Tier M).

## §B Known Issues (run-phase auto-injection, filtered for this SPEC)

Rust project — the Go-specific B1 (cross-platform build tags), B3/B4/B6/B7 (Go harness paths, spec-lint headings) do not apply. Relevant categories:

- **B9 — Commit + push directly to `main`** (Hybrid Trunk 1-person OSS). Conventional Commits (`feat(SPEC-OBS-002): M{N} <subject>` / `fix(...)`). Never `--no-verify`. Per-milestone commits allowed.
- **B10 — Scope discipline (PRESERVE)**. Touch ONLY the § File-touch Map paths below. Do NOT touch other SPEC dirs, runtime-managed files (`.moai/state/*`, `.moai/harness/*`), or unrelated source.
- **B11 — Subagent boundary**. No AskUserQuestion in run-phase; on a blocker (esp. the Open Items), return a structured blocker report.
- **Operator-visibility (B-custom)**: the metric rename (D1) is operator-visible — the rename MUST be stated in the commit body and the observability docs so external Grafana dashboards/alerts keyed on the old `coin_*` series migrate in lockstep.
- **Quality-gate baseline**: run `cargo clippy --all-targets --all-features -- -D warnings` + `cargo fmt --check` BEFORE starting to establish the pre-existing baseline (distinguish NEW lint from baseline).

## §C Pre-flight (run before any change)

```bash
git branch --show-current              # expect: main
git rev-parse HEAD                     # baseline SHA
cargo check --all-targets --all-features   # fast type baseline
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -5   # lint baseline
cargo fmt --check                      # format baseline
# Confirm the described-vs-emitted metric drift + tracked_markets ghost before touching:
grep -n 'quote_insert_duration_seconds\|candle_insert_duration_seconds\|coin_quote_insert\|coin_candle_insert\|tracked_markets' src/metrics/mod.rs src/db/upserts.rs src/collectors/live_poller.rs
```

## §D Design Decisions (LOCKED — verbatim)

These are settled inputs from the orchestrator. Do NOT emit `[NEEDS CLARIFICATION]` for them.

1. **F-38 metric-name single source of truth — DECISION: rename emitters to the spec names.** Introduce shared `pub const` metric-name identifiers in `src/metrics/mod.rs`, referenced by BOTH `describe_all()` AND every emitter (`src/db/upserts.rs:78,143`). Canonical names are the REQ-OBS-015 / module-catalogue names: `quote_insert_duration_seconds` and `candle_insert_duration_seconds` (the current `coin_`-prefixed emitted names are renamed away). This is **operator-visible**: the rename MUST be stated in the commit body and SPEC notes so external Grafana dashboards/alerts keyed on the old `coin_*` series are updated in lockstep. Unit tests MUST emit THROUGH the shared consts so describe/emit drift is structurally impossible; add an explicit describe/emit parity unit test. Remove the `tracked_markets` describe + module-catalogue row + test (`metrics/mod.rs:84,382-396`; table dropped by migration `0011_remove_markets.sql`). NOTE for run-phase: an emitter reference to `tracked_markets` also appears at `src/collectors/live_poller.rs:665` — run-phase must verify whether it is a live emit or describe-only and remove/reconcile it consistently with dropping the ghost gauge.

2. **F-39 supervised relays — DECISION: supervise via the generic supervisor.** The two PG LISTEN relay tasks (`src/main.rs:392-403`, bare `tokio::spawn`) run under the same generic supervision/restart pattern as the workers (folds into F-46's `run_supervised`). Fix the `src/listener.rs:54` doc to match the implemented retry behavior. Initial-connect/listen failure must retry with capped backoff rather than permanently return (`listener.rs:63-73`).

3. **F-40 credential-safe DB connection — DECISION: PgConnectOptions from parts.** Replace `build_database_url` string assembly (`src/config.rs:44-55`) with `sqlx::postgres::PgConnectOptions` built from parts (host/port/username/password/database) — no new dependency; the password never enters a formatted string. Preserve the `DATABASE_URL` local-dev override path.

4. **F-41 bounded shutdown — DECISION: timeout(drain_secs, supervisor).** Replace the unconditional `drain_secs` sleep + unbounded `supervisor.await` (`src/main.rs:455-458`) with `tokio::time::timeout(drain_secs, supervisor)` so shutdown completes as soon as workers finish and is bounded above. `pool.close()` and `telemetry::shutdown()` MUST always run afterward. KEEP the 15 s endpoint-removal grace sleep where it is (upstream of the broadcast, load-bearing). Workers MUST still receive the shutdown broadcast BEFORE the drain wait begins (zero-drop rollout preserved).

5. **F-43/F-44 exact readiness — DECISION: bind before ready; flags before cache.** The API `TcpListener::bind` (and relay spawn) happen BEFORE `set_ready()` (only `axum::serve` follows). `check_readiness` consults the `shutting_down`/`ready` atomics BEFORE the 2 s cache fast-path (`src/health/mod.rs:90-120`); cache only the DB-ping result. Constraint: F-43 reorders ONLY within Step 10-11; do NOT change the documented health-before-DB-retry / readiness-503-before-grace startup ordering guarantees.

6. **F-45 config diagnostics — DECISION: warn + selective fail-fast.** Present-but-unparseable env values (`parse_env_*`, `src/config.rs:566-599`, and `DEEP_BACKFILL_START_DATE`) MUST at minimum emit a `tracing::warn!` naming the variable and the fallback used. Fail-fast for values where a silent default is dangerous (pacer cooldowns). F-56 `Config::from_env()` snapshot is OUT OF SCOPE (follow-up) — do NOT include it.

7. **F-46 generic supervisor + backoff — DECISION.** One `run_supervised(name, registry, shutdown, make_future)` replaces the four copy-pasted supervisors (`src/collectors/mod.rs:207-413`). Restart uses capped exponential backoff (constants-pattern per `src/db/pool.rs:22-23`, as `migrate_with_retry` already demonstrates) that resets after a healthy-run period. Log severity consistent between the panic arm and the error arm.

8. **F-47 airtight shutdown arms — DECISION.** Every `shutdown_rx.changed()` select arm (listener `listener.rs:80-85`, worker loops `live_poller.rs:232-244`, reconciler `reconciler.rs:604-620`) breaks on `Err` (dropped sender) as well as on `*borrow()`.

9. **F-42/F-48 alarm signal quality — DECISION.** `all_providers_down` becomes a sustained, timestamp-based signal (`last_all_failed_at` / `last_chain_success_at` gated on a window, reusing the existing `sustained_*` helpers in `alarm/registry.rs`) so the Critical alarm reflects a sustained condition, not a sampled last-outcome flag (`registry.rs:96-117`, `reconciler.rs:78-81`). Fix `observe_chain_records` so its doc matches behavior (`registry.rs:125-149`) — resolve per REQ-ALARM-020 (state which of doc/code is authoritative). Reconciler ticker uses `MissedTickBehavior::Skip` (`reconciler.rs:605`, currently default Burst; live_poller already uses Skip).

10. **F-49 dead-surface cleanup — DECISION.** Delete the duplicated private `HeaderExtractor` in `src/main.rs:40-49` and use `telemetry::HeaderExtractor`; move `OtelMakeSpan` into `src/telemetry/` (dedup tests in both). Delete the dead `start_api_server` (`src/api/mod.rs:8-13,233`) — main builds the router itself; deleting the dead surface is the chosen resolution (not promoting it to the entry point). Drop the `tracked_markets` ghost (see decision 1).

## §E Milestones (decision-reversibility order — highest-change-likelihood first)

Ordered so human review focuses on the operator-visible / interface-defining / new-state decisions first; mechanical refactors are deferred to the bottom.

| M | Theme | Findings | Reqs | Nature |
|---|-------|----------|------|--------|
| M1 | Metric-name SSOT + describe/emit parity + `tracked_markets` ghost removal | F-38, F-49(metrics) | REQ-OBS-060/061/062 | operator-visible rename + new shared-const interface |
| M2 | Alarm signal quality — sustained `all_providers_down`, `observe_chain_records` doc/behavior, reconciler `Skip` | F-42, F-48 | REQ-ALARM-080/081/082 | operator-visible alarm semantics + new timestamp state |
| M3 | Bounded shutdown drain + airtight shutdown arms | F-41, F-47 | REQ-OBS-066/067/068 | deploy-visible lifecycle behavior |
| M4 | Exact readiness — bind-before-ready + flags-before-cache | F-43, F-44 | REQ-OBS-069/070 | readiness behavior contract (constrained reorder) |
| M5 | Config diagnostics — warn + selective fail-fast | F-45 | REQ-OBS-071/072 | operator-visible diagnostics |
| M6 | Credential-safe connection — `PgConnectOptions` from parts | F-40 | REQ-OBS-073 | connection-construction interface change |
| M7 | Generic supervisor + capped backoff + relay supervision | F-39, F-46 | REQ-OBS-063/064/065 | mechanical consolidation + restart behavior |
| M8 | Dead-surface cleanup — `HeaderExtractor`/`OtelMakeSpan` dedup, delete `start_api_server` | F-49 | REQ-OBS-074 | mechanical |

**Execution-dependency note** (the milestone list is review-priority order, NOT a strict execution DAG): within **M7**, the generic `run_supervised` (F-46) MUST land before the folded relay supervision (F-39 D2). M3's airtight arms (F-47) touch the same shutdown-select sites the M7 supervisor wraps — run-phase may sequence M7's `run_supervised` extraction before finalizing M3's arm edits, or keep the arm-break edits localized to the loop bodies; either is acceptable as long as no busy-spin regression is introduced. All other milestones are mutually independent and may be committed in the listed order.

## §F File-touch Map (PRESERVE = everything else)

| File | Milestones | Change |
|------|-----------|--------|
| `src/metrics/mod.rs` | M1 | shared name consts; describe uses consts; drop `tracked_markets` describe/catalogue/test; parity test |
| `src/db/upserts.rs` | M1 | emitters reference shared consts (rename `coin_*` → spec names) |
| `src/collectors/live_poller.rs` | M1, M3, M7 | reconcile `:665` `tracked_markets` ref; airtight shutdown arm; runs under `run_supervised` |
| `src/alarm/registry.rs` | M2 | `all_providers_down` sustained timestamp signal; `observe_chain_records` doc/behavior |
| `src/alarm/reconciler.rs` | M2, M3 | sustained-signal consumption; ticker `MissedTickBehavior::Skip`; airtight shutdown arm |
| `src/providers/mod.rs` | M2 | `provider-unreachable` streak resolution per REQ-ALARM-020 (per OR-OBS2-3) |
| `src/main.rs` | M3, M4, M8 | bounded `timeout(drain_secs, supervisor)` + ordering; bind-before-`set_ready()`; delete private `HeaderExtractor`; relay spawn under supervisor |
| `src/health/mod.rs` | M4 | flags-before-cache in `check_readiness`; cache only DB-ping |
| `src/config.rs` | M5, M6 | warn/fail-fast in `parse_env_*` + `DEEP_BACKFILL_START_DATE`; `PgConnectOptions` from parts (replace `build_database_url`) |
| `src/collectors/mod.rs` | M7 | generic `run_supervised` replacing the 4 copy-pasted supervisors + capped backoff |
| `src/listener.rs` | M7, M3 | initial-connect capped-backoff retry; supervised; fix `:54` doc; airtight arm |
| `src/telemetry/mod.rs` | M8 | canonical `HeaderExtractor`; house `OtelMakeSpan` |
| `src/api/mod.rs` | M8 | delete dead `start_api_server` |

Docs touched in-change (not after — per sync-phase note): `src/metrics/mod.rs` module-header catalogue (canonical names + rename note), `src/listener.rs` doc comment, `src/alarm/registry.rs` `observe_chain_records` doc comment, observability docs (canonical metric names + `coin_*` rename).

## §G Anti-Patterns (avoid)

- Silently changing metric names without the operator-visible rename note (D1) — external Grafana breaks with no trail.
- Leaving `pool.close()` / `telemetry::shutdown()` inside the timed-out branch only (D4) — they MUST run on both the completed and timed-out paths.
- Moving the 15 s endpoint-removal grace sleep or reordering it after the broadcast (D4) — it is load-bearing upstream of the broadcast.
- Changing the documented health-before-DB-retry / readiness-503-before-grace startup ordering (D5) — F-43 reorders ONLY within Step 10-11.
- Introducing a new dependency for `PgConnectOptions`, the backoff, or the sustained alarm signal (D3/D7/D9) — all reuse existing APIs/helpers.
- Implementing the F-56 `Config::from_env()` snapshot (D6 says OUT OF SCOPE).
- Promoting `start_api_server` to the entry point instead of deleting it (D10 chose deletion).
- Guessing the sustained-window duration, backoff constants, or the REQ-ALARM-081 authoritative source — these are Open Items (return a blocker if under-specified).

## §H @MX Planning

- **@MX:ANCHOR** targets: (1) the shared metric-name consts in `src/metrics/mod.rs` (every cross-SPEC emitter binds to them); (2) the generic `run_supervised` in `src/collectors/mod.rs` (all workers + relays route through it); (3) the bounded-shutdown sequence in `src/main.rs` (grace → broadcast → `timeout` → `pool.close()` → `telemetry::shutdown()`).
- **@MX:WARN** targets (shutdown ordering / concurrency): the shutdown sequence in `src/main.rs` (ordering is load-bearing for zero-drop rollout); the `run_supervised` backoff-reset window in `src/collectors/mod.rs`; the flags-before-cache ordering in `src/health/mod.rs` (`check_readiness`). Each WARN carries a mandatory `@MX:REASON`.
- **@MX:NOTE** targets: the F-38 operator-visible rename rationale on the metric-name consts / catalogue header (external Grafana series migration); the warn-vs-fail-fast policy split in `src/config.rs`.

## §I Cross-References

- `spec.md` — requirements (REQ-OBS-060..074, REQ-ALARM-080..082) + Cross-SPEC Reconciliation.
- `acceptance.md` — per-AC testable criteria + Given/When/Then scenarios + Definition of Done.
- `research/idiomatic-rust.md` §6 Category F (F-38..F-49), §7 roadmap row 6.
- Parent SPECs: SPEC-OBS-001 (REQ-OBS-004/013/015/030-033/040-041), SPEC-API-002 (REQ-API-148), SPEC-ALARM-001 (REQ-ALARM-020).
- Reuse precedents: `src/db/pool.rs:22-23` + `migrate_with_retry` (capped backoff), `src/alarm/registry.rs` `sustained_*` helpers, `HealthState::for_test` (readiness unit tests).
