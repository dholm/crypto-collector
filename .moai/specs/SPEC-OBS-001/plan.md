# Implementation Plan — SPEC-OBS-001 (Observability, Health & Graceful Shutdown)

Contracts: [../SPEC-DB-001/spec.md](../SPEC-DB-001/spec.md) (pool/migrations),
[../SPEC-SCHED-001/spec.md](../SPEC-SCHED-001/spec.md) (worker cancellation),
[../SPEC-API-001/spec.md](../SPEC-API-001/spec.md) (HTTP spans),
[../SPEC-PROV-001/spec.md](../SPEC-PROV-001/spec.md) (provider metrics).
Research: [../SPEC-PROV-001/research.md](../SPEC-PROV-001/research.md) (§3.1 obs crates).
Methodology: greenfield TDD — health/readiness handler tests + a shutdown-ordering test.

## Technical Approach

`main` owns the startup sequence and the three listeners. A shared `AppState` carries
the `PgPool`, the readiness flag (`AtomicBool`/watch), and the workers'
`CancellationToken`. Health and metrics each get their own Axum app/listener. The
metrics registry is initialised once; emitters in other SPECs use the catalogue names.
Tracing is initialised conditionally on `OTEL_EXPORTER_OTLP_ENDPOINT`. The
shutdown future awaits SIGTERM/SIGINT and runs the ordered drain.

## Files (anticipated; confirmed in run phase)

| File | Change |
|---|---|
| `src/main.rs` (new) | Startup sequence, three listeners, signal handling, shutdown orchestration. |
| `src/health/mod.rs` (new) | `/healthz/live`, `/healthz/ready`, readiness gate over `AppState`. |
| `src/metrics/mod.rs` (new) | Registry init, `/metrics` exporter, metric catalogue + gauges refresh task. |
| `src/telemetry/mod.rs` (new) | `tracing-subscriber` JSON; OTLP/gRPC init (conditional); W3C propagation; `tower-http` layer. |
| `src/db/pool.rs` (new; shared with SPEC-DB-001) | Pool build + migration runner + DB ping for readiness. |
| `src/config.rs` (shared) | Ports, shutdown, OTEL_*, gauge interval. |

## Milestones (priority-ordered, no time estimates)

### Milestone 1 — three listeners + health (Priority High)
- RED: `/healthz/live` always 200; `/healthz/ready` 200 only when DB ping + migrations +
  workers are up, else 503; ready flips to 503 during shutdown grace
  (REQ-OBS-001..004). Ports default 8080/8081/9000.
- GREEN: health router + readiness gate + separate listeners.

### Milestone 2 — metrics registry + catalogue (Priority High)
- RED: `/metrics` serves Prometheus text; `http_requests_total` +
  `http_request_duration_seconds` recorded for a sample request; `tracked_*` gauges and
  backlog/pacer gauges registered (REQ-OBS-010..014).
- GREEN: registry init + `tower-http` metric layer + gauge refresh task.

### Milestone 3 — logging + tracing (Priority High)
- RED: JSON log lines to stdout filtered by `RUST_LOG`; with `OTEL_EXPORTER_OTLP_
  ENDPOINT` set, a span is exported and W3C context propagates; unset → no export, no
  startup failure (REQ-OBS-020..024).
- GREEN: `telemetry/mod.rs` conditional init + propagation + per-request span.

### Milestone 4 — graceful shutdown (Priority High)
- RED: on a simulated SIGTERM, readiness flips to 503 first, in-flight requests are
  served to the drain deadline, the worker cancellation token fires, then the pool
  closes — asserted in order (REQ-OBS-030..033).
- GREEN: shutdown orchestration in `main`.

### Milestone 5 — startup sequence + fail-fast (Priority High)
- RED: startup runs config→pool→migrations→telemetry→workers→listeners and readiness is
  false until done; a failed migration / unreachable DB aborts startup with a clear
  error (REQ-OBS-040/041).
- GREEN: ordered `main` + fail-fast error propagation.

### Milestone 6 — NFR verification (Priority Medium)
- Tests/assertions: no in-process data cache (statelessness review); a structural check
  that no upstream call bypasses the pacer (shared with SPEC-PROV-001 REQ-PROV-045); a
  precision sweep that no monetary value is `f64` end-to-end; a multi-replica concurrency
  test (shared with SPEC-SCHED-001) showing no duplicated work (REQ-OBS-050..054).

## Risks

- **Readiness correctness (highest).** A readiness gate that returns 200 before
  migrations/workers are up admits traffic into a broken pod; the gate must be false
  until the full startup completes and 503 during shutdown grace (REQ-OBS-003/004/040).
- **Drain ordering.** Cancelling workers or closing the pool before readiness drains
  causes dropped requests mid-rollout; the ordered shutdown is load-bearing
  (REQ-OBS-030..033) and pairs with the Helm `terminationGracePeriodSeconds`
  (SPEC-DEPLOY-001 REQ-OBS-033).
- **Tracing init fragility.** OTLP init must not panic when the endpoint is unset or
  unreachable; export is best-effort, logging is mandatory (REQ-OBS-022).
- **Metric cardinality.** `path` labels must be route templates, not raw paths with ids,
  to avoid unbounded cardinality (REQ-OBS-011).
- **Port-binding on saturation.** Health/metrics on separate listeners must remain
  responsive when the API is saturated (REQ-OBS-001).

## Definition of Done

- Three listeners (8080/8081/9000); liveness process-only; readiness gated on DB +
  migrations + workers and 503 during shutdown grace.
- `/metrics` serves the full catalogue (HTTP, collection, persistence, registry,
  backlog, pacer, pool).
- JSON logs to stdout; OTLP/gRPC tracing with W3C propagation when configured, disabled
  cleanly when not.
- Ordered graceful shutdown (readiness→drain→cancel→pool-close); startup fail-fast.
- NFRs verified: statelessness, paced egress, end-to-end decimal precision, horizontal
  scaling, crash recovery.
- All EARS REQ-OBS-001..054 covered by tests.
- Open items OR-OBS-1..4 resolved or explicitly deferred with user sign-off.
