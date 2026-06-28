---
id: PROV-001
version: 1.1.0
status: planned
created: 2026-06-28
updated: 2026-06-28
author: dholm
priority: high
issue_number: null
---

# SPEC-PROV-001 — Provider Chain, CoinGecko Client & Upstream Rate-Limit Pacing

Foundation SPEC for upstream data acquisition. Defines the ordered, env-configurable
provider chain; the primary CoinGecko client; optional centralized-exchange fallback
clients; the normalisation contract into internal `Decimal`-typed models; and the
per-provider, credit-aware rate-limit pacer that protects strict upstream quotas.

Schema contract (FROZEN here): [SPEC-DB-001](../SPEC-DB-001/spec.md)
(`upstream_request_pacer`, the data tables written from collected data).
Research (this directory): [research.md](./research.md) (§2 provider analysis,
§3 crate evaluation — including the CoinGecko/exchange client decision).
Consumers: [SPEC-SCHED-001](../SPEC-SCHED-001/spec.md) (workers call the chain).

## HISTORY

- 2026-06-28 (v1.1.0): Stated explicitly that CoinMarketCap is OUT of foundation scope
  (CoinGecko is the foundation primary; CMC is future work) to reconcile the SPEC suite
  with the product/structure docs, which had shown CMC as a co-primary. (audit M1)
- 2026-06-28 (v1.0.0): Initial greenfield provider SPEC. Aggregator-first: CoinGecko
  primary across all three domains, with an optional ordered fallback chain to
  Binance/Coinbase/Kraken. Hand-rolled `reqwest` clients behind a `Provider` trait
  (research §3.2 — no mature/complete CoinGecko crate exists; uniformity beats mixing
  third-party crates). Per-provider, credit-aware DB pacer generalises
  `ticker-collector`'s single-row `yf_request_pacer` to multiple upstreams with
  distinct rate limits (research §2.3, §4.4).

---

## Goal

Acquire spot, OHLC, coin metadata, coin market aggregates, and derivatives data from
CoinGecko (primary) with optional higher-fidelity exchange fallback, normalise every
provider's response into internal `Decimal`-typed models, and enforce both the
per-minute and monthly-credit limits of each upstream — all behind a single ordered,
fail-fast `Provider` chain configured by environment variables.

## Scope

In scope:
- A `Provider` trait with per-domain capability methods and capability advertisement.
- `build_chain(names) -> Result<Vec<Arc<dyn Provider>>>` — ordered, fail-fast on
  unknown names (mirrors `ticker-collector` `providers/mod.rs::build_chain`).
- A hand-rolled CoinGecko client (primary) covering: simple price, `/coins/markets`,
  `/coins/{id}`, `/coins/{id}/ohlc[/range]`, `/coins/{id}/market_chart`,
  `/derivatives/tickers`; with Demo vs Pro base-URL/key switching.
- Optional hand-rolled exchange clients (Binance/Coinbase/Kraken) supplying
  higher-fidelity spot and per-candle-volume OHLCV plus derivative data.
- Normalisation to internal models with `rust_decimal::Decimal` numeric fields.
- The per-provider, credit-aware pacer protocol over `upstream_request_pacer`
  (SPEC-DB-001) plus a replica-local throttle.
- Per-capability fallback semantics and graceful degradation.

Out of scope: see Exclusions. Worker scheduling/claiming is SPEC-SCHED-001; the DB
tables themselves are SPEC-DB-001.

## Decisions Restated (authoritative)

- **D1 — Aggregator-first.** CoinGecko is the primary provider for all three domains;
  exchanges are optional, ordered fallback. (research §2.1)
- **D2 — Hand-rolled clients.** A thin `reqwest` client per provider behind one
  `Provider` trait, not a third-party CoinGecko/exchange crate. (research §3.2)
- **D3 — Ordered, fail-fast chain.** `PROVIDERS` is an ordered CSV; declared order is
  fallback priority; an unknown name is a startup error. (mirrors ticker REQ-COLL-017/018)
