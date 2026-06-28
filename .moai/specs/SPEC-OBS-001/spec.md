---
id: OBS-001
version: 1.1.0
status: planned
created: 2026-06-28
updated: 2026-06-28
author: dholm
priority: high
issue_number: null
---

# SPEC-OBS-001 — Observability, Health, Graceful Shutdown & Non-Functional Requirements

Foundation SPEC for operability. Defines the three-port server topology, the health
endpoints and readiness gating, the Prometheus metrics surface, structured JSON
logging, OpenTelemetry tracing, graceful shutdown/drain, and the cross-cutting
non-functional requirements (statelessness, rate-limit compliance, precision,
horizontal scaling).

Consumers/contracts: [SPEC-DB-001](../SPEC-DB-001/spec.md) (pool + migration runner),
[SPEC-SCHED-001](../SPEC-SCHED-001/spec.md) (worker lifecycle/cancellation),
[SPEC-API-001](../SPEC-API-001/spec.md) (HTTP instrumentation),
[SPEC-PROV-001](../SPEC-PROV-001/spec.md) (provider outcome metrics).
Research: [../SPEC-PROV-001/research.md](../SPEC-PROV-001/research.md).

## HISTORY

- 2026-06-28 (v1.1.0): Added REQ-OBS-015 backing the latency histograms already listed in
  the Design §3 metrics catalogue (`collection_request_duration_seconds`,
  `quote_insert_duration_seconds`, `candle_insert_duration_seconds`) and extended
  Scenario 6 to assert them. (audit m4)
- 2026-06-28 (v1.0.0): Initial greenfield observability SPEC. Three ports (API 8080,
  health 8081, metrics 9000); `/healthz/live` + `/healthz/ready` with DB/migration/
  worker readiness gating; Prometheus metrics via `metrics` + `metrics-exporter-
  prometheus`; tracing JSON logs to stdout; OpenTelemetry OTLP/gRPC with W3C
  propagation; graceful shutdown (readiness flip → drain → worker cancellation → pool
  close). Encodes the cross-cutting NFRs (statelessness, rate-limit compliance,
  precision, horizontal scaling, graceful drain).

---

## Goal

Make Crypto Collector fully operable in Kubernetes: liveness/readiness probes that
reflect true health, a Prometheus metrics surface covering HTTP/collection/worker/DB
dimensions, structured JSON logs and distributed traces correlated via W3C context, and
a graceful shutdown that drains in-flight requests and cleanly stops workers without
data loss — and to state the non-functional guarantees the whole service must meet.

## Scope

In scope:
- Three independent listeners: API (`PORT`, default 8080), health (`HEALTH_PORT`,
  default 8081), metrics (`METRICS_PORT`, default 9000).
- `/healthz/live` (liveness) and `/healthz/ready` (readiness) on the health port.
- Readiness gating on DB reachability, migrations applied, and workers spawned.
- Prometheus metric registry and `/metrics` exposition; the metric catalogue.
- `tracing` + `tracing-subscriber` JSON logs to stdout, controlled by `RUST_LOG`.
- OpenTelemetry OTLP/gRPC exporter with W3C trace-context propagation and `tower-http`
  HTTP auto-instrumentation.
- Graceful shutdown: SIGTERM/SIGINT handling, readiness flip, drain window, worker
  cancellation, pool close.
- The startup sequence (config → pool → migrations → metrics/tracing → workers →
  servers) and the migration runner location.
- The cross-cutting non-functional requirements.

Out of scope: see Exclusions. The Helm probe wiring and env plumbing (SPEC-DEPLOY-001);
metric *emission* call sites inside other SPECs (this SPEC defines the catalogue +
registry, the owners emit).

## Decisions Restated (authoritative)

- **D1 — Three ports** (8080 API / 8081 health / 9000 metrics), each a separate
  listener, matching ticker-collector and product tech.md.
- **D2 — Readiness reflects dependencies:** ready only when DB is reachable, migrations
  are applied, and workers are running. Liveness is a cheap "process alive" check.
