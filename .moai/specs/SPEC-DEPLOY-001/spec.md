---
id: DEPLOY-001
version: 1.1.0
status: planned
created: 2026-06-28
updated: 2026-06-28
author: dholm
priority: high
issue_number: null
---

# SPEC-DEPLOY-001 — Build, Container, Helm Chart & Configuration Surface

Foundation SPEC for build and deployment. Defines the Makefile target set, the
multi-stage `Dockerfile` and the aarch64 `Dockerfile.aarch64`, the production Helm chart
at `charts/crypto-collector/`, and the complete environment-variable configuration
surface (the only configuration mechanism).

Behaviour contracts wired here: [SPEC-OBS-001](../SPEC-OBS-001/spec.md) (ports, probes,
shutdown timing, metrics scrape), [SPEC-PROV-001](../SPEC-PROV-001/spec.md) (provider +
pacer config), [SPEC-SCHED-001](../SPEC-SCHED-001/spec.md) (cadence/lease config),
[SPEC-DB-001](../SPEC-DB-001/spec.md) (DB connection). Research:
[../SPEC-PROV-001/research.md](../SPEC-PROV-001/research.md) (§3.1 crate versions).

## HISTORY

- 2026-06-28 (v1.1.0): Relabelled REQ-DEPLOY-012 and REQ-CFG-007 from Unwanted to
  Ubiquitous — both are unconditional negative constraints (no triggering event), so they
  are Ubiquitous "shall not" constraints rather than EARS Unwanted-behaviour requirements.
  (audit m1)
- 2026-06-28 (v1.0.0): Initial greenfield deployment SPEC. Mirrors ticker-collector's
  proven deployment shape — 2 replicas, RollingUpdate `maxUnavailable=0`/`maxSurge=1`,
  pod anti-affinity, non-root + read-only-rootfs + drop-all-caps security context,
  liveness/readiness on :8081, ConfigMap (non-secret env) + Secret (DB creds),
  Prometheus scrape on :9000, optional PodMonitor/HPA — deployed to namespace `finance`,
  registry `registry.helles.farm/crypto-collector`, aarch64 via `cross` + prebuilt-binary
  `Dockerfile.aarch64` (`make push-aarch64`). Environment-variable-only configuration.

---

## Goal

Make Crypto Collector buildable, containerised (x86_64 and aarch64), and deployable to
the `finance` Kubernetes namespace as a horizontally-scalable, zero-drop-rollout,
hardened workload, with all behaviour controlled by a single, fully-enumerated
environment-variable configuration surface and database credentials supplied via
Kubernetes Secrets.

## Scope

In scope:
- A `Makefile` with build / check / lint / fmt / test / image / push targets plus
  aarch64 cross-compile + image + push targets (`make push-aarch64`).
- A multi-stage `Dockerfile` (x86_64) and a `Dockerfile.aarch64` (cross-compiled,
  prebuilt-binary, zero-RUN runtime).
- A Helm chart `charts/crypto-collector/` (Deployment, Service, ConfigMap, Secret
  wiring, optional PodMonitor, optional HPA, NOTES/helpers).
- The complete environment-variable configuration surface (the only config mechanism).
- The `Config::from_env` parsing contract (required vs defaulted keys; fail-fast on
  invalid).

Out of scope: see Exclusions. The runtime behaviour those settings drive lives in the
other SPECs (this SPEC wires and enumerates, it does not re-specify behaviour). The
actual chart/Dockerfile/Makefile *files* are implementation deliverables defined BY this
SPEC.

## Decisions Restated (authoritative)

- **D1 — Mirror ticker-collector's deployment shape**, adapted (research; product
  structure.md/tech.md): 2 replicas, RollingUpdate `maxUnavailable=0`/`maxSurge=1`,
  pod anti-affinity, hardened security context, probes on :8081, scrape on :9000.
- **D2 — Environment-variable-only configuration.** No config files; required keys error
  at startup; optional keys default. (product structure.md `config.rs`)
- **D3 — DB credentials via Kubernetes Secrets**; non-secret config via ConfigMap.
- **D4 — Two architectures:** x86_64 multi-stage build, and aarch64 via `cross` +
  prebuilt-binary `Dockerfile.aarch64` (the deployed cluster is aarch64;
  `make push-aarch64`).
- **D5 — Namespace `finance`, registry `registry.helles.farm/crypto-collector`.**
- **D6 — Shutdown timing wired:** `terminationGracePeriodSeconds = SHUTDOWN_GRACE_SECONDS
  + SHUTDOWN_DRAIN_SECONDS + buffer` (SPEC-OBS-001 REQ-OBS-033).

---

