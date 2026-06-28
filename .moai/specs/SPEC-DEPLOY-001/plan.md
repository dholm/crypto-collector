# Implementation Plan — SPEC-DEPLOY-001 (Build, Container, Helm & Config)

Contracts wired: [../SPEC-OBS-001/spec.md](../SPEC-OBS-001/spec.md) (ports/probes/
shutdown/scrape), [../SPEC-PROV-001/spec.md](../SPEC-PROV-001/spec.md),
[../SPEC-SCHED-001/spec.md](../SPEC-SCHED-001/spec.md),
[../SPEC-DB-001/spec.md](../SPEC-DB-001/spec.md).
Research: [../SPEC-PROV-001/research.md](../SPEC-PROV-001/research.md) (§3.1 versions).
Methodology: greenfield TDD — `Config::from_env` unit tests (mirroring ticker
`config.rs` tests) + `helm lint --strict` + `helm template` snapshot assertions.

## Technical Approach

Adapt the ticker-collector deployment artifacts (Makefile, both Dockerfiles, the Helm
chart) to crypto-collector names, the three ports (8080/8081/9000), the `finance`
namespace, the `registry.helles.farm/crypto-collector` image, and the crypto config
surface. `Config::from_env` is the single config reader: required keys error, optional
keys default, unparseable values error — each with a focused unit test like ticker's.

## Files (anticipated; confirmed in run phase)

| File | Change |
|---|---|
| `Makefile` (new) | Targets per REQ-DEPLOY-001..003; image vars default to the crypto registry. |
| `Dockerfile` (new) | Multi-stage x86_64; cached deps; non-root; copies `migrations/`; EXPOSE 8080 8081 9000. |
| `Dockerfile.aarch64` (new) | setup stage (user+certs) + zero-RUN `linux/arm64` runtime copying the `cross` binary. |
| `charts/crypto-collector/Chart.yaml`, `values.yaml`, `templates/*`, `_helpers.tpl`, `NOTES.txt` (new) | The chart per REQ-DEPLOY-020..028. |
| `src/config.rs` (new; shared) | `Config` struct + `from_env` (required/defaulted/fail-fast) covering the full surface. |
| `README.md` (new) | Document the env-var surface + deploy steps (REQ-CFG-005). |

## Milestones (priority-ordered, no time estimates)

### Milestone 1 — Config::from_env (Priority High)
- RED: required `DB_HOST`/`DB_NAME` absence → startup error (REQ-CFG-002); unparseable
  port/interval → error naming the var (REQ-CFG-003); DB URL with and without creds
  (REQ-CFG-004); each optional key defaults correctly (REQ-CFG-005) — unit tests
  mirroring ticker `config.rs`.
- GREEN: `Config` + `from_env` covering the full surface (REQ-CFG-006); no secret read
  from any source but env (REQ-CFG-007).

### Milestone 2 — Makefile (Priority High)
- RED/verify: `make lint` runs fmt-check + clippy(-D warnings) + helm lint;
  `make build-aarch64` invokes `cross`; image vars default to the crypto registry
  (REQ-DEPLOY-001..003).
- GREEN: author the Makefile.

### Milestone 3 — Dockerfiles (Priority High)
- Verify: x86_64 multi-stage builds, runs non-root, includes `migrations/` + certs,
  EXPOSEs 8080/8081/9000, no toolchain in runtime (REQ-DEPLOY-010/012); `Dockerfile.aarch64`
  has a zero-RUN runtime stage consuming the `cross` binary (REQ-DEPLOY-011).
- GREEN: author both Dockerfiles; `make image` / `make image-aarch64` build cleanly.

### Milestone 4 — Helm chart (Priority High)
- RED: `helm lint --strict` passes; `helm template` output asserts 2 replicas,
  `maxUnavailable:0`/`maxSurge:1`, anti-affinity when `replicaCount>1`, the hardened
  security context, probes on the health port, scrape annotations on 9000,
  `envFrom` ConfigMap, Secret `secretKeyRef` for DB creds, computed
  `terminationGracePeriodSeconds`, optional PodMonitor/HPA (REQ-DEPLOY-020..028).
- GREEN: author the chart templates + `values.yaml` documenting the full env surface.

### Milestone 5 — docs + parity (Priority Medium)
- README documents every env var (parity with the surface table) and the deploy flow
  (`make push-aarch64`, namespace `finance`) (REQ-CFG-005).

## Risks

- **Silent config defaults (highest).** A required key silently defaulting hides
  misconfiguration; `from_env` must fail fast (REQ-CFG-002/003) — the ticker lesson,
  guarded by unit tests.
- **Shutdown-timing mismatch.** If `terminationGracePeriodSeconds < grace + drain`,
  Kubernetes SIGKILLs mid-drain and drops requests during rollout; the template must
  compute it from the shutdown values (REQ-DEPLOY-027, SPEC-OBS-001 REQ-OBS-033).
- **Read-only rootfs.** `readOnlyRootFilesystem: true` breaks if the binary writes
  scratch files; verify or add a writable `emptyDir` (OR-DEPLOY-3).
- **Secret leakage.** Credentials must never fall back into the ConfigMap or image
  (REQ-CFG-007); the chart wires them only via `secretKeyRef`.
- **aarch64 cross build.** `cross` toolchain/target must be present in CI; the
  prebuilt-binary Dockerfile avoids QEMU but depends on the `cross` artifact existing
  (REQ-DEPLOY-011).
- **Pacer/cadence defaults in values.yaml.** Aggressive defaults exhaust the CoinGecko
  Demo budget; defaults are bound by the tier (OR-DEPLOY-4 / OR-PROV-2 / OR-SCHED-1).

## Definition of Done

- `Config::from_env` reads the full surface; required-key/unparseable failures are
  fail-fast; DB URL built correctly; no secret read outside env — all unit-tested.
- Makefile targets present and correct (lint includes helm lint; aarch64 via `cross`).
- `Dockerfile` (x86_64, non-root, multi-stage, includes migrations) and
  `Dockerfile.aarch64` (zero-RUN runtime) build cleanly.
- Helm chart passes `helm lint --strict`; `helm template` asserts the deployment shape,
  hardening, probes, scrape, ConfigMap/Secret wiring, and shutdown timing.
- README documents the full env surface and the `finance`/aarch64 deploy flow.
- All EARS REQ-DEPLOY-001..028 and REQ-CFG-001..007 covered by tests/assertions.
- Open items OR-DEPLOY-1..4 resolved or explicitly deferred with user sign-off.