- **D3 — `metrics` + `metrics-exporter-prometheus`** for metrics; `tracing` JSON for
  logs; `opentelemetry-otlp` (gRPC) + `tracing-opentelemetry` for traces with W3C
  propagation. (research §3.1)
- **D4 — Graceful drain:** on SIGTERM, readiness flips to 503 for a grace window
  (endpoints drain from kube-proxy), then in-flight requests are given a drain
  deadline, then workers are cancelled and the pool closed. (ticker shutdown values)
- **D5 — Statelessness is a hard NFR:** PostgreSQL is the only state store; no in-process
  cache of collected data; any replica can serve any request.
- **D6 — Precision NFR:** monetary values are exact decimal end-to-end (SPEC-DB-001
  REQ-DB-040, SPEC-PROV-001 REQ-PROV-012, SPEC-API-001 REQ-API-073).

---

## Design Summary (WHAT, not HOW)

1. **Three listeners.** The API router (SPEC-API-001), the health router, and the
   metrics exporter each bind their own port so health/metrics stay available even if
   the API is saturated, and so Kubernetes probes and Prometheus scrape distinct ports.

2. **Health.**
   - `/healthz/live` → 200 whenever the process is running (no dependency checks).
   - `/healthz/ready` → 200 only when the readiness gate is satisfied: a successful DB
     ping, migrations applied, and the worker set spawned; otherwise 503. During
     shutdown grace, readiness is forced to 503.

3. **Metrics catalogue** (Prometheus, on `/metrics`):
   - `http_requests_total{method, path, status}` and
     `http_request_duration_seconds{method, path}` (histogram) — from `tower-http`.
   - `collection_requests_total{provider, capability, outcome}` and
     `collection_request_duration_seconds{provider, capability}` — provider calls
     (SPEC-PROV-001).
   - `quote_insert_duration_seconds` / `candle_insert_duration_seconds` — persistence
     latency.
   - `tracked_coins` (gauge) and `tracked_markets` (gauge) — registry sizes, refreshed
     on a configurable interval.
   - `collection_queue_pending` / `backfill_chunks_pending` (gauges).
   - `pacer_cooldown_active{provider}` (gauge 0/1) and `pacer_credits_used{provider}`
     (gauge) — egress governor state.
   - DB pool stats (in-use/idle connections).

4. **Logging.** `tracing-subscriber` with the JSON formatter to stdout, filtered by
   `RUST_LOG`; log records carry the active span's trace/span ids for log↔trace
   correlation.

5. **Tracing.** `opentelemetry-otlp` over gRPC to `OTEL_EXPORTER_OTLP_ENDPOINT` (when
   set), W3C `traceparent`/`tracestate` propagation inbound and outbound, `tower-http`
   producing a span per request; service name/version/environment as resource
   attributes. When the endpoint is unset, tracing export is disabled but logging
   continues.

6. **Graceful shutdown** (drives the Helm `terminationGracePeriodSeconds`):
   - Receive SIGTERM/SIGINT → flip readiness to 503 (`SHUTDOWN_GRACE_SECONDS`) so
     kube-proxy removes the pod from endpoints before connections are cut.
   - Stop accepting new requests; allow in-flight requests up to
     `SHUTDOWN_DRAIN_SECONDS`.
   - Fire the workers' `CancellationToken` (SPEC-SCHED-001) so they stop claiming and
     release/finish in-flight units.
   - Close the `PgPool`; exit.

7. **Startup sequence.** `Config::from_env` → build `PgPool` → run migrations
   (SPEC-DB-001) → init metrics + tracing → spawn workers (SPEC-SCHED-001) → bind the
   three listeners. Readiness reports false until this completes.

---

## Requirements (EARS)

### Server topology and health

- **REQ-OBS-001** (Ubiquitous): The system shall expose three independent listeners —
  API on `PORT` (default 8080), health on `HEALTH_PORT` (default 8081), and metrics on
  `METRICS_PORT` (default 9000).
- **REQ-OBS-002** (Ubiquitous): The system shall serve `GET /healthz/live` on the
  health port, returning 200 whenever the process is running, with no dependency checks.
