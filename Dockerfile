# syntax=docker/dockerfile:1
# ── Stage 1: build ───────────────────────────────────────────────────────────
FROM rust:1-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Cache dependency layer — only rebuilds when Cargo.toml/Cargo.lock change.
# The crate declares both a [lib] (src/lib.rs) and a [[bin]] (src/main.rs), so the
# stub must provide both files or `cargo build` errors on the missing lib target.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main(){}' > src/main.rs && touch src/lib.rs && \
    cargo build --release && \
    rm -f target/release/deps/crypto_collector* target/release/deps/crypto-collector* \
          target/release/deps/libcrypto_collector*

# Build the real binary.
COPY src ./src
COPY migrations ./migrations
RUN cargo build --release

# ── Stage 2: runtime ─────────────────────────────────────────────────────────
# distroless cc ships glibc, libssl/openssl, and ca-certificates; the :nonroot
# tag runs as uid/gid 65532 with no shell or package manager.
FROM gcr.io/distroless/cc-debian13:nonroot AS runtime

COPY --from=builder /build/target/release/crypto-collector /usr/local/bin/crypto-collector
COPY --from=builder /build/migrations /migrations

USER nonroot

# API port / health port / Prometheus metrics port
EXPOSE 8080 8081 9000

ENTRYPOINT ["/usr/local/bin/crypto-collector"]
