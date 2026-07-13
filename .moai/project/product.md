# Crypto Collector — Product Definition

## Vision

Crypto Collector is a stateless microservice that continuously aggregates cryptocurrency market data from multiple sources and exposes it via REST API (with optional WebSocket streaming as a future phase). It serves as the crypto sibling to the existing ticker-collector equities service, providing internal teams and analysts with authoritative, continuously-updated pricing, market metadata, and derivatives data across the cryptocurrency asset universe.

## Purpose

Enable real-time and historical cryptocurrency market data consumption across the organization through a scalable, horizontally-deployable microservice. Crypto markets operate continuously (24/7/365), with no trading halts or market-hours concepts. Crypto Collector abstracts the complexity of multi-source data aggregation, provider fallback chains, and rate-limiting orchestration.

## Target Users

- **Internal services**: Portfolio tracking, risk analytics, and trading systems that require real-time crypto pricing and metrics.
- **Data analysts**: Teams building ad-hoc analysis, backtests, and reports requiring historical OHLCV candles, market metadata, and tokenomics data.
- **Infrastructure operators**: Deployment teams managing the Kubernetes cluster who need a stable, scalable, and observable microservice with standard health checks and Prometheus metrics.

## Core Capabilities

### 1. Coin Spot Quotes and OHLCV Candles

- Real-time spot quotes (price, vs_currency, source) for tracked coins via `GET /v1/coins/{coin_id}/quotes/latest` and paginated history via `GET /v1/coins/{coin_id}/quotes`.
- Historical OHLCV (open, high, low, close, volume) candles at multiple intervals (1m, 5m, 15m, 1h, 4h, 1d, 1w) via `GET /v1/coins/{coin_id}/candles` (interval required).
- Binance is the preferred upstream for spot quotes and OHLCV candles (lower latency, no monthly credit cap). CoinGecko retained for coin search and metadata.
- Cursor-based keyset pagination for efficient range queries across historical data. All price/quantity fields serialized as JSON strings (DecimalString) to preserve full precision.
- Per-coin polling cadence (`live_poll_interval`) configurable per coin in H/M/S notation (e.g. "5m", "1h30m"); null = global cadence. Set on registration, adjustable via PATCH, bounds-validated.

### 2. Real-Time WebSocket Streams

- `GET /v1/coins/stream/quotes` — live spot quote push per subscribed coin.
- `GET /v1/coins/stream/candles` — live OHLCV candle push per subscribed (coin, interval) pair.
- Per-connection subscription management via JSON control frames (subscribe/unsubscribe).
- Cross-replica delivery via PostgreSQL LISTEN/NOTIFY — all service replicas push to subscribed clients regardless of which replica collected the data.
- No authentication, no backfill on connect; malformed control frames return an error frame and the connection stays open.

### 3. Market Metadata and Tokenomics
- Coin-level aggregated metadata: official name, symbol, category, description, official links.
- Supply metrics: circulating supply, total supply, maximum supply.
- Market valuation: market capitalization, fully-diluted valuation (FDV), price-to-FDV ratio.
- On-chain presence: contract addresses (if applicable), blockchain references.

### 4. Derived Analytics
- **Bitcoin halving-cycle overlay**: Materialised daily price points normalized against Bitcoin halving cycles. Each point stores the raw daily price and two normalization baselines — one anchored to the halving day (halving day = 1.0) and one anchored to the cycle low (cycle low = 1.0). Both series are plotted against `days_since_halving` to reproduce the shape of Bitbo's "Cycle Repeat" chart entirely from local persisted candles, with no external data source or scraping.
- Recomputed on the periodic collector tick from the persisted `1d` (daily) OHLCV history in `coin_candles`. Keyset-paginated read route at `GET /v1/coins/{coin_id}/cycle-overlay` with optional `?cycle=N`, `?vs_currency`, and `?as_of=<RFC3339>` filters. The `as_of` parameter enables point-in-time historical reads, reconstructing the overlay as it would have appeared at a given timestamp—useful for comparing the projection that was active at that date against the price actuals that followed. Known halving dates treated as block-derived constants; future halvings extend the model via code updates.

