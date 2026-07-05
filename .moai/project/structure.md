# Crypto Collector — Repository Structure

This document describes the planned module and directory layout for the Crypto Collector microservice. Crypto Collector is greenfield (no code yet), so this reflects the intended structure to be built during the run phase.

## Repository Layout

```
crypto-collector/
├── src/
│   ├── main.rs                # Entry point: server startup, signal handling
│   ├── lib.rs                 # Library root, re-exports public modules
│   ├── config.rs              # Environment-variable configuration parsing
│   ├── error.rs               # Application error types and Display impl
│   ├── models/                # Domain models (Quote, Coin, Derivatives)
│   │   ├── mod.rs
│   │   ├── quote.rs           # Price snapshot and OHLCV candle models
│   │   ├── coin.rs            # Coin metadata, supply, market cap
│   │   └── derivatives.rs      # Funding rate, open interest, mark price
│   ├── db/                    # Database layer (sqlx queries)
│   │   ├── mod.rs
│   │   ├── pool.rs            # PgPool initialization and health checks
│   │   ├── quotes.rs          # Quote insert/query operations
│   │   ├── coins.rs           # Coin metadata insert/query operations
│   │   └── derivatives.rs      # Derivatives data insert/query operations
│   ├── providers/             # Data source abstraction and client wrappers
│   │   ├── mod.rs
│   │   ├── coingecko.rs       # CoinGecko API HTTP client (foundation primary)
│   │   │                       # coinmarketcap.rs — future work, not in foundation
│   │   ├── binance.rs         # Binance REST client (optional fallback)
│   │   ├── coinbase.rs        # Coinbase API client (optional fallback)
│   │   ├── kraken.rs          # Kraken API client (optional fallback)
│   │   └── chain.rs           # Provider fallback chain orchestration
│   ├── collectors/            # Background worker implementations
│   │   ├── mod.rs
│   │   ├── live_poller.rs     # Real-time price polling from primary provider
│   │   ├── collection_queue.rs # Durable work queue with FOR UPDATE SKIP LOCKED claiming
│   │   └── backfill.rs        # Historical candle backfill from providers
│   ├── api/                   # HTTP API handlers and routing
│   │   ├── mod.rs
│   │   ├── v1/                # Stable read-only endpoints (v1)
│   │   │   ├── mod.rs
│   │   │   ├── quotes.rs      # GET /api/v1/quotes, /api/v1/quotes/:pair
│   │   │   ├── candles.rs     # GET /api/v1/candles/:pair with cursor pagination
│   │   │   ├── coins.rs       # GET /api/v1/coins, /api/v1/coins/:id
│   │   │   └── derivatives.rs # GET /api/v1/derivatives
│   │   └── v2/                # Emerging endpoints (v2, frozen after v1 ships)
│   │       ├── mod.rs
│   │       └── ...            # Future features added here, maintaining v1 compatibility
│   ├── health/                # Health check handlers
│   │   └── mod.rs             # /healthz/live (liveness), /healthz/ready (readiness)
│   ├── telemetry/             # OpenTelemetry and tracing infrastructure
│   │   ├── mod.rs
│   │   ├── tracer.rs          # OTLP gRPC exporter initialization
│   │   └── logs.rs            # tracing-subscriber JSON logging setup
│   ├── metrics/               # Prometheus metrics
│   │   ├── mod.rs
│   │   └── prometheus.rs      # Metrics registry and /metrics endpoint
│   └── pacer/                 # Rate limiter for upstream provider API calls
│       └── mod.rs             # Request pacing table, advisory locks for coordination
├── migrations/                # sqlx SQL migrations (executed on startup)
│   ├── 001_initial_schema.sql # Quotes, coins, derivatives tables; indexes; RANGE partitions
│   └── ...
├── api/
│   └── crypto-collector.yaml  # OpenAPI v3.1 specification
├── charts/
│   └── crypto-collector/      # Helm chart for Kubernetes deployment
│       ├── Chart.yaml
│       ├── values.yaml        # Configurable replicas, resource limits, provider choices
│       ├── templates/
│       │   ├── deployment.yaml
│       │   ├── service.yaml
│       │   ├── configmap.yaml # Provider config, worker intervals
│       │   ├── secret.yaml    # PostgreSQL credentials (injected by CI/CD)
│       │   └── hpa.yaml       # Horizontal Pod Autoscaler (optional)
│       └── _helpers.tpl
├── Dockerfile                 # Linux x86_64 container image
├── Dockerfile.aarch64         # aarch64 (ARM64) cross-compiled image
├── Makefile                   # build, lint, test, image, push + aarch64 targets
├── .gitignore
├── .moai/                     # MoAI configuration
├── CLAUDE.md
└── README.md
```

