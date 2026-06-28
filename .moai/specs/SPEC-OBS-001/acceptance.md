# Acceptance Criteria — SPEC-OBS-001 (Observability, Health & Graceful Shutdown)

Each scenario maps to EARS requirements in `spec.md`. Handler scenarios use
`axum-test`; shutdown/startup scenarios are integration-level.

## Scenario 1 — Three independent listeners (REQ-OBS-001)

- **Given** a started replica with defaults
- **When** ports are probed
- **Then** the API serves on 8080, health on 8081, and metrics on 9000, each as a
  separate listener.

## Scenario 2 — Liveness is process-only (REQ-OBS-002)

- **Given** a running process whose database is temporarily unreachable
- **When** `GET /healthz/live` is called
- **Then** it returns 200 (liveness does not check dependencies).

## Scenario 3 — Readiness gates on DB + migrations + workers (REQ-OBS-003)

- **Given** the process
- **When** the DB is reachable, migrations are applied, and workers are spawned
- **Then** `GET /healthz/ready` returns 200; if any of those is not satisfied, it
  returns 503.

## Scenario 4 — Readiness flips to 503 during shutdown grace (REQ-OBS-004/030)

- **Given** a healthy, ready process
- **When** SIGTERM is received and the grace window begins
- **Then** `GET /healthz/ready` returns 503 while in-flight requests are still served.

## Scenario 5 — Prometheus metrics exposed (REQ-OBS-010/011)

- **Given** the metrics listener
- **When** `GET /metrics` is called after some API traffic
- **Then** the response is Prometheus text including `http_requests_total{method,path,
  status}` and an `http_request_duration_seconds` histogram with route-template paths
  (not raw ids).

## Scenario 6 — Collection + registry + backlog + pacer + duration metrics (REQ-OBS-012/013/014/015)

- **Given** provider activity and tracked coins/markets
- **When** `/metrics` is scraped
- **Then** it includes `collection_requests_total{provider,capability,outcome}`,
  `tracked_coins`, `tracked_markets`, `collection_queue_pending`,
  `backfill_chunks_pending`, `pacer_cooldown_active{provider}`,
  `pacer_credits_used{provider}`, and the latency histograms
  `collection_request_duration_seconds{provider,capability}`,
  `quote_insert_duration_seconds`, and `candle_insert_duration_seconds`.

## Scenario 7 — Structured JSON logs (REQ-OBS-020/023)

- **Given** `RUST_LOG=info`
- **When** an API request is served
- **Then** stdout contains a JSON log line for the request carrying the active span's
  trace/span ids.

## Scenario 8 — OTLP tracing on/off by config (REQ-OBS-021/022/024)

- **Given** `OTEL_EXPORTER_OTLP_ENDPOINT` set to a collector
- **When** a request is served
- **Then** a span is exported over OTLP/gRPC with W3C `traceparent` propagation and
  service name/version/environment resource attributes; and **given** the variable
  unset, startup succeeds, no export is attempted, and logging continues.

## Scenario 9 — Graceful shutdown ordering (REQ-OBS-030/031/032)

- **Given** a ready process serving an in-flight request
- **When** SIGTERM is received
- **Then** the order observed is: readiness → 503 (grace), in-flight request completes
  within the drain deadline, worker cancellation token fires (workers stop claiming),
  then the DB pool closes and the process exits.

## Scenario 10 — terminationGracePeriod sizing (REQ-OBS-033)

- **Given** `SHUTDOWN_GRACE_SECONDS` and `SHUTDOWN_DRAIN_SECONDS`
- **When** the Helm value is computed (SPEC-DEPLOY-001)
- **Then** `terminationGracePeriodSeconds >= grace + drain + buffer`, so no SIGKILL
  occurs mid-drain.

## Scenario 11 — Startup sequence + fail-fast (REQ-OBS-040/041)

- **Given** a starting process
- **When** startup runs
- **Then** the order is config → pool → migrations → telemetry → workers → listeners,
  readiness is false until complete; and a failed migration or unreachable DB aborts
  startup with a clear error rather than serving traffic.

## Scenario 12 — Statelessness (REQ-OBS-050)

- **Given** two replicas behind a load balancer
- **When** a client's successive requests hit different replicas
- **Then** responses are consistent with PostgreSQL state and there is no in-process
  cache or sticky-session dependency.

## Scenario 13 — Paced egress NFR (REQ-OBS-051)

- **Given** the full service
- **When** upstream calls are made from any path
- **Then** every call is preceded by a pacer `acquire_slot` (no unpaced upstream
  request exists — shared with SPEC-PROV-001 REQ-PROV-045).

## Scenario 14 — Precision NFR end-to-end (REQ-OBS-052)

- **Given** a tiny-price / huge-supply asset
- **When** it flows provider → DB → API
- **Then** the value is exact at every stage (`Decimal`/`NUMERIC`/lossless JSON); no
  `f64` monetary representation exists in the pipeline.

## Scenario 15 — Horizontal scaling + crash recovery (REQ-OBS-053/054)

- **Given** N replicas collecting concurrently
- **When** one replica is killed mid-work
- **Then** no persisted data is lost, the killed replica's claimed work is recovered via
  lease/marker expiry (SPEC-SCHED-001), and no collection work is duplicated across the
  surviving replicas.

## Quality Gate / Definition of Done

- [ ] Three listeners; liveness process-only; readiness gated and 503 during grace
      (1–4).
- [ ] Full Prometheus metric catalogue exposed with bounded label cardinality, incl.
      duration histograms (5, 6).
- [ ] JSON logs with trace correlation; OTLP tracing toggled by config (7, 8).
- [ ] Ordered graceful shutdown; terminationGracePeriod sized; startup fail-fast
      (9, 10, 11).
- [ ] NFRs verified: statelessness, paced egress, decimal precision, horizontal scaling,
      crash recovery (12–15).
- [ ] All EARS REQ-OBS-001..054 covered by tests.
- [ ] Open items OR-OBS-1..4 resolved or explicitly deferred with user sign-off.
