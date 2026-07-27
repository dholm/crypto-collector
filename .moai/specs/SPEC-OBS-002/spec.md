---
id: SPEC-OBS-002
title: "Lifecycle, Shutdown & Observability Integrity"
version: "0.1.0"
status: completed
created: 2026-07-27
updated: 2026-07-27
author: dholm
priority: High
phase: "improvement-phase-6"
module: "src (lifecycle: main.rs, collectors, metrics, config.rs, health, listener.rs, alarm, telemetry)"
lifecycle: spec-anchored
tags: "observability, lifecycle, shutdown, supervision, metrics, config, alarm"
tier: M
---

# SPEC-OBS-002 — Lifecycle, Shutdown & Observability Integrity

Phase 6 of the crypto-collector review-driven improvement roadmap
(`research/idiomatic-rust.md` §7, Phase 6 — "Lifecycle, shutdown & observability
integrity"). Prior phases: SPEC-SCHED-002, SPEC-CANDLE-002, SPEC-PROV-003,
SPEC-API-005 (all complete or implemented-pending-db). This SPEC closes the
**process-lifecycle and observability** findings — metric-name drift, unsupervised
relays, an unbounded/fixed shutdown drain, a flappy Critical alarm, a readiness
window, silent config fallbacks, credential-unsafe DB URL assembly, supervisor
duplication, and dead observability surface.

Findings addressed (Category F of `research/idiomatic-rust.md` §6): **F-38, F-39,
F-40, F-41, F-42, F-43, F-44, F-45, F-46, F-47, F-48, F-49**. This is a brownfield
SPEC that is **behavioral-defect + maintainability** in nature over the existing
`main.rs`/lifecycle/observability layer. It adds no new endpoint, no migration, and
no new dependency.

Behavioral base: [SPEC-OBS-001](../SPEC-OBS-001/spec.md) owns the observability +
lifecycle contract this SPEC hardens — the metric catalogue (REQ-OBS-010..015), the
readiness gate (REQ-OBS-003/004/040/041), and the graceful-shutdown ordering
(REQ-OBS-030..033). Cross-replica WebSocket delivery is [SPEC-API-002](../SPEC-API-002/spec.md)
REQ-API-148. Alarm-signal behavior is [SPEC-ALARM-001](../SPEC-ALARM-001/spec.md)
(REQ-ALARM-020, provider-unreachable signal). This SPEC allocates a fresh
**REQ-OBS-060..074** block and **REQ-ALARM-080..082** block; the parent SPECs remain
authoritative and are referenced (with two noted cross-SPEC reconciliations — see
§ Cross-SPEC Reconciliation).

## HISTORY

| Version | Date | Change |
|---------|------|--------|
| 0.1.0 | 2026-07-27 | Initial plan-phase authoring (Tier M). Findings F-38..F-49. REQ-OBS-060..074 + REQ-ALARM-080..082. |

## Goal

Make the process lifecycle and its observability **honest and bounded**:

1. **Honest metrics** — every described metric has a real emitter and vice versa, via a shared name const; no ghost gauges.
2. **Durable background tasks** — PG LISTEN relays and workers run under one generic supervisor with capped-exponential backoff; a transient failure or panic never permanently disables cross-replica delivery.
3. **Bounded, ordered shutdown** — the drain completes as soon as workers finish and is bounded above; a wedged worker cannot block `pool.close()`/trace flush past `drain_secs`; zero-drop rollout preserved.
4. **Exact readiness** — Ready is never reported before the API listener binds; a 503 is served immediately on shutdown even with a warm readiness cache.
5. **Diagnosable config** — a present-but-unparseable env override warns (and fails fast where a silent default is dangerous) rather than silently reverting.
6. **Credential-safe connection** — the DB password never enters a formatted URL string.
7. **No dead surface** — one `HeaderExtractor`/`OtelMakeSpan`, no dead `start_api_server`, no ghost gauge.
8. **Trustworthy Critical alarm** — `all_providers_down` reflects a sustained condition, not a sampled last-outcome flag.

## Scope

In scope (behavioral + maintainability, all within the existing lifecycle/observability layer):

- Metric-name single source of truth + describe/emit parity (F-38); ghost-gauge removal (F-49).
- Generic supervision with capped-exponential backoff for workers AND relays (F-39, F-46).
- Bounded shutdown drain + airtight shutdown-select arms (F-41, F-47).
- Exact readiness ordering (bind-before-ready) + flags-before-cache (F-43, F-44).
- Config diagnostics: warn on unparseable, fail-fast where dangerous (F-45).
- Credential-safe DB connection via `PgConnectOptions` (F-40).
- Dead-surface cleanup: `HeaderExtractor`/`OtelMakeSpan` dedup, dead `start_api_server` deletion (F-49).
- Alarm signal quality: sustained `all_providers_down`, doc/behavior reconciliation, reconciler ticker `Skip` (F-42, F-48).

Out of scope: see § Exclusions.

## Decisions Restated (authoritative — LOCKED)

These decisions are settled inputs from the orchestrator. They are NOT open questions;
run-phase MUST implement them as stated. Full verbatim wording and rationale live in
`plan.md § Design Decisions (LOCKED)` — this section names them for requirement traceability.

- **D1 (F-38) — Metric-name SSOT: rename emitters to the spec names.** Shared `pub const` metric-name identifiers in `src/metrics/mod.rs` referenced by BOTH `describe_all()` AND every emitter. Canonical names `quote_insert_duration_seconds` / `candle_insert_duration_seconds` (the `coin_`-prefixed emitted names are renamed away). **Operator-visible** — the rename is stated in the commit body and SPEC notes so external Grafana dashboards/alerts keyed on the old `coin_*` series are updated in lockstep.
- **D2 (F-39) — Supervised relays via the generic supervisor.** The two PG LISTEN relay tasks fold into F-46's `run_supervised`; initial-connect/listen failure retries with capped backoff; the `listener.rs:54` doc is fixed to match.
- **D3 (F-40) — `PgConnectOptions` from parts.** Replace `build_database_url` string assembly with `sqlx::postgres::PgConnectOptions` built from host/port/username/password/database. No new dependency. `DATABASE_URL` local-dev override preserved.
- **D4 (F-41) — `timeout(drain_secs, supervisor)`.** Bound the drain above; `pool.close()` + `telemetry::shutdown()` always run afterward. Keep the 15 s endpoint-removal grace sleep upstream of the broadcast. Workers receive the shutdown broadcast BEFORE the drain wait begins.
- **D5 (F-43/F-44) — Bind before ready; flags before cache.** `TcpListener::bind` (and relay spawn) happen before `set_ready()`; `check_readiness` consults `shutting_down`/`ready` atomics before the 2 s cache fast-path; only the DB-ping result is cached. F-43 reorders ONLY within Step 10-11; documented health-before-DB-retry / readiness-503-before-grace ordering guarantees are unchanged.
- **D6 (F-45) — Warn + selective fail-fast.** Present-but-unparseable env values emit a `tracing::warn!` naming the variable + fallback; fail-fast where a silent default is dangerous (pacer cooldowns). F-56 `Config::from_env()` snapshot is OUT OF SCOPE (follow-up).
- **D7 (F-46) — Generic supervisor + backoff.** One `run_supervised(name, registry, shutdown, make_future)` replaces the four copy-pasted supervisors. Capped exponential backoff (constants pattern per `src/db/pool.rs:22-23`; `migrate_with_retry` precedent) that resets after a healthy-run period. Log severity consistent between panic and error arms.
- **D8 (F-47) — Airtight shutdown arms.** Every `shutdown_rx.changed()` select arm breaks on `Err` (dropped sender) as well as on `*borrow()`.
- **D9 (F-42/F-48) — Alarm signal quality.** `all_providers_down` becomes a sustained, timestamp-based signal (`last_all_failed_at` / `last_chain_success_at` gated on a window, reusing `sustained_*` helpers). `observe_chain_records` doc reconciled with behavior per REQ-ALARM-020. Reconciler ticker uses `MissedTickBehavior::Skip`.
- **D10 (F-49) — Dead-surface cleanup.** Delete the duplicated private `HeaderExtractor` in `main.rs`; use `telemetry::HeaderExtractor`; move `OtelMakeSpan` into `src/telemetry/`. Delete dead `start_api_server`. Drop the `tracked_markets` ghost (see D1/F-49).

## Change Surface (brownfield delta markers)

| Finding | Requirement(s) | Site (indicative — run-phase verifies) |
|---------|----------------|-----------------------------------------|
| F-38 | REQ-OBS-060, 061 | `src/metrics/mod.rs:73-81` (describe), `src/db/upserts.rs:78,143` (emit) |
| F-49 (metrics) | REQ-OBS-062 | `src/metrics/mod.rs:84,382-396`; `src/collectors/live_poller.rs:665` (verify emit vs describe-only) |
| F-39 | REQ-OBS-063, 064 | `src/listener.rs:54,63-73,80-85`; `src/main.rs:392-403` |
| F-46 | REQ-OBS-063, 065 | `src/collectors/mod.rs:207-413`; backoff pattern `src/db/pool.rs:22-23` |
| F-41 | REQ-OBS-066, 067 | `src/main.rs:455-458` |
| F-47 | REQ-OBS-068 | `src/listener.rs:80-85`; `src/collectors/live_poller.rs:232-244`; `src/alarm/reconciler.rs:604-620` |
| F-43 | REQ-OBS-069 | `src/main.rs:372,424-426` |
| F-44 | REQ-OBS-070 | `src/health/mod.rs:90-120` |
| F-45 | REQ-OBS-071, 072 | `src/config.rs:566-599` + `DEEP_BACKFILL_START_DATE` |
| F-40 | REQ-OBS-073 | `src/config.rs:44-55` |
| F-49 (surface) | REQ-OBS-074 | `src/main.rs:40-49`; `src/telemetry/mod.rs:110-120`; `src/api/mod.rs:8-13,233` |
| F-42 | REQ-ALARM-080 | `src/alarm/registry.rs:96-117`; `src/alarm/reconciler.rs:78-81` |
| F-48 | REQ-ALARM-081 | `src/alarm/registry.rs:125-149`; `src/providers/mod.rs:434-439` |
| F-42/F-48 | REQ-ALARM-082 | `src/alarm/reconciler.rs:605` (ticker `MissedTickBehavior`) |

## Requirements (GEARS)

### Metric-name integrity (F-38, F-49)

- **REQ-OBS-060** (Ubiquitous): The metrics module shall define the persistence-latency
  metric names as shared `pub const` identifiers referenced by BOTH `describe_all()` and
  every emitter, with canonical names `quote_insert_duration_seconds` and
  `candle_insert_duration_seconds` (the `coin_`-prefixed emitted names are renamed away),
  consistent with REQ-OBS-015.
- **REQ-OBS-061** (Ubiquitous): The metrics module shall guarantee describe/emit parity —
  every described metric name has a real emitter reference via the shared const, and no
  emitter records a metric name that is not described.
- **REQ-OBS-062** (Unwanted): The system shall not describe, test, or emit the
  `tracked_markets` gauge (the backing table was dropped by migration `0011_remove_markets.sql`);
  the describe entry, module-catalogue row, unit test, and any live emitter reference shall
  be removed. (Reconciles the now-invalid `tracked_markets` portion of REQ-OBS-013 — see
  § Cross-SPEC Reconciliation.)

### Supervision & durability (F-39, F-46)

- **REQ-OBS-063** (Event-driven): When a supervised task (a background worker or a PG LISTEN
  relay) exits or panics, the system shall restart it under a single generic supervisor using
  capped exponential backoff that resets after a healthy-run period, rather than a fixed-delay
  restart with no backoff.
- **REQ-OBS-064** (Event-driven): When a PG LISTEN relay fails its initial connect or `listen()`,
  the system shall retry with capped backoff rather than permanently returning, so a transient
  DB hiccup at spawn does not permanently disable cross-replica WebSocket delivery (REQ-API-148).
- **REQ-OBS-065** (Ubiquitous): The system shall implement supervision through one
  `run_supervised(name, registry, shutdown, make_future)` function (replacing the four
  copy-pasted supervisors), with log severity consistent between the panic arm and the error arm.

### Bounded, ordered shutdown (F-41, F-47)

- **REQ-OBS-066** (Event-driven): When the servers exit during shutdown, the system shall bound
  the drain wait via `tokio::time::timeout(drain_secs, supervisor)` — completing as soon as the
  workers finish and bounded above by `drain_secs` — and shall always run `pool.close()` and
  `telemetry::shutdown()` afterward regardless of whether the drain completed or timed out.
- **REQ-OBS-067** (Ubiquitous): The system shall deliver the shutdown broadcast to workers before
  the drain wait begins, and shall preserve the 15 s endpoint-removal grace sleep upstream of the
  broadcast, so the zero-drop rollout guarantee (REQ-OBS-030..033) is unchanged.
- **REQ-OBS-068** (Event-driven): When the shutdown watch sender is dropped (`changed()` returns
  `Err`), every shutdown-select arm (listener, worker loops, reconciler) shall break rather than
  busy-spin on the immediate error.

### Exact readiness (F-43, F-44)

- **REQ-OBS-069** (Event-driven): When the process starts, the API `TcpListener::bind` and the
  relay spawn shall complete before `set_ready()`; only `axum::serve` follows, so readiness never
  reports Ready while the listener is not yet accepting connections (REQ-OBS-040). This reorders
  only within the existing Step 10-11; the documented startup ordering guarantees are unchanged.
- **REQ-OBS-070** (Event-driven): When `set_shutting_down()` is called, `check_readiness` shall
  consult the `shutting_down`/`ready` atomics before the 2 s readiness cache fast-path, caching
  only the DB-ping result, so the 503-on-shutdown guarantee (REQ-OBS-004) cannot lag behind a warm
  cache.

### Config diagnostics (F-45)

- **REQ-OBS-071** (Event-driven): When an env value is present but unparseable, the config layer
  shall emit a `tracing::warn!` naming the variable and the fallback value used, rather than
  silently reverting to the default.
- **REQ-OBS-072** (Event-driven): When a present-but-unparseable env value would otherwise
  silently default to a value that is dangerous to mis-set (pacer cooldowns), the system shall
  fail fast with a clear error rather than warn-and-continue.

### Credential-safe connection (F-40)

- **REQ-OBS-073** (Ubiquitous): The system shall build the database connection from
  `sqlx::postgres::PgConnectOptions` assembled from parts (host, port, username, password,
  database) rather than formatting credentials into a `postgres://…` URL string, so a password
  containing URL-significant characters connects correctly and the password never enters a
  loggable formatted string. The `DATABASE_URL` local-dev override path is preserved. No new
  dependency is introduced.

### Dead-surface cleanup (F-49)

- **REQ-OBS-074** (Ubiquitous): The system shall expose a single `HeaderExtractor` and a single
  `OtelMakeSpan` located in `src/telemetry/` (the duplicated private copy in `main.rs` is removed),
  and shall not carry a dead `start_api_server` entry point (deleted — `main` builds the router itself).

### Alarm signal quality (F-42, F-48)

- **REQ-ALARM-080** (State-driven): While the whole provider chain has failed continuously for a
  sustained window, the system shall raise the Critical `all_providers_down` alarm based on
  timestamped signals (`last_all_failed_at` / `last_chain_success_at`) gated on that window and
  reusing the existing `sustained_*` helpers — not on a sampled last-outcome flag; a single coin's
  failure among many successes shall not flip it, and a lone success mid-outage shall not suppress it.
- **REQ-ALARM-081** (Ubiquitous): The `observe_chain_records` documentation shall match its
  implementation; the doc/code drift shall be resolved per REQ-ALARM-020, explicitly stating which
  of the doc or code is authoritative and whether the per-provider `provider-unreachable` streak
  should count non-`Network` failures (repeated 5xx).
- **REQ-ALARM-082** (Ubiquitous): The reconciler ticker shall use `MissedTickBehavior::Skip`
  (consistent with `live_poller`) rather than the default `Burst`.

## Cross-SPEC Reconciliation

This SPEC is a NEW numbered SPEC (a roadmap phase adding orthogonal lifecycle/observability
hardening), NOT an in-place amendment of SPEC-OBS-001 or SPEC-ALARM-001. Two parent requirements
are partially reconciled here; the parent SPECs are not edited by this SPEC:

- **REQ-OBS-013** (SPEC-OBS-001) lists both `tracked_coins` and `tracked_markets` gauges.
  REQ-OBS-062 removes the `tracked_markets` half because migration `0011_remove_markets.sql` dropped
  the backing table. The `tracked_coins` gauge is unchanged.
- **REQ-OBS-015** (SPEC-OBS-001) already names `quote_insert_duration_seconds` /
  `candle_insert_duration_seconds` as the persistence-latency metrics. REQ-OBS-060 makes the
  emitters match those already-specified names (the drift was an implementation defect, not a spec change).
- **Parent-status caveat**: SPEC-OBS-001 is **stale-but-implemented** — its frontmatter `status` reads
  `planned`, yet its REQ-OBS-010..015 / REQ-OBS-030..041 are live, deployed, and (as F-38..F-49 show)
  defective, which is precisely why this SPEC hardens them. This SPEC therefore does NOT declare
  `depends_on: [SPEC-OBS-001]` — the parent is not `completed`, so a blocking dependency edge would
  falsely gate run-phase entry on a SPEC that is already in production. The parent SPEC file is left unedited.

## Exclusions (What NOT to Build)

The following are explicitly out of scope for this SPEC.

### Out of Scope — other roadmap phases
- Phase 7 batching & structural debt (F-50..F-56, F-16) — a separate SPEC. This SPEC touches lifecycle/observability only.
- Findings from Categories A-E and G-H not listed in § Scope.

### Out of Scope — config snapshot refactor
- F-56 `Config::from_env()` validated snapshot (the config bag → single validated struct refactor) is a follow-up, per D6. This SPEC adds only warn/fail-fast diagnostics to the existing per-call env readers, not a snapshot type.

### Out of Scope — new dependencies
- No new crate is added. `PgConnectOptions` (F-40) uses `sqlx`'s existing API; the supervisor/backoff (F-46) reuses the existing constants pattern; the alarm signal (F-42) reuses existing `sustained_*` helpers.

### Out of Scope — F-49 informational nits not in the LOCKED decisions
- Metrics coverage asymmetry (live-poller/backfill provider calls uninstrumented) — not addressed.
- `timeout_seconds: u64` vs alarm-center int32 clamp, `AlarmClient` retry backoff — not addressed.
- `Dockerfile` not copying `build.rs`; both Dockerfiles copying compile-time-embedded `migrations/`; `Makefile` `upgrade` missing from `.PHONY`; the fixed mutable `:aarch64` deploy tag / `imagePullPolicy` — deployment concerns, out of this SPEC's lifecycle/observability scope.
- Health server stopping during drain (liveness fails during drain window) — documented as harmless under normal K8s termination; not changed.

### Out of Scope — startup ordering guarantees
- The documented health-before-DB-retry and readiness-503-before-grace startup ordering guarantees (SPEC-OBS-001) are NOT changed. F-43 reorders ONLY within the existing Step 10-11 (bind before `set_ready()`).

### Out of Scope — money representation
- No change to the `Decimal`-everywhere invariant (REQ-OBS-052 / REQ-PROV-012). No `f64` is introduced.

## @MX Annotation Targets (high fan_in)

- The shared metric-name consts (`src/metrics/mod.rs`) — `@MX:ANCHOR` (every emitter across SPECs binds to these names) + `@MX:NOTE` recording the F-38 **operator-visible rename** rationale (external Grafana series keyed on `coin_*` must migrate).
- The generic `run_supervised` (`src/collectors/mod.rs`) — `@MX:ANCHOR` (all workers + relays route through it) + `@MX:WARN`/`@MX:REASON` on the restart-backoff-and-reset policy (a wrong reset window re-introduces the deterministic-crasher restart storm).
- The bounded-shutdown sequence (`src/main.rs`) — `@MX:ANCHOR` + `@MX:WARN`: ordering (grace sleep → broadcast → `timeout(drain_secs, supervisor)` → `pool.close()` → `telemetry::shutdown()`) is load-bearing for zero-drop rollouts (REQ-OBS-066/067).
- The readiness gate (`src/health/mod.rs`) — `@MX:WARN` on flags-before-cache ordering (REQ-OBS-070); pairs with the existing SPEC-OBS-001 readiness `@MX:ANCHOR`.
- The config diagnostics helpers (`src/config.rs`) — `@MX:NOTE` recording the warn-vs-fail-fast policy split (REQ-OBS-071/072).

## Open Items (do not guess)

- **OR-OBS2-1:** Exact backoff constants for `run_supervised` (initial delay, cap, reset-after-healthy window) — run-phase picks values consistent with the `src/db/pool.rs:22-23` / `migrate_with_retry` precedent; record the chosen constants in progress.md.
- **OR-OBS2-2:** The sustained-window duration for `all_providers_down` (REQ-ALARM-080) — run-phase selects reusing the existing `sustained_*` helper's window semantics; record the chosen window.
- **OR-OBS2-3:** REQ-ALARM-081 authoritative-source decision (doc vs code for `observe_chain_records`) AND whether the `provider-unreachable` streak should count non-`Network` failures — resolve against REQ-ALARM-020 during run; do not guess the intended alarm semantics.
- **OR-OBS2-4:** `src/collectors/live_poller.rs:665` `tracked_markets` reference — run-phase verifies whether it is a live emit or describe-only and removes/reconciles it consistently with dropping the ghost gauge (REQ-OBS-062).
- **OR-OBS2-5:** Placement of the moved `OtelMakeSpan` within `src/telemetry/` and which of the two duplicated test sets is canonical (REQ-OBS-074) — run-phase de-duplicates.