- **REQ-OBS-003** (State-Driven): While the database is reachable, migrations are
  applied, and the background workers are spawned, `GET /healthz/ready` shall return
  200; otherwise it shall return 503.
- **REQ-OBS-004** (Event-Driven): When the process enters the shutdown grace window,
  `GET /healthz/ready` shall return 503 even though the process is still serving
  in-flight requests.

### Metrics

- **REQ-OBS-010** (Ubiquitous): The system shall expose a Prometheus `/metrics`
  endpoint on the metrics port in Prometheus text format.
- **REQ-OBS-011** (Ubiquitous): The system shall record `http_requests_total{method,
  path, status}` and an `http_request_duration_seconds` histogram for every API request
  via `tower-http` instrumentation.
- **REQ-OBS-012** (Ubiquitous): The system shall record `collection_requests_total{
  provider, capability, outcome}` for every upstream provider attempt (SPEC-PROV-001).
- **REQ-OBS-013** (Ubiquitous): The system shall expose `tracked_coins` and
  `tracked_markets` gauges, refreshed on a configurable interval
  (`TRACKED_GAUGE_INTERVAL_SECS`).
- **REQ-OBS-014** (Ubiquitous): The system shall expose gauges for collection backlog
  (`collection_queue_pending`, `backfill_chunks_pending`) and pacer state
  (`pacer_cooldown_active{provider}`, `pacer_credits_used{provider}`).
- **REQ-OBS-015** (Ubiquitous): The system shall record latency histograms
  `collection_request_duration_seconds{provider, capability}` for upstream provider calls
  (SPEC-PROV-001) and `quote_insert_duration_seconds` / `candle_insert_duration_seconds`
  for persistence, exposed on `/metrics`.

### Logging and tracing

- **REQ-OBS-020** (Ubiquitous): The system shall emit structured JSON logs to stdout via
  `tracing-subscriber`, filtered by `RUST_LOG`.
- **REQ-OBS-021** (State-Driven): While `OTEL_EXPORTER_OTLP_ENDPOINT` is set, the system
  shall export traces over OTLP/gRPC to that endpoint and propagate W3C trace context on
  inbound and outbound requests.
- **REQ-OBS-022** (State-Driven): While `OTEL_EXPORTER_OTLP_ENDPOINT` is unset, the
  system shall continue logging without attempting trace export and shall not fail
  startup.
- **REQ-OBS-023** (Ubiquitous): The system shall create a tracing span per inbound API
  request (method, path, status, duration) and correlate log records to the active span.
- **REQ-OBS-024** (Ubiquitous): The system shall attach service name, version
  (`OTEL_SERVICE_VERSION`), and deployment environment (`DEPLOYMENT_ENVIRONMENT`) as
  trace resource attributes when provided.

### Graceful shutdown

- **REQ-OBS-030** (Event-Driven): When the process receives SIGTERM or SIGINT, the
  system shall begin graceful shutdown: flip readiness to 503, stop accepting new
  connections, and continue serving in-flight requests.
- **REQ-OBS-031** (Event-Driven): When the grace window (`SHUTDOWN_GRACE_SECONDS`) has
  elapsed, the system shall enforce a drain deadline (`SHUTDOWN_DRAIN_SECONDS`) for
  in-flight requests.
- **REQ-OBS-032** (Event-Driven): When draining, the system shall fire the workers'
  cancellation token so workers stop claiming new work and release or finish in-flight
  units, then close the database pool, then exit.
- **REQ-OBS-033** (Ubiquitous): The shutdown timing shall be designed so that a
  Kubernetes `terminationGracePeriodSeconds` of at least `SHUTDOWN_GRACE_SECONDS +
  SHUTDOWN_DRAIN_SECONDS + buffer` avoids SIGKILL during drain (SPEC-DEPLOY-001 wires
  the value).

### Startup

- **REQ-OBS-040** (Event-Driven): When the process starts, the system shall, in order:
  parse config, build the DB pool, run migrations, initialise metrics and tracing, spawn
  workers, then bind the three listeners; and readiness shall report false until this
  completes.