## Module Descriptions

### Core Modules

**`config.rs`**
- Parses environment variables into a structured Config struct.
- Environment keys: `DB_HOST`, `DB_PORT`, `DB_NAME`, `DB_USERNAME`, `DB_PASSWORD`, `HOST`, `PORT`, `HEALTH_PORT`, `METRICS_PORT`, `RUST_LOG`, provider chain specification, worker intervals, pacer tuning.
- Defaults or errors on missing required keys.
- No file-based config; environment variables only.

**`models/`**
- **`quote.rs`**: Snapshot quotes (symbol, base, quote, price, timestamp) and OHLCV candle models.
- **`coin.rs`**: Coin metadata (name, symbol, category), supply metrics (circulating, total, max), valuations (market cap, FDV).
- **`derivatives.rs`**: Funding rates, open interest, mark/index prices, aggregated per-venue.

**`db/`**
- **`pool.rs`**: PgPool initialization, connection health checks, migration runner.
- **`quotes.rs`**: INSERT quote snapshots, SELECT with range queries and cursor pagination.
- **`coins.rs`**: UPSERT coin metadata, SELECT by symbol/ID.
- **`derivatives.rs`**: INSERT/SELECT derivatives data, keyed by asset pair and venue.
- All queries use `sqlx::query!` (compile-time checked); partitioned tables with BRIN indexes on timestamp and keyset.

**`providers/`**
- **`coingecko.rs`**: HTTP client for the foundation primary aggregator API. Normalizes response data to internal models. (CoinMarketCap is future work, not part of the foundation — see SPEC-PROV-001 Exclusions.)
- **`binance.rs` / `coinbase.rs` / `kraken.rs`**: Optional clients for centralized-exchange fallback data (trading pairs, higher-fidelity timestamps).
- **`chain.rs`**: Orchestrates provider fallback: try primary aggregator, on failure or stale data, attempt exchange fallback, gracefully degrade if all fail.

**`collectors/`**
- **`live_poller.rs`**: Spawned background task; polls primary provider at configurable interval (e.g., 30s). Inserts or updates quotes in DB.
- **`collection_queue.rs`**: Durable work queue for historical data collection. Uses `SELECT ... FOR UPDATE SKIP LOCKED` to claim unprocessed work rows, preventing duplicate work across replicas. Implements lease/heartbeat/retry.
- **`backfill.rs`**: Spawned background task; fills gaps in historical candle data. Queries the gaps table, requests missing intervals from providers, and backfills the quotes table.
- **`rollup.rs`**: Network-free materializer that computes and maintains native 1d/1w OHLCV rollups from finer candles; driven by the `collection_queue` `kind='rollup'` dispatch, with chunked full-history backfill and forward-only incremental maintenance via window-reconcile.

**`api/v1/`**
- **`quotes.rs`**: `GET /api/v1/quotes` (list recent snapshots), `GET /api/v1/quotes/:pair` (latest quote for a pair).
- **`candles.rs`**: `GET /api/v1/candles/:pair?interval=1h&cursor=<cursor>&limit=100` (paginated historical OHLCV).
- **`coins.rs`**: `GET /api/v1/coins` (list all coins with supply/valuation), `GET /api/v1/coins/:symbol` (fetch one).
- **`derivatives.rs`**: `GET /api/v1/derivatives?asset=BTC&venue=binance` (funding rates and open interest).

**`health/`**
- Implements two standard Kubernetes health checks:
  - `/healthz/live`: Pod is running (always returns 200).
  - `/healthz/ready`: Connections to PostgreSQL, primary provider, and background workers are healthy.