- **D4 — `Decimal` everywhere.** Provider responses deserialize into `Decimal` numeric
  fields; no `f64` for monetary values. (research §1.5, §3.4)
- **D5 — Per-provider, credit-aware pacer.** Every outbound call acquires a pacer slot
  honouring per-minute min-gap, cooldown, and monthly credit budget. (research §2.3)
- **D6 — Dual CoinGecko auth.** Demo (`api.coingecko.com`, `x-cg-demo-api-key`) vs Pro
  (`pro-api.coingecko.com`, `x-cg-pro-api-key`) selected by config. (research §2.3)

---

## Design Summary (WHAT, not HOW)

1. **`Provider` trait (async).** Capability methods (each returning normalised models
   or a "not supported" sentinel): `fetch_spot(markets)`, `fetch_ohlc(market,
   interval, range)`, `fetch_coin_metadata(coin_id)`, `fetch_coin_market(coin_id,
   vs_currency)`, `fetch_derivatives(market)`. A `name()` accessor and a
   `supports(capability)` predicate let the chain skip providers lacking a capability.

2. **Chain orchestration.** `build_chain` maps each configured name to a constructed
   provider (fail-fast on unknown). For a given capability+target, the orchestrator
   tries providers in declared order; the first to return usable data wins; failures
   and "not supported" advance to the next provider; the per-attempt outcome is
   recorded as a metric (`collection_requests_total{provider, capability, outcome}` —
   SPEC-OBS-001). If all providers fail, the capability call returns a typed error and
   the system serves last-persisted data (read-only degradation).

3. **CoinGecko client (primary).** A `reqwest` client targeting the configured base
   URL with the correct API-key header for the tier. Endpoints mapped to capabilities:
   - spot → `/simple/price` or `/coins/markets`
   - coin metadata → `/coins/{id}`
   - coin market aggregates (cap/FDV/supply) → `/coins/markets` or `/coins/{id}`
   - OHLC → `/coins/{id}/ohlc` (and `/ohlc/range` on Analyst+); **volume absent** →
     normalised candle `volume = None`, `source = "coingecko"` (research §2.2)
   - derivatives → `/derivatives/tickers` (funding_rate, open_interest, index, price
     as mark, basis)
   Numeric fields deserialize directly into `Decimal`.

4. **Exchange clients (optional fallback).** Hand-rolled `reqwest` clients exposing the
   same `Provider` trait. They supply per-candle-volume OHLCV (e.g. Binance klines),
   tighter spot spreads, and exchange-native derivative data. Each maps its own
   rate-limit model to a pacer row.

5. **Per-provider, credit-aware pacer.** Before any outbound call the caller acquires a
   slot from `upstream_request_pacer` for that provider via an atomic
   `UPDATE … SET next_allowed_at = GREATEST(now(), next_allowed_at) + (min_gap_ms ||
   'ms')::interval, credits_used = credits_used + 1 … WHERE provider = $1 AND
   (cooldown_until IS NULL OR cooldown_until <= now()) AND (credit_limit IS NULL OR
   credits_used < credit_limit) RETURNING next_allowed_at`. The caller waits until the
   returned instant. On HTTP 429 / quota signal the caller sets
   `cooldown_until = now() + cooldown` so every replica on every path pauses. The
   monthly credit window resets when `now() - credit_window_start >= 1 month`. A
   replica-local throttle smooths intra-replica bursts (research §3.3).

6. **Normalisation contract.** Each provider converts its wire format into the shared
   internal models (the structs SPEC-DB-001 defines): spot quote, candle, coin
   metadata, coin market snapshot, derivative quote — all with `Decimal` numbers,
   `DateTime<Utc>` timestamps, and a `source` provider tag.

---

## Requirements (EARS)

### Provider chain

- **REQ-PROV-001** (Ubiquitous): The system shall expose a `Provider` trait with async
  capability methods for spot, OHLC, coin metadata, coin market aggregates, and
  derivatives, plus a `name()` accessor and a capability-support predicate.