- **REQ-OBS-041** (If/Unwanted): If migrations fail or the database is unreachable at
  startup, then the process shall fail fast with a clear error rather than serving
  traffic in a broken state.

### Non-functional requirements (cross-cutting)

- **REQ-OBS-050** (Ubiquitous): The service shall be stateless — PostgreSQL shall be the
  only state store, with no in-process cache of collected data, so any replica can serve
  any request and replicas can be added or removed freely.
- **REQ-OBS-051** (Ubiquitous): The service shall comply with upstream rate limits at all
  times via the SPEC-PROV-001 per-provider pacer; no code path shall issue an unpaced
  upstream request.
- **REQ-OBS-052** (Ubiquitous): The service shall represent all monetary/quantity values
  as exact decimals end-to-end (storage `NUMERIC`, runtime `Decimal`, lossless
  serialization) with no `f64` for monetary values.
- **REQ-OBS-053** (Ubiquitous): The service shall support horizontal scaling to multiple
  replicas with no duplicated collection work and no cross-replica coordination beyond
  PostgreSQL (SPEC-SCHED-001).
- **REQ-OBS-054** (Event-Driven): When a replica crashes, the system shall lose no
  persisted data and shall recover claimed-but-unfinished work via lease/marker expiry
  (SPEC-SCHED-001), and in-flight HTTP requests shall be retryable by clients against
  other replicas.

## Exclusions (What NOT to Build)

- **No metrics/health on the API port** — health (8081) and metrics (9000) are separate
  listeners so they survive API saturation (REQ-OBS-001).
- **No dependency checks in `/healthz/live`** — liveness is process-only; dependency
  health belongs to readiness (REQ-OBS-002/003).
- **No vendor-specific tracing SDK** — OTLP/gRPC only; the backend (Jaeger/Tempo/SaaS)
  is a deployment choice, not a code dependency.
- **No always-on trace export** — disabled when `OTEL_EXPORTER_OTLP_ENDPOINT` is unset
  (REQ-OBS-022).
- **No in-process data cache** and **no sticky sessions** — statelessness is required
  (REQ-OBS-050).
- **No `f64` for monetary values** (REQ-OBS-052).
- **No Helm/probe YAML here** — wiring is SPEC-DEPLOY-001 (this SPEC defines the
  behaviour the probes observe).

## @MX Annotation Targets (high fan_in)

- The readiness gate function — `@MX:ANCHOR` (Kubernetes traffic admission depends on
  it) + `@MX:WARN`/`@MX:REASON`: must reflect DB + migrations + workers, and must flip to
  503 during shutdown grace (REQ-OBS-003/004).
- The graceful-shutdown orchestrator — `@MX:ANCHOR` + `@MX:WARN`: ordering
  (readiness→drain→cancel→pool-close) is load-bearing for zero-drop rollouts
  (REQ-OBS-030..033).
- The metric registry init — `@MX:NOTE` enumerating the metric catalogue so emitters in
  other SPECs use consistent names/labels (REQ-OBS-010..014).
- The startup sequence in `main` — `@MX:ANCHOR` on the strict ordering (REQ-OBS-040/041).

## Open Items (do not guess)

- **OR-OBS-1:** default shutdown timings (`SHUTDOWN_GRACE_SECONDS` 15,
  `SHUTDOWN_DRAIN_SECONDS` 30 — ticker values) and the Helm
  `terminationGracePeriodSeconds` buffer. Rule normative (REQ-OBS-033); numbers at run.
- **OR-OBS-2:** the exact histogram buckets for the duration metrics. Run/ops tuning.
- **OR-OBS-3:** `TRACKED_GAUGE_INTERVAL_SECS` default (ticker uses 30). Run.
- **OR-OBS-4:** whether `/healthz/ready` also degrades on prolonged total-provider
  outage, or stays ready while serving last-persisted data (recommend: stay ready —
  reads degrade gracefully per SPEC-PROV-001 REQ-PROV-005). Confirm at run.