**`telemetry/`**
- **`tracer.rs`**: Initializes OpenTelemetry OTLP/gRPC exporter, W3C trace propagation, integration with `tower-http` for auto-instrumentation.
- **`logs.rs`**: Configures `tracing-subscriber` with JSON formatting to stdout, controlled by `RUST_LOG` environment variable.

**`metrics/`**
- Prometheus metrics: request count/duration per endpoint, quote insertion latency, provider call latency, worker job duration.
- Exposes `/metrics` endpoint in Prometheus text format.

**`pacer/`**
- Advisory lock–based rate limiter for upstream provider API calls.
- Table tracks per-provider call history and enforces configured rate limits to avoid hitting upstream quotas.
- Used by `live_poller` and `backfill` workers before making external API calls.

## What's Intentionally Dropped from ticker-collector

The following ticker-collector modules/concepts are **not** ported to Crypto Collector:

- **`calendar.rs` / market calendar logic**: Crypto trades 24/7; no holiday calendars, market-halts, or trading-hours concepts.
- **`market.rs` / market configuration**: Ticker-collector models individual stock markets (NYSE, NASDAQ); Crypto Collector has no equivalent concept. Venues (Binance, Coinbase, Kraken) are optional dimensions, not first-class entities.
- **`exchange.rs` / MIC and ISIN machinery**: Market Identifier Code (MIC) is an equities standard; crypto uses informal exchange names. ISIN is not applicable.
- **Market-phase logic** (pre-market, regular, after-hours): Crypto has no phases.
- **Holiday and trading-halts tables**: Not needed for 24/7 markets.

These simplifications make Crypto Collector lighter and more suitable for continuous operation.

## Deliverables

**`api/crypto-collector.yaml`**
- OpenAPI v3.1 specification describing all REST endpoints, request/response schemas, error codes.
- Used for API documentation, client code generation, and integration testing.

**`charts/crypto-collector/`**
- Production-grade Helm chart for Kubernetes deployment.
- Includes: Deployment with resource requests/limits, Service, ConfigMap for config, Secret for DB credentials, optional HPA for auto-scaling.
- Deployed to namespace `finance` on the aarch64 cluster.

**`Dockerfile` and `Dockerfile.aarch64`**
- Dockerfile: Multi-stage Linux x86_64 build.
- Dockerfile.aarch64: Cross-compiled aarch64 (ARM64) build using `cross` crate + prebuilt-binary strategy.
- Images pushed to `registry.helles.farm/crypto-collector:<version>`.

**`Makefile`**
- Targets: `build` (cargo build), `lint` (clippy, rustfmt), `test` (cargo test), `image` (build Docker image), `push` (push to registry), `push-aarch64` (cross-compile and push).
- User commits to `main`; CI/CD pipeline triggered, which runs Makefile targets and deploys Helm chart.

## Design Patterns

### Stateless Replicas and Multi-Replica Coordination
- All state lives in PostgreSQL.
- Replicas coordinate via `FOR UPDATE SKIP LOCKED` work-queue claiming and advisory locks.
- No in-process caches; no Raft or consensus.

### Provider Fallback Chain
- Primary aggregator (CoinGecko) is the default source; CoinMarketCap is future work.
- Configurable fallback to centralized exchanges (Binance/Coinbase/Kraken) for higher-fidelity data or on provider outage.
- Graceful degradation: if all providers fail, service continues serving cached data (read-only mode).

### Time-Series Data and Partitioning
- Quote and candle tables are RANGE-partitioned by month.
- BRIN indexes on timestamp and keyset (cursor) columns for efficient range queries.
- Cursor-based pagination prevents large offset queries.

### Observability
- All incoming requests are traced via OpenTelemetry.
- JSON-formatted logs to stdout (parsed by container orchestration).
- Prometheus metrics at `/metrics` (scraped by monitoring system).

### Worker Isolation
- Background workers (live_poller, backfill, collection_queue) are spawned at startup.
- Lease/heartbeat mechanism prevents zombie workers on replica crash.
- Work-queue claiming ensures no duplicate processing across replicas.