### 5. Operational Alarms
- **Alarm Center integration** (SPEC-ALARM-001): Raises and auto-clears alarms to an external Alarm Center for abnormal operational conditions — provider outages, rate-limiting, database unreachability/pool saturation, collection-queue/backfill failures, worker crash-loops, coin staleness, and upsert-failure streaks.
- Periodic reconciler worker maintains desired-state sweep with server-side TTL auto-clear; every active condition is re-raised with `timeoutSeconds = ALARM_TTL_SECS`, and recovered conditions simply stop being refreshed so the Alarm Center auto-expires them once the TTL lapses.
- Best-effort delivery with bounded retry; alarm center outages do not degrade collector performance.
- Feature gated on `ALARM_CENTER_URL` (unset = fully disabled, no requests to alarm center).
- Operator-facing documentation at `docs/alarms.md` enumerates all 14 alarm types with fingerprint, severity, component, active-signal, clear trigger, thresholds, and remediation guidance.

## Out of Scope (Explicit Non-Goals)

The following are explicitly **not** in scope and are reserved for future work or external systems:

- **On-chain / network metrics**: Active addresses, hashrate, total value locked (TVL), staking yield, validator counts. These require blockchain RPC access and are a separate concern.
- **Market-hours and trading calendars**: Crypto trades continuously; no exchange calendars, market-halts, or trading-phase concepts. (This machinery exists in ticker-collector for equities; it is intentionally removed here.)
- **Sentiment analysis**: Social media sentiment, whale watching, or on-chain behavior tracking.
- **Decentralized exchange (DEX) data**: Order books from DEXes; liquidity pool composition; governance token dynamics. DEX integration is a future phase.
- **News and announcements**: Parsing press releases or news feeds. Orthogonal to market data aggregation.

## Data Sources and Provider Strategy

### Primary Aggregator
- **CoinGecko API** is the primary data source for coin search and metadata. Binance is the preferred upstream for live spot quotes and OHLCV candles (no monthly credit cap). **CoinMarketCap API** is future work — a second-provider integration with its own rate-limit/credit model, deliberately deferred out of the foundation scope (see SPEC-PROV-001 Exclusions).
- Aggregators are preferred for metadata because they normalize data across venues and provide broad asset coverage. Binance is preferred for quotes/candles because it has no monthly credit cap and lower latency.

### Optional Fallback Chain
- Centralized exchange APIs (**Binance**, **Coinbase**, **Kraken**) as optional secondary sources for higher-fidelity trading pair data (tighter bid-ask spreads, more granular timestamp precision).
- Fallback is explicit and configurable (can be disabled to keep the service aggregator-only).
- This mirrors the provider-chain pattern in ticker-collector.

## Statelessness and Scalability

- **Stateless design**: No in-process caches, no memory-resident state. All state persists in PostgreSQL.
- **Horizontal scalability**: Deploy N identical replicas across the Kubernetes cluster. Replicas coordinate via PostgreSQL-native mechanisms (advisory locks, `FOR UPDATE SKIP LOCKED` work-queue claiming) to avoid duplicate work and contention.
- **No distributed consensus**: Multi-replica safety is achieved through PostgreSQL transactions, not Raft, Paxos, or consensus algorithms.
- **24/7 operation**: No downtime windows, no market-calendar-driven shutdown logic. Service runs continuously.

## Success Criteria

- **Data availability**: ≥99.5% uptime in production (Kubernetes namespace `finance`).
- **Freshness**: Price data updated within 30 seconds of aggregator refresh; historical candle data complete within 24 hours.
- **Accuracy**: Crypto pair prices match upstream aggregator ±0.1% under normal network conditions.
- **Scalability**: Support ≥50 concurrent API consumers without degradation; support horizontal scaling to 5+ replicas.
- **Observability**: 100% of API requests traced via OpenTelemetry; Prometheus metrics exposed at `/metrics`; structured JSON logs in stdout.
- **No data loss**: All collected data persisted to PostgreSQL with transactional guarantees; no in-flight request loss on replica crash.
