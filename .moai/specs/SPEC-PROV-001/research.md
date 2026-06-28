# Research — Crypto Collector Foundation

Cross-cutting research backing the Crypto Collector foundation SPEC suite
(SPEC-DB-001, SPEC-PROV-001, SPEC-SCHED-001, SPEC-API-001, SPEC-OBS-001,
SPEC-DEPLOY-001). This is the single authoritative research document; each SPEC's
"Research" link points here.

Authoritative inputs: `.moai/project/product.md`, `.moai/project/structure.md`,
`.moai/project/tech.md`, and the sibling equities service `ticker-collector`
(read for proven patterns, adapted — never copied verbatim).

---

## 1. Crypto market-data domain analysis (how it differs from equities)

Crypto Collector is the crypto sibling of `ticker-collector`. The two share an
architecture (stateless Rust service, PostgreSQL-only state, provider-chain
collection, partitioned time-series tables, multi-replica SKIP-LOCKED workers) but
the **market domain differs in five structural ways** that drive the design.

### 1.1 Continuous (24/7/365) markets → no calendar machinery

Equities trade in sessions bounded by exchange calendars: opening hours, holidays,
half-days, pre-market / regular / after-hours phases, and trading halts.
`ticker-collector` carries substantial machinery for this: `src/calendar.rs`, the
`holidays` crate (every-country holiday tables), `chrono-tz`, the ISO 10383 MIC
registry, `market.rs`, `exchange.rs`, `market_phase` gating, `suspended_at`,
`market_close_grace_seconds`, and a `market_fallback_behavior` knob.

Crypto markets never close. **All of that is deliberately dropped.** There are no
exchange calendars, no MIC/ISIN, no market phases, no trading-halt tables, and no
close-grace window. The live-quote poller runs continuously and the SPEC-RT-001-style
"is the market open?" gate is removed entirely. This is the single largest
simplification versus the equities service and is reflected in the absence of any
`REQ-*-calendar` requirement and the explicit Exclusions in every SPEC.

Consequence: the live poller's claim predicate is purely cadence-driven
(`last_polled_at + interval <= now()`), with no market-open precondition.

### 1.2 The instrument is a base/quote pair, not a single symbol

An equity is identified by one symbol on one exchange (`AAPL` on `XNAS`). A crypto
instrument is a **pair**: a base asset quoted in a quote asset (`BTC/USD`,
`ETH/USDT`, `SOL/BTC`). The same base trades against many quotes, and the "price of
Bitcoin" is meaningless without a quote currency. Additionally the same pair trades
on many **venues** (Binance, Coinbase, Kraken) at slightly different prices.

The model therefore needs a `(base, quote, venue?)` market identity where `venue` is
**optional**: a NULL venue means the aggregator-normalised (cross-venue) price from
CoinGecko; a non-NULL venue means a specific exchange's order book. This is
fundamentally richer than the equities one-symbol-one-exchange model and is why a
dedicated `tracked_markets` registry exists rather than reusing the `tickers` shape.

### 1.3 Coin-level aggregates are keyed by asset, not by pair

Market capitalisation, circulating/total/max supply, and fully-diluted valuation
(FDV) are properties of a **coin** (the base asset), not of any one pair. "Bitcoin's
market cap" is a single number across all pairs and venues. So the data model has two
distinct keys:

- **Coin-keyed** (`coin_id`, e.g. CoinGecko `"bitcoin"`): metadata, supply,
  market cap, FDV, categories, links.
- **Pair-keyed** (`market_id` = base/quote/venue): spot quotes, OHLCV candles,
  derivatives.

This is the crypto analogue of the equities split between company fundamentals
(per-issuer) and quotes (per-symbol), but the keys are genuinely different
namespaces, requiring two registries.

### 1.4 Derivatives are a first-class, continuously-changing domain

Equities derivatives in `ticker-collector` are option chains (snapshot-as-of reads).
Crypto derivatives are **perpetual swaps and futures** with three continuously-moving
observables that have no equities analogue:

- **Funding rate** — the periodic payment between long and short holders of a
  perpetual that tethers its price to spot. Changes every funding interval (typically
  1h or 8h) and can be negative.
- **Open interest** — total notional value of open positions; a liquidity/leverage
  signal.
- **Mark vs index price** and **basis** — the exchange's mark price versus the broad
  index average, and their difference.