## Design Summary (WHAT, not HOW)

### Makefile targets

`build`, `build-release`, `check`, `lint` (`cargo fmt --check` + `cargo clippy
-D warnings` + `helm lint --strict charts/crypto-collector`), `fmt`, `test`, `image`
(native), `push`, `build-aarch64` (`cross build --target aarch64-unknown-linux-gnu
--release`), `image-aarch64` (build image from the prebuilt aarch64 binary), `push-aarch64`,
`clean`. Image vars default to `registry.helles.farm/crypto-collector:{latest,aarch64}`.

### Containers

- `Dockerfile` (x86_64): multi-stage — a `rust:*-slim` builder with cached-dependency
  layer, then a minimal `debian:*-slim` runtime with `ca-certificates`, a non-root
  user (uid/gid 10001), the binary, and the migrations; exposes 8080/8081/9000.
- `Dockerfile.aarch64`: a setup stage creating the non-root user + certs, then a
  `--platform=linux/arm64` runtime stage with **zero RUN commands** (QEMU-free),
  copying the `cross`-built `target/aarch64-unknown-linux-gnu/release` binary.

### Helm chart `charts/crypto-collector/`

- `Chart.yaml`, `values.yaml`, `templates/deployment.yaml`, `service.yaml`,
  `configmap.yaml`, `secret.yaml` (optional), `podmonitor.yaml` (optional),
  `hpa.yaml` (optional), `_helpers.tpl`, `NOTES.txt`.
- Deployment: `replicaCount: 2`; `strategy: RollingUpdate` with `maxUnavailable: 0`,
  `maxSurge: 1`; `minReadySeconds`; pod anti-affinity (preferred,
  `topologyKey: kubernetes.io/hostname`) when `replicaCount > 1`; pod security context
  `runAsNonRoot: true`, fixed uid/gid, `seccompProfile: RuntimeDefault`; container
  security context `allowPrivilegeEscalation: false`, `readOnlyRootFilesystem: true`,
  `capabilities.drop: [ALL]`; ports http/health/metrics; `envFrom` ConfigMap; DB
  credentials from Secret `secretKeyRef`; liveness `/healthz/live` and readiness
  `/healthz/ready` on the health port; Prometheus scrape annotations
  (`prometheus.io/scrape`, `/port 9000`, `/path /metrics`);
  `terminationGracePeriodSeconds` computed from the shutdown values; resource
  requests/limits.
- ConfigMap: all non-secret env (ports, RUST_LOG, providers, CoinGecko base URL/tier,
  pacer, poll/lease/backfill knobs, OTEL_*).
- Secret wiring: `DB_USERNAME`/`DB_PASSWORD` (and optional CoinGecko API key) via
  `secretKeyRef`, enabled only when a secret name is configured.
- Optional PodMonitor (scrape :9000) and optional HPA (min 2).

### Configuration surface (environment variables)

| Variable | Required | Default | Owner SPEC |
|---|---|---|---|
| `DB_HOST` | yes | — | DB-001 |
| `DB_PORT` | no | 5432 | DB-001 |
| `DB_NAME` | yes | — | DB-001 |
| `DB_USERNAME` | no (Secret) | — | DB-001 |
| `DB_PASSWORD` | no (Secret) | — | DB-001 |
| `HOST` | no | 0.0.0.0 | OBS-001 |
| `PORT` | no | 8080 | OBS-001 |
| `HEALTH_PORT` | no | 8081 | OBS-001 |
| `METRICS_PORT` | no | 9000 | OBS-001 |
| `RUST_LOG` | no | info | OBS-001 |
| `SHUTDOWN_GRACE_SECONDS` | no | 15 | OBS-001 |
| `SHUTDOWN_DRAIN_SECONDS` | no | 30 | OBS-001 |
| `PROVIDERS` | no | coingecko | PROV-001 |
| `COINGECKO_TIER` | no | demo | PROV-001 |
| `COINGECKO_BASE_URL` | no | tier-derived | PROV-001 |
| `COINGECKO_API_KEY` | no (Secret) | — | PROV-001 |
| `PACER_<PROVIDER>_MIN_GAP_MS` | no | per-provider | PROV-001 |
| `PACER_<PROVIDER>_COOLDOWN_SECONDS` | no | per-provider | PROV-001 |
| `PACER_<PROVIDER>_MONTHLY_CREDITS` | no | tier-derived | PROV-001 |
| `LIVE_QUOTE_POLL_INTERVAL_SECS` | no | (run) | SCHED-001 |
| `LIVE_POLL_MIN_INTERVAL_SECS` | no | (run) | SCHED-001 |
| `LIVE_POLL_MAX_INTERVAL_SECS` | no | (run) | SCHED-001 |
| `LIVE_POLL_CLAIM_TTL_SECS` | no | (run) | SCHED-001 |
| `COLLECTION_LEASE_SECONDS` | no | (run) | SCHED-001 |
| `COLLECTION_HEARTBEAT_INTERVAL_SECONDS` | no | (run) | SCHED-001 |
| `COLLECTION_MAX_ATTEMPTS` | no | (run) | SCHED-001 |
| `BACKFILL_LEASE_SECONDS` | no | (run) | SCHED-001 |
| `BACKFILL_HEARTBEAT_INTERVAL_SECONDS` | no | (run) | SCHED-001 |
| `BACKFILL_MAX_ATTEMPTS` | no | (run) | SCHED-001 |
| `CANDLE_INTERVALS` | no | (run) | API-001 |
| `TRACKED_GAUGE_INTERVAL_SECS` | no | 30 | OBS-001 |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | no | — | OBS-001 |
| `OTEL_SERVICE_VERSION` | no | — | OBS-001 |
| `DEPLOYMENT_ENVIRONMENT` | no | — | OBS-001 |

