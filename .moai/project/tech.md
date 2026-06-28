# Crypto Collector — Technology Stack

## Overview

Crypto Collector is built on a proven async Rust stack inherited and adapted from the ticker-collector equities microservice. The stack emphasizes compile-time safety, async I/O efficiency, and production-grade observability.

## Core Runtime and Framework

**Async Runtime: Tokio**
- Industry-standard async runtime for Rust.
- Enables efficient concurrent handling of background workers (live_poller, collection_queue, backfill).
- Rationale: Tokio is battle-tested in production microservices; ticker-collector uses it extensively.

**Web Framework: Axum**
- Lightweight, modular HTTP framework built on Tokio.
- Composable middleware stack via Tower.
- Strongly-typed routing and request extraction.
- Rationale: Axum balances performance and maintainability; less boilerplate than Actix; better type safety than Rocket.

**HTTP Client: reqwest**
- Async HTTP client with connection pooling.
- Used for all external provider API calls (CoinGecko, CoinMarketCap, Binance, etc.).
- Rationale: Standard choice for async HTTP in Rust; integrates seamlessly with Tokio.

## Data Serialization and Types

**Serialization: serde + serde_json**
- Structured serialization/deserialization for JSON API responses and database models.
- Derive macros for minimal boilerplate.
- Rationale: Industry standard; performant JSON handling.

**Numeric Precision: rust_decimal**
- Arbitrary-precision decimal type for prices and supply counts.
- Avoids floating-point rounding errors in financial calculations.
- Rationale: Crypto prices demand exact decimal arithmetic (e.g., $0.0001 cent precision for altcoins).

**Date/Time: chrono**
- Timezone-aware and naive datetime types.
- Conversion to/from timestamps (UNIX epoch).
- Rationale: Standard for time handling in Rust; integrates with sqlx queries.

**Unique Identifiers: uuid**
- UUID v4 generation for trace IDs, request IDs, worker instance IDs.
- Rationale: Lightweight unique identification without database round-trips.

## Database

**PostgreSQL**
- Relational database backend for all persistent state (quotes, coins, derivatives, work queue, pacer table).
- ACID transactions with row-level locking for multi-replica coordination (`FOR UPDATE SKIP LOCKED`).
- Rationale: Same backend as ticker-collector; proven for time-series data; native advisory locks for inter-process coordination.

**SQL Query Builder: sqlx**
- Async-first, compile-time-checked SQL queries.
- `sqlx::query!` macro verifies queries against live database schema at compile time.
- Connection pooling via `PgPool`.
- Rationale: Type safety catches SQL errors before runtime; async support is native; ticker-collector uses it.

**Schema Versioning: sqlx migrations**
- SQL migration files in `migrations/` directory; executed on startup.
- Supports RANGE partitioning, BRIN indexes, and advisory lock tables.
- Rationale: Declarative, version-controlled schema; works with sqlx; no external tool dependency.

## Error Handling

**Application Errors: anyhow**
- Flexible error type for application-level error propagation (?, .context()).
- Error chains preserved through async boundaries.
- Rationale: Lighter than thiserror for internal error handling; good for quick error context.

**Library Errors: thiserror**
- Structured error enums for library-facing error types.
- `#[from]` and `#[source]` derive macros for error composition.
- Rationale: When exporting error types from modules; better error semantics for API consumers.

## Observability

### Distributed Tracing

**Tracing Library: tracing + tracing-subscriber**
- Structured logging via `tracing` macros (debug!, info!, warn!, error!, trace!).
- Span-based context tracking for distributed tracing.
- Subscriber configuration in `telemetry/logs.rs`.
- Rationale: Modern, composable structured logging; integrates with OpenTelemetry.

**OpenTelemetry Export: opentelemetry + opentelemetry-otlp**
- OTLP/gRPC exporter for traces to a collector (e.g., Jaeger, Tempo, or vendor-hosted tracing service).
- W3C Trace Context and Baggage propagation for cross-service tracing.
- Rationale: Vendor-neutral tracing standard; enables correlation of logs and traces across services.

**HTTP Middleware Instrumentation: tower-http**
- Auto-instrumentation of all HTTP requests/responses via Tower middleware.
- Captures request method, path, status code, and duration.
- Integrates with tracing for automatic span generation.
- Rationale: Minimal manual instrumentation; consistent across all endpoints.

