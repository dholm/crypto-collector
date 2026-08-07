# syntax=docker/dockerfile:1
# Unified multiarch image (linux/amd64 + linux/arm64), COPY-only — zero RUN
# commands, so a `--platform linux/amd64,linux/arm64` build never needs QEMU.
#
# Both release binaries must be pre-compiled on the host first:
#   target/x86_64-unknown-linux-gnu/release/crypto-collector   (cargo, native)
#   target/aarch64-unknown-linux-gnu/release/crypto-collector  (cross)
#
# Run `make rust-build` before `make image`.

ARG TARGETARCH

# ── Per-arch binary selection ────────────────────────────────────────────────
FROM scratch AS binary-amd64
COPY target/x86_64-unknown-linux-gnu/release/crypto-collector /crypto-collector

FROM scratch AS binary-arm64
COPY target/aarch64-unknown-linux-gnu/release/crypto-collector /crypto-collector

FROM binary-${TARGETARCH} AS binary

# ── Runtime ──────────────────────────────────────────────────────────────────
# distroless cc ships glibc, libssl/openssl, and ca-certificates. The uid/gid
# below matches the Helm chart's enforced securityContext (runAsUser/runAsGroup
# 10001 in charts/crypto-collector/templates/deployment.yaml).
FROM gcr.io/distroless/cc-debian13:nonroot AS runtime

COPY --from=binary /crypto-collector /usr/local/bin/crypto-collector
COPY migrations /migrations

USER 10001:10001

# API port / health port / Prometheus metrics port
EXPOSE 8080 8081 9000

ENTRYPOINT ["/usr/local/bin/crypto-collector"]
