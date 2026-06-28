# Acceptance Criteria — SPEC-DEPLOY-001 (Build, Container, Helm & Config)

Each scenario maps to EARS requirements in `spec.md`. Config scenarios are unit tests
(mirroring ticker `config.rs`); chart scenarios use `helm lint`/`helm template`.

## Scenario 1 — Makefile target set (REQ-DEPLOY-001/002/003)

- **Given** the `Makefile`
- **When** targets are listed
- **Then** `build`, `build-release`, `check`, `lint`, `fmt`, `test`, `image`, `push`,
  `build-aarch64`, `image-aarch64`, `push-aarch64`, `clean` exist; `lint` runs
  `cargo fmt --check` + `cargo clippy … -D warnings` + `helm lint --strict
  charts/crypto-collector`; `build-aarch64` uses `cross` and `push-aarch64` defaults to
  `registry.helles.farm/crypto-collector:aarch64`.

## Scenario 2 — x86_64 Dockerfile non-root multi-stage (REQ-DEPLOY-010/012)

- **Given** the `Dockerfile`
- **When** built
- **Then** it is multi-stage with a cached-dependency layer, the runtime stage runs as a
  non-root user, includes `ca-certificates` and the `migrations/` directory, exposes
  8080/8081/9000, and contains no Rust build toolchain.

## Scenario 3 — aarch64 Dockerfile is QEMU-free (REQ-DEPLOY-011)

- **Given** `Dockerfile.aarch64` and a `cross`-built binary
- **When** the image is assembled
- **Then** the `linux/arm64` runtime stage has zero RUN commands, copies the prebuilt
  aarch64 binary, runs as a non-root user, and includes certs (via a setup stage).

## Scenario 4 — Helm lint passes (REQ-DEPLOY-020)

- **Given** `charts/crypto-collector/`
- **When** `helm lint --strict` runs
- **Then** it passes with no errors, and the image repository defaults to
  `registry.helles.farm/crypto-collector`.

## Scenario 5 — Deployment rollout + anti-affinity (REQ-DEPLOY-021/022)

- **Given** default values (`replicaCount: 2`)
- **When** `helm template` renders the Deployment
- **Then** it sets 2 replicas, `RollingUpdate` with `maxUnavailable: 0`/`maxSurge: 1`,
  a non-zero `minReadySeconds`, and preferred pod anti-affinity on
  `kubernetes.io/hostname`.

## Scenario 6 — Hardened security context (REQ-DEPLOY-023)

- **Given** the rendered Deployment
- **When** the pod/container security contexts are inspected
- **Then** the pod has `runAsNonRoot: true`, a fixed non-root uid/gid, and
  `seccompProfile: RuntimeDefault`; the container has `allowPrivilegeEscalation: false`,
  `readOnlyRootFilesystem: true`, and `capabilities.drop: [ALL]`.

## Scenario 7 — Probes + scrape (REQ-DEPLOY-024/025)

- **Given** the rendered Deployment
- **When** probes and annotations are inspected
- **Then** liveness is `/healthz/live` and readiness is `/healthz/ready` on the health
  port, and the pod carries `prometheus.io/scrape: "true"`, `prometheus.io/port: "9000"`,
  `prometheus.io/path: "/metrics"`; an optional PodMonitor is available.

## Scenario 8 — ConfigMap + Secret wiring (REQ-DEPLOY-026/CFG-007)

- **Given** the chart with a configured DB secret name
- **When** rendered
- **Then** non-secret env comes from a ConfigMap via `envFrom`, `DB_USERNAME`/
  `DB_PASSWORD` (and any provider API key) come from `secretKeyRef`, and no credential
  appears in the ConfigMap or the image.

## Scenario 9 — terminationGracePeriod sized from shutdown values (REQ-DEPLOY-027)

- **Given** `SHUTDOWN_GRACE_SECONDS` and `SHUTDOWN_DRAIN_SECONDS`
- **When** the Deployment renders
- **Then** `terminationGracePeriodSeconds >= grace + drain + buffer`.

## Scenario 10 — Optional HPA (REQ-DEPLOY-028)

- **Given** `autoscaling.enabled: true`
- **When** rendered
- **Then** an HPA with `minReplicas >= 2` is produced; when disabled (default), no HPA
  is rendered.

## Scenario 11 — Required config keys fail fast (REQ-CFG-002)

- **Given** `DB_HOST` (or `DB_NAME`) unset
- **When** `Config::from_env` runs
- **Then** it returns an error naming the missing variable; the process does not start.

## Scenario 12 — Unparseable config fails fast (REQ-CFG-003)

- **Given** `PORT=abc` (or any non-numeric numeric key)
- **When** `Config::from_env` runs
- **Then** it returns an error naming the variable and its bad value.

## Scenario 13 — DB URL with/without credentials (REQ-CFG-004)

- **Given** `DB_HOST`/`DB_PORT`/`DB_NAME` with and without `DB_USERNAME`/`DB_PASSWORD`
- **When** `Config::from_env` builds the database URL
- **Then** the URL includes credentials when both are present and omits them otherwise.

## Scenario 14 — Optional defaults + full surface coverage (REQ-CFG-005/006/001)

- **Given** only the required keys set
- **When** `Config::from_env` runs
- **Then** every optional key takes its documented default (ports 8080/8081/9000,
  `PROVIDERS=coingecko`, `COINGECKO_TIER=demo`, etc.), the full behaviour surface
  (DB/ports/shutdown/providers/pacer/cadence/intervals/OTEL) is represented, and no
  configuration file is read.

## Quality Gate / Definition of Done

- [ ] Makefile target set incl. helm lint and `cross` aarch64 (1).
- [ ] x86_64 non-root multi-stage Dockerfile; QEMU-free aarch64 Dockerfile (2, 3).
- [ ] `helm lint --strict` passes; rollout + anti-affinity; hardened security context;
      probes + scrape; ConfigMap/Secret wiring; sized terminationGracePeriod; optional
      PodMonitor/HPA (4–10).
- [ ] `Config::from_env`: required fail-fast, unparseable fail-fast, DB URL build,
      optional defaults, full surface, env-only, no secret leakage (11–14).
- [ ] README documents the env surface and the `finance`/aarch64 deploy flow.
- [ ] All EARS REQ-DEPLOY-001..028 and REQ-CFG-001..007 covered.
- [ ] Open items OR-DEPLOY-1..4 resolved or explicitly deferred with user sign-off.