### Metrics

**Prometheus Metrics: prometheus + metrics**
- Counter, Gauge, Histogram, Summary metric types.
- Metrics registry and in-process exposition.
- Rationale: Industry-standard monitoring format; ticker-collector uses it.

**Exporter: metrics-exporter-prometheus (or similar)**
- `/metrics` endpoint exposing Prometheus text format.
- Scraped by Prometheus/Grafana or other monitoring systems.
- Rationale: Standard Prometheus scrape protocol; consistent with existing monitoring infrastructure.

**Instrumentation Points**
- Request count and latency per endpoint.
- Quote insertion latency (per provider).
- Provider API call latency and error rates.
- Background worker job duration and work-queue claim rates.
- Database connection pool stats.
- Rationale: Enable visibility into performance bottlenecks and provider reliability.

## Concurrency and Coordination

**Advisory Locks: PostgreSQL Native**
- Used for inter-replica coordination (e.g., pacer rate-limiting, work-queue exclusive claiming).
- `SELECT pg_advisory_lock(id)` / `pg_advisory_unlock(id)` for advisory locks.
- `FOR UPDATE SKIP LOCKED` for non-blocking row claiming in work-queue queries.
- Rationale: No external distributed-lock service needed; PostgreSQL transactions are the source of truth.

**Background Worker Spawning: tokio::spawn**
- Live poller, backfill worker, and collection-queue worker spawned at startup.
- Each worker runs in its own Tokio task.
- Graceful shutdown via signal handling (SIGTERM/SIGINT).
- Rationale: Tokio task spawning is lightweight; cancellation tokens enable clean shutdown.

## Crypto Data Providers

### Primary Aggregators (Configuration Choice)

**Candidate: CoinGecko API**
- Free and premium tiers.
- Broad asset coverage, market metadata, derivatives data, historical OHLCV.
- Rate limits: 10-50 calls/minute depending on tier.
- No authentication required for free tier.
- Rationale: Widely used, stable, good free tier.

**Candidate: CoinMarketCap API**
- Free and premium tiers.
- Broader derivatives coverage than CoinGecko.
- Rate limits: 333 calls/day free tier.
- Requires API key authentication.
- Rationale: Trusted data source; stronger derivatives data.

**Selection Process**: Product and SPEC decision; environment variable configures choice. Both APIs normalized to internal models.

### Optional Fallback Exchanges

**Binance API (REST + WebSocket)**
- Largest crypto exchange by volume.
- Extensive spot and derivatives data.
- Free public endpoints (no authentication for public data).
- Candidate: Hand-rolled reqwest client wrapping Binance REST v3 API; or use existing crate (e.g., `binance-api` crate if suitable).
- Rationale: Highest trading volume; strong data quality.

**Coinbase API (REST)**
- US/EU regulated exchange.
- Public order book, ticker, and trade data.
- Authentication optional for public endpoints.
- Candidate: Hand-rolled reqwest client or existing `coinbase-pro` crate.
- Rationale: Regulatory trust; strong venue for US institutional flows.

**Kraken API (REST + WebSocket)**
- European exchange with strong crypto/fiat liquidity.
- Public OHLCV, ticker, and order book data.
- Candidate: Hand-rolled reqwest client or existing `kraken` crate.
- Rationale: Geographic diversity; strong EUR/crypto pairs.

### Provider Client Architecture (TBD)

**Option A: Hand-Rolled Clients with reqwest**
- Dedicated module per provider wrapping HTTP calls.
- Pros: Full control, minimal dependencies, easier customization.
- Cons: More code to maintain; must parse all responses manually.

**Option B: Existing Ecosystem Crates**
- Candidates: `binance-rs`, `coinbase-pro`, `kraken-rs`, `coingecko-rs`.
- Pros: Reduced boilerplate, pre-built parsing.
- Cons: Dependency on external crate quality; may be over-engineered for our use case.

**Decision**: SPEC planning phase will evaluate crate quality, API coverage, and maintenance status. Likely hybrid: hand-rolled for primary aggregators (CoinGecko, CoinMarketCap), existing crates for exchanges if they meet quality bar.

## Build and Deployment

