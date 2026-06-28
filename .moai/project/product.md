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

### 1. Spot Quotes and OHLCV Candles
- Real-time bid/ask snapshots and last-traded prices for cryptocurrency market pairs (e.g., BTC/USD, ETH/USDT).
- Historical OHLCV (open, high, low, close, volume) candles at multiple time intervals (1m, 5m, 15m, 1h, 4h, 1d, 1w, 1M).
- Markets modeled as base/quote currency pairs with optional venue/exchange dimension for higher-fidelity trading venues (Binance, Coinbase, Kraken).
- Cursor-based pagination for efficient range queries across historical data.

### 2. Market Metadata and Tokenomics
- Coin-level aggregated metadata: official name, symbol, category, description, official links.
- Supply metrics: circulating supply, total supply, maximum supply.
- Market valuation: market capitalization, fully-diluted valuation (FDV), price-to-FDV ratio.
- On-chain presence: contract addresses (if applicable), blockchain references.

### 3. Derivatives Data
- Perpetual and futures funding rates (borrowed cost of maintaining open positions).
- Open interest (total notional value of open positions), stored as per-venue ticks (`derivatives_quotes` keyed by `(market_id, ts, venue)`); any cross-exchange aggregation is a deferred query-time concern, not a stored cross-venue total.
- Mark and index price observations (exchange-reported mark price vs. broad index average).
- Aggregated at venue level where applicable.

## Out of Scope (Explicit Non-Goals)

The following are explicitly **not** in scope and are reserved for future work or external systems:

- **On-chain / network metrics**: Active addresses, hashrate, total value locked (TVL), staking yield, validator counts. These require blockchain RPC access and are a separate concern.
- **Market-hours and trading calendars**: Crypto trades continuously; no exchange calendars, market-halts, or trading-phase concepts. (This machinery exists in ticker-collector for equities; it is intentionally removed here.)
- **Sentiment analysis**: Social media sentiment, whale watching, or on-chain behavior tracking.
- **Decentralized exchange (DEX) data**: Order books from DEXes; liquidity pool composition; governance token dynamics. DEX integration is a future phase.
- **News and announcements**: Parsing press releases or news feeds. Orthogonal to market data aggregation.
- **WebSocket streaming push**: Real-time push of quotes/candles/derivatives over WebSocket. The foundation is REST-polling only; streaming output is a future phase (all foundation SPECs — DB, PROV, SCHED, API, OBS — explicitly defer it).

## Data Sources and Provider Strategy

### Primary Aggregator
- **CoinGecko API** is the primary (foundation) data source for prices, market metadata, supply, and derivatives data. **CoinMarketCap API** is future work — a second-provider integration with its own rate-limit/credit model, deliberately deferred out of the foundation scope (see SPEC-PROV-001 Exclusions).
- Aggregators are preferred because they normalize data across venues, provide broad asset coverage, and handle stale/missing venue data gracefully.

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