---

## Requirements (EARS)

### Build (Makefile)

- **REQ-DEPLOY-001** (Ubiquitous): The project shall provide a `Makefile` with targets
  `build`, `build-release`, `check`, `lint`, `fmt`, `test`, `image`, `push`,
  `build-aarch64`, `image-aarch64`, `push-aarch64`, and `clean`.
- **REQ-DEPLOY-002** (Ubiquitous): The `lint` target shall run `cargo fmt --check`,
  `cargo clippy --all-targets --all-features -- -D warnings`, and
  `helm lint --strict charts/crypto-collector`.
- **REQ-DEPLOY-003** (Ubiquitous): The `build-aarch64` target shall cross-compile for
  `aarch64-unknown-linux-gnu` using `cross`, and `push-aarch64` shall build and push the
  aarch64 image, defaulting to `registry.helles.farm/crypto-collector:aarch64`.

### Containers

- **REQ-DEPLOY-010** (Ubiquitous): The project shall provide a multi-stage `Dockerfile`
  that builds the release binary in a Rust builder stage (with a cached-dependency
  layer) and ships it in a minimal runtime image running as a non-root user, including
  the `migrations/` directory and `ca-certificates`, exposing ports 8080, 8081, 9000.
- **REQ-DEPLOY-011** (Ubiquitous): The project shall provide a `Dockerfile.aarch64` that
  consumes the `cross`-built aarch64 binary and assembles a `linux/arm64` runtime image
  with zero RUN commands in the runtime stage (no QEMU required), running as a non-root
  user with `ca-certificates`.
- **REQ-DEPLOY-012** (Ubiquitous): The runtime images shall not run as root and shall not
  include the build toolchain.

### Helm chart

- **REQ-DEPLOY-020** (Ubiquitous): The project shall provide a Helm chart at
  `charts/crypto-collector/` deployable to the `finance` namespace, with the image
  defaulting to `registry.helles.farm/crypto-collector`.
- **REQ-DEPLOY-021** (Ubiquitous): The Deployment shall default to 2 replicas with a
  `RollingUpdate` strategy of `maxUnavailable: 0` and `maxSurge: 1` and a non-zero
  `minReadySeconds`.
- **REQ-DEPLOY-022** (State-Driven): While `replicaCount > 1`, the Deployment shall
  declare preferred pod anti-affinity on `topologyKey: kubernetes.io/hostname`.
- **REQ-DEPLOY-023** (Ubiquitous): The pod shall run with `runAsNonRoot: true`, a fixed
  non-root uid/gid, and `seccompProfile: RuntimeDefault`; the container shall set
  `allowPrivilegeEscalation: false`, `readOnlyRootFilesystem: true`, and
  `capabilities.drop: [ALL]`.
- **REQ-DEPLOY-024** (Ubiquitous): The Deployment shall configure a liveness probe on
  `/healthz/live` and a readiness probe on `/healthz/ready`, both on the health port.
- **REQ-DEPLOY-025** (Ubiquitous): The pod shall expose Prometheus scrape annotations
  (`prometheus.io/scrape: "true"`, `prometheus.io/port: "9000"`,
  `prometheus.io/path: "/metrics"`) and the chart shall provide an optional PodMonitor.
- **REQ-DEPLOY-026** (Ubiquitous): Non-secret configuration shall be supplied via a
  ConfigMap (`envFrom`), and database credentials (and any provider API key) shall be
  supplied via Kubernetes Secret `secretKeyRef`, injected only when a secret name is
  configured.