CoinGecko's `/derivatives/tickers` returns all of these together per derivative
ticker (see §2.3), so they are captured as one time-series tick per
`(derivative market, ts)` rather than as separate funding / OI tables.

### 1.5 Precision requirements are far stricter than equities

`ticker-collector` stores prices as PostgreSQL `DOUBLE PRECISION` (IEEE-754 `f64`) —
adequate for equities priced in the $1–$10,000 range with cent granularity. Crypto
breaks `f64` at both ends of the range:

- **Tiny prices:** micro-cap tokens trade at `$0.00000000001` (1e-11) and below.
  `f64`'s ~15–17 significant digits silently lose precision when such a price is
  multiplied by a large supply.
- **Huge supplies:** SHIB total supply is ≈ 5.89 × 10¹⁴ tokens; some tokens have
  10¹⁸+ base units. Market cap = price × supply must stay exact for reconciliation.

This forces a **decimal** representation end to end (PostgreSQL `NUMERIC`, Rust
`rust_decimal::Decimal`) instead of `f64`. This is the most important divergence from
`ticker-collector`'s storage types and is a hard requirement (REQ-DB-040, NFR
precision). See §4.5 and §3.4.

---

## 2. Provider analysis

### 2.1 Aggregator-first strategy (CoinGecko primary)

The product mandate is **aggregator-first**: CoinGecko is the primary provider for
all three domains (spot, metadata/tokenomics, derivatives), with an **optional,
ordered fallback chain** to centralized exchanges (Binance / Coinbase / Kraken) for
higher-fidelity spot and derivatives data. This mirrors `ticker-collector`'s
ordered, env-configurable provider chain (`QUOTE_PROVIDERS` → `build_chain`,
fail-fast on unknown names, declared order = fallback priority).

Rationale for aggregator-first: a single CoinGecko integration covers the full asset
universe and all three domains with normalised cross-venue data, minimal auth, and
graceful handling of stale/missing venue data — exactly the properties an internal
data service wants by default. Exchanges are opt-in for cases needing tighter spreads
or exchange-specific OHLCV with volume.

### 2.2 CoinGecko API capabilities

Verified from `docs.coingecko.com` (2026-06-28). CoinGecko V3 covers all three
in-scope domains:

| Domain | Endpoint(s) | Notes |
|---|---|---|
| Spot price (live) | `GET /simple/price` | Many coins × many vs_currencies in one call; cheap. |
| Spot + supply + cap + FDV (bulk) | `GET /coins/markets` | Per-coin price, market_cap, circulating/total/max supply, FDV, 24h volume. Bulk, paginated. |
| Coin metadata | `GET /coins/{id}` | Name, symbol, categories, description, homepage/social links, contract addresses, plus market data. |
| OHLC candles | `GET /coins/{id}/ohlc?vs_currency&days` | `days` ∈ {1,7,14,30,90,180,365,max}. Auto granularity: 30m (1–2d), 4h (3–30d), 4d (31d+). Paid plans: `interval=daily\|hourly`. **Returns OHLC only — no volume.** |
| OHLC by range | `GET /coins/{id}/ohlc/range` | Timestamp range. **Analyst plan and above.** |
| Price/cap/volume series | `GET /coins/{id}/market_chart[/range]` | Time-series price, market cap, **and 24h volume**. Volume source when candle volume is required. |
| Supply breakdown | `GET /coins/{id}/supply_breakdown` | Analyst+. |
| Derivatives tickers | `GET /derivatives/tickers` | funding_rate, open_interest, index, price (mark), basis, spread, volume_24h, contract_type, last_traded_at, expired_at. |
| Derivatives exchanges | `GET /derivatives/exchanges` | Per-exchange open interest and volume. |

**Critical nuance — OHLC has no volume.** `/coins/{id}/ohlc` returns
`[time, open, high, low, close]` with **no volume field**. True OHLC**V** requires
either correlating `/market_chart` 24h-volume series (coarse, not per-candle) or
falling back to an exchange's native kline endpoint (per-candle volume). This is a
primary justification for the exchange fallback chain and for making the candle
`volume` column **nullable** with a `source`/provenance marker (REQ-DB-022,
REQ-PROV-031).

### 2.3 CoinGecko rate limits and tiers

