# syntax=docker/dockerfile:1
# ── Stage 1: build ───────────────────────────────────────────────────────────
FROM rust:1-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Cache dependency layer — only rebuilds when Cargo.toml/Cargo.lock change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main(){}' > src/main.rs && \
    cargo build --release && \
    rm -f target/release/deps/crypto_collector* target/release/deps/crypto-collector*

# Build the real binary.
COPY src ./src
COPY migrations ./migrations
RUN cargo build --release

# ── Stage 2: runtime ─────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates && rm -rf /var/lib/apt/lists/*

RUN groupadd --gid 10001 app && \
    useradd --uid 10001 --gid 10001 --no-create-home app

COPY --from=builder /build/target/release/crypto-collector /usr/local/bin/crypto-collector
COPY --from=builder /build/migrations /migrations

USER app

# API port / health port / Prometheus metrics port
EXPOSE 8080 8081 9000

ENTRYPOINT ["/usr/local/bin/crypto-collector"]