- **REQ-DEPLOY-027** (Ubiquitous): The Deployment shall set
  `terminationGracePeriodSeconds` to at least `SHUTDOWN_GRACE_SECONDS +
  SHUTDOWN_DRAIN_SECONDS + buffer` (SPEC-OBS-001 REQ-OBS-033).
- **REQ-DEPLOY-028** (Ubiquitous): The chart shall provide an optional HPA with a
  minimum of 2 replicas, disabled by default.

### Configuration surface

- **REQ-CFG-001** (Ubiquitous): All runtime configuration shall be supplied via
  environment variables; the service shall read no configuration file.
- **REQ-CFG-002** (If/Unwanted): If a required variable (`DB_HOST`, `DB_NAME`) is
  absent, then `Config::from_env` shall fail at startup with an error naming the missing
  variable.
- **REQ-CFG-003** (If/Unwanted): If a provided variable has an unparseable value
  (e.g. a non-numeric port or interval), then `Config::from_env` shall fail at startup
  with an error naming the variable and its bad value.
- **REQ-CFG-004** (Ubiquitous): The system shall construct the database URL from
  `DB_HOST`/`DB_PORT`/`DB_NAME` and optional `DB_USERNAME`/`DB_PASSWORD`, omitting
  credentials from the URL when they are not supplied.
- **REQ-CFG-005** (Ubiquitous): Optional variables shall apply documented defaults when
  absent (per the configuration-surface table), and the full set of variables shall be
  documented in the chart `values.yaml` and the README.
- **REQ-CFG-006** (Ubiquitous): The configuration surface shall expose every behaviour
  knob owned by SPEC-DB/PROV/SCHED/API/OBS (DB connection, ports, shutdown, providers,
  CoinGecko tier/base-url/key, per-provider pacer, poll/lease/backfill cadences, candle
  intervals, gauge interval, OTEL_*).
- **REQ-CFG-007** (Ubiquitous): The system shall not embed secrets (DB password, API keys)
  in the image, the ConfigMap, or source; secrets shall arrive only via Secret-backed
  environment variables.

## Exclusions (What NOT to Build)

- **No re-specification of runtime behaviour** — this SPEC wires and enumerates config;
  the behaviour each knob drives is owned by SPEC-DB/PROV/SCHED/API/OBS.
- **No config files / no flags** — environment variables only (REQ-CFG-001).
- **No secrets in image/ConfigMap/source** (REQ-CFG-007).
- **No root container, no build toolchain in the runtime image** (REQ-DEPLOY-012).
- **No QEMU emulation for aarch64** — `cross` + prebuilt-binary, zero-RUN runtime stage
  (REQ-DEPLOY-011).
- **No CI/CD pipeline definition** here (the user commits to `main`; CI runs Makefile
  targets) — pipeline config is out of foundation scope.
- **No PostgreSQL provisioning / retention policy** — the DB is an external dependency;
  retention is OR-DEPLOY-1.

## @MX Annotation Targets (high fan_in)

- `Config::from_env` — `@MX:ANCHOR` (every subsystem reads `Config`) + `@MX:WARN`/
  `@MX:REASON`: required-key absence and unparseable values must fail startup, not
  silently default (REQ-CFG-002/003).
- The Deployment shutdown-timing template (`terminationGracePeriodSeconds`) —
  `@MX:WARN`/`@MX:REASON`: must be ≥ grace + drain + buffer or SIGKILL drops in-flight
  requests during rollout (REQ-DEPLOY-027, SPEC-OBS-001).
- The security-context block — `@MX:NOTE`: non-root + read-only-rootfs + drop-all-caps
  is the hardening contract (REQ-DEPLOY-023).
- The Secret wiring — `@MX:WARN`: credentials must never fall back into the ConfigMap
  (REQ-CFG-007).

## Open Items (do not guess)

- **OR-DEPLOY-1:** PostgreSQL retention window and partition-drop policy (shared with
  OR-DB-3). Deployment/ops decision.
- **OR-DEPLOY-2:** resource requests/limits sizing for the aarch64 cluster (ticker uses
  100m/128Mi requests, 500m/256Mi limits as a starting point). Confirm at run/ops.
- **OR-DEPLOY-3:** whether `readOnlyRootFilesystem: true` requires an `emptyDir` for any
  scratch path (e.g. `/tmp`); add a writable mount only if the binary needs it. Confirm
  at run.
- **OR-DEPLOY-4:** default values for the SCHED/PROV cadence + pacer knobs in
  `values.yaml`, bound by the CoinGecko tier budget (shared with OR-PROV-2, OR-SCHED-1).