**Package Manager: Cargo**
- `Cargo.toml` pins all dependencies with checked-in `Cargo.lock`.
- Reproducible builds; dependency resolution is deterministic.
- Rationale: Standard Rust package manager.

**Dockerfile Strategy: Multi-Stage**
- Stage 1: Build in container with full Rust toolchain.
- Stage 2: Runtime container with binary only (minimal attack surface).
- Base image: `rust:latest` for build, `debian:bookworm-slim` or `distroless` for runtime.

**Cross-Compilation: cross crate**
- Dockerfile.aarch64 uses `cross` for aarch64 (ARM64) cross-compilation.
- Builds on x86_64, targets aarch64 architecture.
- Rationale: Crypto Collector cluster is aarch64; supports `make push-aarch64` target.

**Container Registry: registry.helles.farm**
- Images tagged as `registry.helles.farm/crypto-collector:<version>`.
- Pushed by CI/CD pipeline on merge to `main`.

## Kubernetes and Orchestration

**Helm Chart**
- Defines Deployment, Service, ConfigMap, Secret, HPA resources.
- Parameterizable via `values.yaml`: replica count, resource limits, provider configuration, worker intervals.
- Rationale: Standard for Kubernetes deployments; enables environment-specific overrides.

**Deployment Namespace: finance**
- Kubernetes namespace for crypto-collector and related services.
- Secrets for PostgreSQL credentials injected by CI/CD or secrets manager.

**Health Checks**
- Liveness probe: `/healthz/live` (always 200; confirms the process is alive and not deadlocked).
- Readiness probe: `/healthz/ready` (checks PostgreSQL connectivity and provider availability).
- Rationale: Kubernetes orchestrates pod restart and traffic management based on probe results.

## Crate Summary by Category

| Category | Primary Crate | Rationale |
|----------|---------------|-----------|
| **Async Runtime** | tokio | Industry standard; battle-tested. |
| **Web Framework** | axum | Lightweight, modular, type-safe. |
| **HTTP Client** | reqwest | Async-first, connection pooling. |
| **Serialization** | serde, serde_json | Standard; fast; derive macros. |
| **Numerics** | rust_decimal | Precise decimal arithmetic for prices. |
| **Date/Time** | chrono | Timezone support; UNIX timestamp conversion. |
| **Unique IDs** | uuid | Lightweight; UUID v4 generation. |
| **Error Handling** | anyhow, thiserror | Flexible + structured error types. |
| **Database** | sqlx | Async; compile-time checked queries. |
| **Logging** | tracing, tracing-subscriber | Structured; OpenTelemetry-native. |
| **Tracing Export** | opentelemetry, opentelemetry-otlp | Vendor-neutral OTLP/gRPC export. |
| **Metrics** | prometheus, metrics | Standard Prometheus format. |
| **HTTP Middleware** | tower-http | Auto-instrumentation for requests. |
| **Crypto Providers** | reqwest (+ hand-rolled or crate ecosystem) | TBD during SPEC planning. |

## TBD Decisions (SPEC Planning)

1. **CoinGecko vs. CoinMarketCap**: Evaluate free tier coverage, rate limits, and data freshness. Recommend primary; other available as fallback configuration.
2. **Crypto Provider Clients**: Decide hand-rolled reqwest wrappers vs. existing crates per exchange. Criteria: crate maturity, maintenance, API surface fit, test coverage.
3. **OpenTelemetry Backend**: Determine target tracing system (Jaeger, Tempo, vendor SaaS). Configure OTLP exporter endpoint in Helm chart.
4. **Metrics Backend**: Confirm Prometheus scrape configuration and retention policy.
5. **Database Sizing**: Determine retention period for quotes/candles; RANGE partition strategy (monthly? weekly?); backfill historical candle depth.

## Related Services

- **ticker-collector**: Equities sibling service; source for architectural patterns.
- **PostgreSQL**: Shared database cluster in `finance` namespace (or dedicated instance TBD).
- **Prometheus/Grafana**: Monitoring stack; scrapes `/metrics` endpoint.
- **Jaeger/Tempo**: Distributed tracing backend; receives OTLP/gRPC traces.
- **Kubernetes**: Cluster with aarch64 nodes; Helm chart deployed to `finance` namespace.