- **REQ-PROV-002** (If/Unwanted): If the configured `PROVIDERS` list contains a name
  that does not map to a known provider, then the system shall fail at startup with an
  error naming the offending value and the set of valid names.
- **REQ-PROV-003** (Ubiquitous): The system shall treat the declared `PROVIDERS` order
  as fallback priority, attempting providers first-named-first for each capability.
- **REQ-PROV-004** (Event-Driven): When a provider returns an error or reports a
  capability as unsupported for a target, the system shall advance to the next provider
  in the chain for that capability.
- **REQ-PROV-005** (If/State-Driven): If every provider in the chain fails for a
  read-serving datum, then the system shall serve the last-persisted value (read-only
  degradation) rather than erroring the API, and shall record the failure.
- **REQ-PROV-006** (Ubiquitous): The system shall record each outbound attempt's
  provider, capability, and outcome (success/failure/unsupported) as a metric
  consumable by SPEC-OBS-001.

### CoinGecko client (primary)

- **REQ-PROV-010** (Ubiquitous): The system shall provide a CoinGecko provider that
  covers spot price, coin market aggregates (market cap, FDV, circulating/total/max
  supply), coin metadata, OHLC candles, and derivatives (funding rate, open interest,
  index, mark price, basis).
- **REQ-PROV-011** (State-Driven): While the configured CoinGecko tier is Demo, the
  client shall target the Demo base URL and send the Demo API-key header; while the
  tier is Pro/paid, it shall target the Pro base URL and send the Pro API-key header.
- **REQ-PROV-012** (Ubiquitous): The CoinGecko client shall deserialize every numeric
  price, supply, market-cap, FDV, funding-rate, open-interest, mark/index, and basis
  field into `rust_decimal::Decimal`, never into `f64`.
- **REQ-PROV-013** (Ubiquitous): When mapping a CoinGecko OHLC response (which carries
  no volume), the system shall normalise the candle with `volume = None` and a
  `source` tag identifying CoinGecko, so the absence of volume is explicit rather than
  zero.
- **REQ-PROV-014** (Where feature exists): Where the configured tier permits, the
  client shall use the range-bounded OHLC endpoint for historical backfill; where it
  does not, it shall use the day-bucketed OHLC endpoint and the chain shall surface the
  granularity limitation rather than fabricating finer candles.

### Exchange fallback clients (optional)

- **REQ-PROV-020** (Where feature exists): Where an exchange provider is configured in
  the chain, the system shall expose it through the same `Provider` trait and supply,
  at minimum, spot quotes and per-candle-volume OHLCV for the venue.
- **REQ-PROV-021** (Ubiquitous): Each exchange provider shall map its venue identity
  onto the `tracked_markets.venue` dimension so that venue-specific data is stored
  distinctly from aggregator (NULL-venue) data.
- **REQ-PROV-022** (Ubiquitous): Each exchange provider shall normalise responses into
  the same internal `Decimal`-typed models as the CoinGecko provider.

### Normalisation

- **REQ-PROV-030** (Ubiquitous): Every provider shall normalise its wire format into
  the shared internal models for spot quote, candle, coin metadata, coin market
  snapshot, and derivative quote, tagging each record with its `source` provider name.
- **REQ-PROV-031** (Ubiquitous): The candle model shall carry a nullable `volume` and a
  `source` tag so that volume-bearing (exchange) and volume-absent (CoinGecko) candles
  are distinguishable downstream.
- **REQ-PROV-032** (Ubiquitous): All timestamps produced by providers shall be UTC
  `DateTime<Utc>`; providers shall convert epoch/seconds/millis inputs to UTC.

### Pacing and quota compliance

- **REQ-PROV-040** (Ubiquitous): Before any outbound provider HTTP request, the caller
  shall acquire a slot from `upstream_request_pacer` for that provider, advancing
  `next_allowed_at` by the provider's `min_gap_ms` and incrementing `credits_used`,
  atomically.