| Plan | Rate limit | Monthly credits | Base URL / key header |
|---|---|---|---|
| Demo (free) | ~30–100 calls/min | **10,000 / month** | `api.coingecko.com`, header `x-cg-demo-api-key` |
| Basic | 300 calls/min | 100,000 / month | `pro-api.coingecko.com`, header `x-cg-pro-api-key` |
| Analyst | 500 calls/min | 500,000 / month | `pro-api.coingecko.com` |
| Lite | 500 calls/min | 2,000,000 / month | `pro-api.coingecko.com` |
| Enterprise | custom | custom | custom |

Two distinct limits apply: a **per-minute** ceiling AND a **monthly credit** budget.
For the free Demo tier the **monthly credit (10k) is the binding constraint** — at one
call every 30s a single endpoint loop would exhaust 10k in ~3.5 days. The pacer must
therefore enforce both a per-minute min-gap **and** a monthly-credit budget, and the
collection cadence defaults must be chosen against the configured tier. The base URL
and key header **differ between Demo and Pro**, so both are env-configurable
(`COINGECKO_BASE_URL`, `COINGECKO_TIER`, `COINGECKO_API_KEY`). These strict, dual
limits are why the pacer is per-provider and credit-aware (§4.4, an improvement over
`ticker-collector`'s single-row, gap-only pacer).

### 2.4 Candidate exchange APIs (fallback)

| Exchange | Public spot | Public derivatives | Auth for public reads | Notes |
|---|---|---|---|---|
| **Binance** | `/api/v3/klines`, `/ticker` | `/fapi/v1/*` (funding, OI, premiumIndex) | none | Largest volume; per-candle kline **with volume**; rich perp data. Weight-based rate limits. |
| **Coinbase** | `/products/{id}/candles`, `/ticker` | limited | none for public | US/EU regulated; strong USD pairs. |
| **Kraken** | `/0/public/OHLC`, `/0/public/Ticker` | futures API separate | none for public | EUR liquidity; counter-based call limits. |

Exchanges provide the per-candle **volume** that CoinGecko OHLC lacks, plus tighter
spreads and finer timestamps — the higher-fidelity rationale. Each exchange has a
**distinct rate-limit model** (Binance weight, Kraken counter), reinforcing the
per-provider pacer design.

### 2.5 Recommended provider-chain design

Mirror `ticker-collector`'s abstraction, adapted for three domains:

- A `Provider` trait with capability methods per domain
  (`fetch_spot`, `fetch_ohlc`, `fetch_coin_metadata`, `fetch_coin_market`,
  `fetch_derivatives`). A provider advertises which capabilities it supports; the
  chain skips a provider for a capability it lacks.
- `build_chain(names: &[String], …) -> Result<Vec<Arc<dyn Provider>>>` — fail-fast on
  unknown names (REQ-PROV-002), declared order = fallback priority (REQ-PROV-003),
  exactly like `providers/mod.rs::build_chain`.
- Per-capability fallback: try providers in order until one returns usable data;
  record `collection_requests_total{provider, outcome}`; degrade gracefully if all
  fail (serve last-persisted data read-only).
- Every outbound call first acquires a slot from the **per-provider pacer**
  (§4.4) and a replica-local throttle, preserving the `ticker-collector`
  `yf_request_pacer` + `YfThrottle` two-layer discipline.

Config: `PROVIDERS` (ordered CSV, default `coingecko`), valid names
`coingecko, binance, coinbase, kraken`. Unknown → startup error.

---

## 3. Crate evaluation and recommended dependency list

The user explicitly asked **not** to copy `ticker-collector`'s `Cargo.toml`
verbatim, and to research current best crates — especially numeric precision and a
CoinGecko / exchange client. All versions below were verified against crates.io on
**2026-06-28**.

### 3.1 Recommended dependency list (with versions and rationale)

| Category | Crate | Version | Rationale / divergence from ticker-collector |
|---|---|---|---|
| Async runtime | `tokio` | `1` (1.52) | Same. `features=["full"]`. Battle-tested. |
| Cancellation | `tokio-util` | `0.7` | Same. `CancellationToken` for graceful worker shutdown. |
| Web framework | `axum` | `0.8` | Same. **Drop the `ws` feature** — no WebSocket streaming in foundation scope (REST only). |
| Middleware | `tower`, `tower-http` | `0.5`, **`0.7`** | **Bump `tower-http` 0.6 → 0.7** (latest). `features=["trace","request-id","util"]`. |
| DB | `sqlx` | `0.9` | Same engine. **Add `rust_decimal` feature** for `NUMERIC` ↔ `Decimal` mapping (ticker only used `chrono,uuid`). `features=["runtime-tokio","postgres","chrono","uuid","rust_decimal"]`. |
| HTTP client | `reqwest` | `0.13` | Same (0.13.4). `features=["json","gzip"]`; add `gzip` for large `/coins/markets` payloads. |
| Serialization | `serde`, `serde_json` | `1`, `1` | Same. |
| **Numeric precision** | **`rust_decimal`** | **`1` (1.42)** | **Promoted from a transitive dependency to a first-class, primary type.** All prices/supplies/caps/funding are `Decimal`, stored as `NUMERIC`. See §3.4. |
| Decimal macros | `rust_decimal_macros` | `1` | Ergonomic `dec!()` literals in tests/bounds. New vs ticker. |
| Date/time | `chrono` | `0.4` | Keep `chrono` (sqlx `chrono` feature, `TIMESTAMPTZ`). **Drop `chrono-tz`** — no timezone/calendar logic. |
| UUIDs | `uuid` | `1` | Same — worker/replica instance IDs, request IDs. `features=["v4","serde"]`. |
| Error (app) | `anyhow` | `1` | Same. |
| Error (lib) | `thiserror` | `2` | Same. |
| Logging | `tracing`, `tracing-subscriber` | `0.1`, `0.3` | Same. `features=["env-filter","json"]`. |
| Tracing export | `opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp`, `tracing-opentelemetry` | `0.32`, `0.32`, `0.32`, `0.33` | Same versions. OTLP/gRPC, W3C propagation. |
| Metrics | `metrics`, `metrics-exporter-prometheus` | `0.24`, `0.18` | Same. `http-listener` on :9000. |
| Async traits | `async-trait` | `0.1` | Same — `Provider` trait is async. |
| Base64 | `base64` | `0.22` | Same — keyset cursor encoding. |
| Futures | `futures-util` | `0.3` | Same — `select!`/stream helpers in workers. |

**Dropped from `ticker-collector` (with reason):**

- `yfinance-rs` — equities-only provider; replaced by hand-rolled crypto providers.
- `thirtyfour` (Selenium WebDriver) — no browser-scraping provider in crypto scope.
- `csv` — Stooq CSV provider is equities-only.
- `holidays`, `chrono-tz` — no calendar/timezone logic (§1.1).
- `axum` `ws` feature — no WebSocket streaming in foundation scope.

**Added vs `ticker-collector`:** `rust_decimal_macros`, sqlx `rust_decimal` feature,
reqwest `gzip`, tower-http bump to 0.7. Optional: `governor` `0.10` (see §3.3).

### 3.2 CoinGecko / exchange client crate evaluation (the headline question)

Surveyed crates.io (2026-06-28):

| Crate | Version | Downloads | Last update | Verdict |
|---|---|---|---|---|
| `coingecko` | 1.1.3 | ~32k | 2025-02 | Most mature CoinGecko crate. Covers **public V3 only**; lightly maintained; no Pro derivatives/ohlc-range coverage guaranteed; ties error handling to its own types. |
| `cgko` | 0.1.4 | ~240 | 2026-02 | New, unofficial, tiny adoption — not production-grade. |
| `rust-gecko` | 0.1.0 | ~4.8k | 2022 | Abandoned. |
| `bothan-coingecko` | 0.0.1 | — | 2025-06 | Tied to the Bothan framework; not a general client. |
| `binance` | 0.21.2 | ~214k | 2025-09 | Most mature exchange crate; maintained; covers spot + futures. |
| `binance-rs-async` / others | <13k | — | mixed | Lower adoption / maintenance. |
| `ccxt` (Rust) | 0.1.0 | ~120 | 2025-11 | Multi-exchange CCXT port; very immature (0.1.0). Not viable. |

**Recommendation: hand-roll a `reqwest`-based client per provider** (CoinGecko +
each exchange), behind the `Provider` trait. Rationale:

1. **No mature, complete CoinGecko crate exists.** The best (`coingecko` 1.1.3) is
   public-V3-only and lightly maintained; we need Pro base-URL/key switching,
   `/ohlc/range`, and `/derivatives/tickers`. A thin hand-rolled client gives full
   control of exactly the ~8 endpoints we use, the dual Demo/Pro auth, and direct
   deserialization into our `Decimal`-typed internal models.
2. **Consistency over convenience.** Mixing a CoinGecko crate with the `binance`
   crate and hand-rolled Coinbase/Kraken clients would mean three different error
   models, retry styles, and maintenance cadences behind one trait. Hand-rolling all
   providers on one `reqwest` client gives uniform pacing, error mapping, tracing,
   and `Decimal` parsing — exactly the pattern `ticker-collector` already proves with
   its hand-rolled `yahoo_v8` / `stooq` providers.
3. **Surface is small and stable.** The endpoints we consume are few and rarely
   change; the maintenance cost of a thin client is lower than tracking a third-party
   crate's breaking changes and coverage gaps.
4. **Precision control.** Hand parsing lets us deserialize numeric fields straight to
   `rust_decimal::Decimal` (via `serde`) and reject lossy `f64` paths.

Escape hatch (documented, not adopted now): the `binance` crate (0.21, well
maintained) is a reasonable opt-in for the Binance provider specifically if
development velocity is later prioritised over uniformity — it sits behind the same
`Provider` trait, so adopting it is a localized change.

### 3.3 Rate limiting: DB pacer primary, `governor` optional

`ticker-collector` uses a replica-local throttle (`YfThrottle`) plus a fleet-wide
single-row DB pacer (`yf_request_pacer`). Crypto keeps both layers but:

- The **DB pacer is per-provider and credit-aware** (§4.4) — mandatory shared egress
  infrastructure, the source of truth across replicas.
- A replica-local token bucket can be hand-rolled (as ticker does) or use
  **`governor` 0.10** (a mature, well-adopted rate-limiter crate). `governor` is
  **optional**: it only smooths bursts *within* a replica; cross-replica correctness
  comes from the DB pacer. Recommendation: start hand-rolled for parity, adopt
  `governor` only if local burst-shaping proves fiddly.

### 3.4 Numeric precision decision (rust_decimal vs alternatives)

| Option | Pros | Cons | Verdict |
|---|---|---|---|
| `f64` (ticker's choice) | simple, fast, sqlx-native | **lossy** for tiny prices / huge supplies; unacceptable for crypto reconciliation | Rejected (§1.5). |
| **`rust_decimal::Decimal`** | 96-bit mantissa, 28–29 significant digits, exact base-10, sqlx `NUMERIC` mapping, `serde` support, fast | scale capped at 28 decimal places; magnitude capped ~7.9 × 10²⁸ | **Recommended.** |
| `bigdecimal` | arbitrary precision/scale | slower, less ergonomic, larger | Fallback only if a specific asset exceeds `Decimal`'s 28-digit envelope. |

`rust_decimal`'s envelope (28–29 significant digits, magnitude to ~10²⁸)
**comfortably covers** crypto: a price of 1 × 10⁻¹⁸ and a supply of 1 × 10¹⁸ are each
far inside range, and their product (market cap) is ~1 with full precision. Storage is
PostgreSQL `NUMERIC` (arbitrary precision in the DB regardless of the Rust type), so
the column never loses data even if the Rust type is later swapped for `bigdecimal`.
**Decision: `rust_decimal` end-to-end, `NUMERIC` columns, `f64` prohibited for
monetary values** (REQ-DB-040). `bigdecimal` is the documented escape hatch.

---

## 4. Schema design rationale

### 4.1 Two registries (coin-keyed vs pair-keyed)

Per §1.2–1.3 the domain has two key namespaces, so two registries:

- `tracked_coins (coin_id PK, …)` — the unit for metadata/tokenomics collection.
- `tracked_markets (id PK, base, quote, venue NULL, coin_id FK NULL, …)` — the unit
  for spot/candles/derivatives. Uniqueness on `(base, quote, COALESCE(venue,''))`
  so a NULL venue (aggregator) and a named venue coexist for the same pair.

This is a deliberate divergence from `ticker-collector`'s single `tickers` table; the
crypto domain genuinely has two identity spaces.

### 4.2 Partitioned time-series tables

Reuse the proven `ticker-collector` SPEC-DB-002 pattern (migrations 0008/0009):
RANGE partition by `ts`, one partition per calendar month (UTC), parent-level indexes
inherited by children:

- `btree(<key>, ts DESC)` for key-scoped reads,
- `BRIN(ts)` for large append-ordered multi-key time-range scans.

Applied to four hot tables: `live_quotes`, `candles`, `coin_market_snapshots`,
`derivatives_quotes`. App-side "ensure future partition exists before write" (the
ticker OR-1 note) carries over.

### 4.3 Revisioned (point-in-time) metadata vs time-series market data — a refinement

`ticker-collector`'s revision pattern (`first_seen_at` / `last_seen_at` / `revision`,
incremented only when tracked values change via `IS NOT DISTINCT FROM`; migrations
0005/0010) is ideal for **slowly-changing** values. The product brief lumps
"metadata + tokenomics" together, but these change at very different rates:

- **Slowly-changing** (revision pattern fits): name, symbol, categories, description,
  links, contract addresses, and `max_supply` (fixed for most assets). →
  `coin_metadata` revisioned table.
- **Continuously-changing** (revision pattern is wrong — would create a new revision
  every poll): `market_cap`, `fully_diluted_valuation`, `circulating_supply`,
  `total_supply`, current price. These are time-series. →
  `coin_market_snapshots` partitioned time-series table.

**Decision:** split them. This directly honours the brief's "point-in-time /
revisioned **where values change over time**" while avoiding revision-table churn for
high-frequency aggregates. It is a conscious improvement over a naive single
"tokenomics revisioned" table and is called out as a design decision, not an
accident. (Open question OR-DB-1 records the alternative if the team prefers a single
table.)

### 4.4 Per-provider, credit-aware pacer (improvement over single-row pacer)

`ticker-collector`'s `yf_request_pacer` is a **single row** (one upstream: Yahoo)
with `next_allowed_at` + `min_gap_ms` + `cooldown_until`. Crypto has **multiple
upstreams with different limits** (CoinGecko per-minute + monthly-credit; Binance
weight; Kraken counter), so the pacer is generalised to **one row per provider**:

```
upstream_request_pacer(
  provider TEXT PK, next_allowed_at, min_gap_ms, cooldown_until,
  credit_window_start, credits_used, credit_limit, updated_at)
```

Each outbound call atomically advances `next_allowed_at = GREATEST(now,
next_allowed_at) + min_gap` and increments `credits_used`, honouring both the
per-minute gap and the monthly credit budget (§2.3). This is mandatory shared egress
infrastructure consumed by all three workers, exactly as the ticker pacer is shared
across COLL/RT/BACKFILL.

### 4.5 NUMERIC columns

All monetary/quantity columns are `NUMERIC` (not `DOUBLE PRECISION`): prices, OHLC,
volume, funding_rate, open_interest, mark/index, supply, market_cap, FDV. See §3.4.

### 4.6 Keyset / cursor pagination

Reuse `ticker-collector`'s `/v2` keyset cursor design (`api/v2/cursor.rs`): the cursor
is the ordering-key tuple of the last row, serialized to JSON and base64url-no-pad
encoded — opaque, stable under concurrent appends, O(1) deep (unlike OFFSET). Applied
to all list reads (quotes, candles, market snapshots, derivatives). Since this is
greenfield, **only the keyset cursor exists** — there is no legacy OFFSET `/v1`
cursor to preserve (see §5).

---

## 5. API versioning decision (single /v1 vs ticker's v1+v2)

`ticker-collector` exposes **both** `/v1` (management + early reads, OFFSET cursor)
and `/v2` (keyset-paginated reads). That split is an **evolutionary artifact**: `/v1`
shipped first, and `/v2` was added later to introduce keyset pagination and
partition-pruned reads **without breaking existing `/v1` clients**.

Crypto Collector is **greenfield with no shipped clients**, so there is no `/v1`
contract to preserve. **Decision: a single coherent `/v1`** that uses keyset
pagination from day one for every list endpoint, covering both management
(register/list/get/update/delete/search) and reads (quotes/candles/metadata/market/
derivatives). This avoids carrying a deprecated OFFSET surface that exists in ticker
only for backward compatibility. A future breaking change would introduce `/v2`
then. This is a deliberate, documented divergence (REQ-API-001).

---

## 6. Worker / multi-replica safety rationale

Reuse the proven `ticker-collector` coordination primitives (no new consensus
machinery):

- **`FOR UPDATE SKIP LOCKED` claiming** on `collection_queue` and `backfill_chunks`
  (migrations 0016/0019) — non-blocking, each row claimed by exactly one replica.
- **Lease + heartbeat + attempts** — a claimed row carries `claimed_by`,
  `lease_expires_at`, `heartbeat_at`, `attempts`; a crashed replica's lease expires
  and the row is re-claimable; `attempts` bounds retries before permanent failure.
- **Self-expiring in-flight marker** for the live poller (the SPEC-RT-002 pattern):
  claim sets `live_poll_claimed_until = now()+ttl` inside a short transaction that
  commits **before** any network I/O, separating cross-replica dedup from the
  schedule cursor so a transient fetch failure does not advance the schedule.
- **Idempotent upserts** + per-`(target, kind)` dedup partial-unique indexes — give
  effective exactly-once persistence: re-running a claimed-but-crashed unit re-writes
  the same rows rather than duplicating.

"Exactly-once" here means **exactly-once persistence of each datum**, achieved by
idempotent writes keyed on `(market_id, ts[, interval])` / `(coin_id, revision)`, not
distributed-transaction exactly-once delivery.

---

## 7. Open questions / risks

- **OR-DB-1 (metadata/tokenomics split):** §4.3 splits slow-changing `coin_metadata`
  (revisioned) from fast-changing `coin_market_snapshots` (time-series). If the team
  prefers one revisioned "tokenomics" table, that is the alternative — but it churns
  revisions on every poll. *Recommendation: keep the split.*
- **OR-PROV-1 (exchange client crate):** §3.2 recommends hand-rolled for uniformity;
  the `binance` crate is the documented opt-in. Confirm at run.
- **OR-PROV-2 (CoinGecko tier defaults):** collection cadence defaults depend on the
  configured tier (Demo 10k/month is binding). The pacer rule is normative; only the
  default *numbers* (min-gap, monthly cap, poll interval) are confirmed at run against
  the deployed tier.
- **OR-DB-2 (candle volume provenance):** CoinGecko OHLC lacks volume (§2.2). Decide
  per deployment whether to (a) leave `volume` NULL from CoinGecko, (b) enrich from
  `/market_chart`, or (c) require an exchange provider for volume-bearing candles. The
  schema supports all three (nullable `volume` + `source` column); the policy is a run
  decision.
- **OR-SCHED-1 (per-market vs global poll cadence):** the live poller supports an
  optional per-market interval (SPEC-RT-002 pattern). Default numbers
  (min/max/claim-ttl) confirmed at run.
- **OR-DB-3 (partition automation):** initial monthly partitions are seeded by
  migration; future-month creation (app-side ensure-on-write vs operational cron) is a
  run/ops decision, as in ticker OR-1.
- **OR-DEPLOY-1 (PostgreSQL sizing/retention):** retention window for hot time-series
  and partition-drop policy are deployment decisions, not foundation requirements.
- **Risk — rate-limit exhaustion:** the Demo monthly credit (10k) is easy to exhaust;
  the credit-aware pacer mitigates but cadence defaults must be conservative for free
  tier. **Risk — sqlx offline cache:** `cargo sqlx prepare` must be run against a
  **live** Postgres after every schema change (a known ticker lesson — offline tests
  pass with stale column refs).

---

## 8. Sources

Fetched / verified 2026-06-28:

- CoinGecko API documentation — `https://docs.coingecko.com/` (endpoint overview,
  `/coins/{id}/ohlc` parameters and granularity, `/derivatives/tickers` fields,
  Demo vs Pro authentication and base URLs).
- CoinGecko API pricing — `https://www.coingecko.com/en/api/pricing` (per-plan
  rate limits and monthly credit allocations).
- crates.io registry API — `https://crates.io/api/v1/crates/{crate}` for current
  versions: axum 0.8.9, tokio 1.52.3, sqlx 0.9.0, reqwest 0.13.4, rust_decimal
  1.42.1, chrono 0.4.45, time 0.3.51, opentelemetry / opentelemetry-otlp 0.32.0,
  metrics-exporter-prometheus 0.18.3, tower-http 0.7.0, governor 0.10.4,
  coingecko 1.1.3, cgko 0.1.4, rust-gecko 0.1.0, binance 0.21.2,
  binance-rs-async, ccxt 0.1.0.
- `ticker-collector` source (sibling service, read for patterns, adapted):
  `Cargo.toml`, `src/config.rs`, `src/providers/mod.rs`, `src/api/v2/cursor.rs`,
  `migrations/0001,0008,0009,0010,0016,0017,0019`, `charts/ticker-collector/*`,
  `Makefile`, `Dockerfile`, `Dockerfile.aarch64`.