- **REQ-PROV-041** (State-Driven): While a provider's pacer `cooldown_until` is in the
  future, the system shall not issue outbound requests to that provider on any replica
  or path until the cooldown expires.
- **REQ-PROV-042** (Event-Driven): When a provider returns HTTP 429 or an equivalent
  rate-limit / quota-exceeded signal, the system shall set that provider's
  `cooldown_until` to `now() + configured cooldown`.
- **REQ-PROV-043** (State-Driven): While a provider has a non-NULL monthly
  `credit_limit` and `credits_used` has reached it within the current credit window,
  the system shall withhold further outbound requests to that provider until the
  credit window resets.
- **REQ-PROV-044** (Ubiquitous): The credit window shall reset (`credits_used` to 0,
  `credit_window_start` to now) once the elapsed time since `credit_window_start`
  reaches the configured monthly window.
- **REQ-PROV-045** (Ubiquitous): The pacer protocol shall be the single source of
  fleet-wide egress control; no provider client shall bypass it, and no second pacing
  table shall be introduced.

## Exclusions (What NOT to Build)

- **No CoinMarketCap provider** in the foundation — CoinGecko is the foundation primary
  aggregator across all three domains. CoinMarketCap is deliberately deferred to future
  work: it is a second-provider integration with a distinct rate-limit/credit model
  (e.g. a daily/monthly call quota rather than CoinGecko's per-minute + credit shape),
  and adding it now would double the client surface and the pacer's quota modelling for
  no foundation benefit. Adding CMC is a follow-up SPEC, not part of this one.
- **No third-party CoinGecko/exchange SDK adoption** in the foundation — clients are
  hand-rolled behind the `Provider` trait (research §3.2). The `binance` crate is a
  documented future opt-in (OR-PROV-1), not part of this SPEC.
- **No `f64` for monetary values** anywhere in provider code (REQ-PROV-012).
- **No on-chain, sentiment, social, news, or DEX/order-book-depth providers** — out of
  product scope (research §1; product non-goals).
- **No worker/scheduler logic** — claiming, cadence, lease/heartbeat are SPEC-SCHED-001.
  This SPEC defines only the chain, clients, normalisation, and pacing protocol.
- **No second pacing mechanism** — the per-provider `upstream_request_pacer` is the
  only egress governor (REQ-PROV-045).
- **No fabricated candle granularity** — when an endpoint only offers coarse buckets,
  the limitation is surfaced, not interpolated (REQ-PROV-014).
- **No WebSocket/streaming ingestion** — REST polling only in foundation scope.

## @MX Annotation Targets (high fan_in)

- `build_chain` — `@MX:ANCHOR` (every worker depends on its ordering + fail-fast
  contract) + `@MX:NOTE` on valid provider names.
- The pacer acquire/cooldown SQL — `@MX:WARN`/`@MX:REASON`: this is the single
  fleet-wide egress governor; all outbound calls route through it; bypass risks upstream
  bans / quota exhaustion (REQ-PROV-040/045).
- The CoinGecko OHLC→candle normaliser — `@MX:NOTE` that volume is intentionally
  `None` for CoinGecko (research §2.2, REQ-PROV-013).
- The `Provider` trait — `@MX:ANCHOR` (the cross-provider contract).

## Open Items (do not guess)

- **OR-PROV-1:** hand-rolled Binance client vs the `binance` crate (0.21). Recommend
  hand-rolled for uniformity (research §3.2); the crate is the documented opt-in. Run.
- **OR-PROV-2:** CoinGecko tier defaults (min-gap, monthly credit cap, which endpoints
  per tier). Rules are normative (REQ-PROV-040..044); default numbers confirmed at run
  against the deployed tier.
- **OR-PROV-3:** which exchanges to enable by default (recommend `coingecko` only by
  default; exchanges opt-in). Run/ops decision.
- **OR-PROV-4 (= OR-DB-2):** candle volume policy when CoinGecko-only (NULL vs enrich
  from `/market_chart` vs require exchange). Schema + normaliser support all; policy is
  a run decision.
