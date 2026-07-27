//! Provider trait, capability enum, and chain builder (SPEC-PROV-001).
//!
//! The chain is ordered (declared order = fallback priority) and fail-fast on unknown names.
//! Mirrors `ticker-collector`'s `providers/mod.rs::build_chain` pattern (research §2.5).
//!
//! ## Shared request path (SPEC-PROV-002)
//!
//! Every provider endpoint routes through the [`transport`] module — the single
//! request-path frame that removes the per-endpoint duplication which produced this class
//! of drift (F-14). [`transport::build_client`] is the ONLY place a provider
//! `reqwest::Client` is constructed: it applies a total-request timeout (default 30 s,
//! `PROVIDER_HTTP_TIMEOUT_SECS`), a shorter connect timeout (default 10 s,
//! `PROVIDER_HTTP_CONNECT_TIMEOUT_SECS`), and a `User-Agent`, so no client can hang a
//! worker indefinitely (F-11/F-19). [`transport::paced`] wraps every call with the
//! throttle + `pacer::acquire_slot` prelude and the 429 → `pacer::signal_cooldown`
//! postlude; [`transport::get_json`] is the shared response epilogue.

use anyhow::anyhow;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use sqlx::PgPool;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use thiserror::Error;

pub mod binance;
pub mod bitstamp;
pub mod coinbase;
pub mod coingecko;
pub mod kraken;
pub mod transport;

pub use binance::BinanceProvider;
pub use bitstamp::BitstampProvider;
pub use coinbase::CoinbaseProvider;
pub use coingecko::{CoinGeckoConfig, CoinGeckoProvider};
pub use kraken::KrakenProvider;

// ── Domain types ─────────────────────────────────────────────────────────────

/// Capabilities a provider may or may not support.
///
/// The chain orchestrator calls `provider.supports(cap)` before dispatching;
/// unsupported capabilities advance to the next provider (REQ-PROV-001/004).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    Spot,
    Ohlc,
    /// Date-range-bounded OHLC fetch (`fetch_ohlc_range`). Distinct from `Ohlc` because
    /// not every provider/tier can serve an arbitrary historical window in one call
    /// (e.g. CoinGecko Demo tier only exposes the "most recent N days" `/ohlc` endpoint).
    OhlcRange,
    CoinMetadata,
    CoinMarket,
    Derivatives,
}

/// Context for market-level provider calls (SPEC-REFACTOR-001 M6, F-56 keyed query).
///
/// Keyed either by CoinGecko `coin_id` (every collection path) or by an internal
/// market-registry id. The `CoinKeyed` variant structurally carries NO `market_id`: the
/// coin-keyed collectors (live_poller, collection_queue, backfill) re-key the produced rows by
/// `coin_id` and discard the model's `market_id`, so the old `market_id: 0 /* dummy */` sentinel
/// is unrepresentable (REQ-REFACTOR-063). Providers read the shared fields through the accessor
/// methods below rather than a possibly-dummy `market_id` field.
#[derive(Debug, Clone)]
pub enum MarketQuery {
    /// Coin-keyed context (CoinGecko `coin_id` path) — carries no market-registry id.
    CoinKeyed {
        /// CoinGecko coin identifier (e.g. `"bitcoin"`).
        coin_id: String,
        /// Base asset symbol (e.g. `"BTC"`).
        symbol: String,
        /// Quote asset symbol (e.g. `"USDT"`).
        quote: String,
        /// Price vs-currency (e.g. `"usd"`).
        vs_currency: String,
    },
    /// Market-keyed context — identified by an internal market-registry id.
    MarketKeyed {
        /// Internal market registry ID (used to tag normalised models).
        market_id: i64,
        /// CoinGecko coin identifier (e.g. `"bitcoin"`); `None` for exchange-only providers.
        coin_id: Option<String>,
        /// Base asset symbol (e.g. `"BTC"`).
        base: String,
        /// Quote asset symbol (e.g. `"USDT"`).
        quote: String,
        /// Trading venue (e.g. `"binance"`); `None` = aggregator/CoinGecko source.
        venue: Option<String>,
        /// Price vs-currency (e.g. `"usd"`).
        vs_currency: String,
    },
}

impl MarketQuery {
    /// Market-registry id to stamp on produced models.
    ///
    /// `CoinKeyed` has no registry id and returns `0` — the coin-keyed collectors discard the
    /// produced model's `market_id` (they re-key rows by `coin_id`), so this projection is never
    /// consumed on that path (behavior-preserving vs the former `market_id: 0` dummy).
    pub fn market_id(&self) -> i64 {
        match self {
            MarketQuery::CoinKeyed { .. } => 0,
            MarketQuery::MarketKeyed { market_id, .. } => *market_id,
        }
    }

    /// CoinGecko coin identifier, if this query carries one.
    pub fn coin_id(&self) -> Option<&str> {
        match self {
            MarketQuery::CoinKeyed { coin_id, .. } => Some(coin_id.as_str()),
            MarketQuery::MarketKeyed { coin_id, .. } => coin_id.as_deref(),
        }
    }

    /// Base asset symbol (`symbol` for coin-keyed, `base` for market-keyed).
    pub fn base(&self) -> &str {
        match self {
            MarketQuery::CoinKeyed { symbol, .. } => symbol,
            MarketQuery::MarketKeyed { base, .. } => base,
        }
    }

    /// Quote asset symbol.
    pub fn quote(&self) -> &str {
        match self {
            MarketQuery::CoinKeyed { quote, .. } | MarketQuery::MarketKeyed { quote, .. } => quote,
        }
    }

    /// Trading venue, if any. `CoinKeyed` has none.
    pub fn venue(&self) -> Option<&str> {
        match self {
            MarketQuery::CoinKeyed { .. } => None,
            MarketQuery::MarketKeyed { venue, .. } => venue.as_deref(),
        }
    }

    /// Price vs-currency.
    pub fn vs_currency(&self) -> &str {
        match self {
            MarketQuery::CoinKeyed { vs_currency, .. }
            | MarketQuery::MarketKeyed { vs_currency, .. } => vs_currency,
        }
    }
}

/// Normalised spot quote (provider-level, before DB write).
///
/// Mirrors `models::LiveQuote` but without DB-assigned fields.
#[derive(Debug, Clone)]
pub struct SpotQuote {
    pub market_id: i64,
    pub ts: DateTime<Utc>,
    pub price: Decimal,
    pub bid: Option<Decimal>,
    pub ask: Option<Decimal>,
    pub volume_24h: Option<Decimal>,
    pub vs_currency: String,
    pub source: String,
}

/// Normalised OHLC candle (provider-level).
///
/// `volume` is nullable: CoinGecko `/coins/{id}/ohlc` returns no per-candle volume (REQ-PROV-013/031).
#[derive(Debug, Clone)]
pub struct OhlcCandle {
    pub market_id: i64,
    pub interval: String,
    pub ts: DateTime<Utc>,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    /// `None` for CoinGecko (no volume in OHLC endpoint); `Some` for exchanges.
    pub volume: Option<Decimal>,
    pub vs_currency: String,
    pub source: String,
}

/// Normalised coin metadata (provider-level, before revision tracking).
#[derive(Debug, Clone)]
pub struct CoinMeta {
    pub coin_id: String,
    pub name: String,
    pub symbol: String,
    pub categories: Option<Vec<String>>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub links: Option<serde_json::Value>,
    pub contract_addresses: Option<serde_json::Value>,
    pub max_supply: Option<Decimal>,
    pub genesis_date: Option<chrono::NaiveDate>,
}

/// Normalised coin market snapshot (provider-level).
#[derive(Debug, Clone)]
pub struct CoinMarket {
    pub coin_id: String,
    pub vs_currency: String,
    pub ts: DateTime<Utc>,
    pub price: Decimal,
    pub market_cap: Option<Decimal>,
    pub fully_diluted_valuation: Option<Decimal>,
    pub circulating_supply: Option<Decimal>,
    pub total_supply: Option<Decimal>,
    pub volume_24h: Option<Decimal>,
    pub source: String,
}

/// Coin search result returned from a provider search (SPEC-PROV-001 REQ-PROV-005).
///
/// Shared between the provider layer and the API layer so that
/// `CoinGeckoClient::search_coins` and `GET /v1/coins/search` operate on one type.
#[derive(Debug, Clone, Serialize)]
pub struct CoinSearchResult {
    pub coin_id: String,
    pub symbol: String,
    pub name: String,
}

/// Market search result returned from a provider ticker fetch (SPEC-PROV-001 REQ-PROV-005).
///
/// Shared between the provider layer and the API layer so that
/// `CoinGeckoClient::fetch_coin_tickers` and `GET /v1/markets/search` operate on one type.
/// Fields map to CoinGecko `/coins/{id}/tickers`: base/target/market.identifier.
#[derive(Debug, Clone, Serialize)]
pub struct MarketSearchResult {
    pub base: String,
    pub quote: String,
    pub venue: Option<String>,
}

/// Normalised derivative tick (provider-level).
#[derive(Debug, Clone)]
pub struct DerivTick {
    pub market_id: i64,
    pub ts: DateTime<Utc>,
    pub funding_rate: Option<Decimal>,
    pub open_interest: Option<Decimal>,
    pub open_interest_usd: Option<Decimal>,
    pub mark_price: Option<Decimal>,
    pub index_price: Option<Decimal>,
    pub basis: Option<Decimal>,
    pub volume_24h: Option<Decimal>,
    pub contract_type: Option<String>,
    pub venue: Option<String>,
    pub source: String,
}

// ── Error taxonomy ────────────────────────────────────────────────────────────

/// Provider-level error taxonomy (transient vs permanent, REQ-PROV-004).
#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("capability {0:?} not supported by provider")]
    NotSupported(Capability),

    /// A non-empty provider chain had NO member capable of the requested capability — every
    /// provider was skipped as `Unsupported` (F-26). Distinct from the genuinely-empty-chain
    /// case, and never surfaced as the misleading `"empty provider chain"` label
    /// (REQ-PROV-080).
    #[error("no provider in the chain supports capability {0:?}")]
    NoCapableProvider(Capability),

    #[error("rate limited (HTTP 429) — cooldown required")]
    RateLimited,

    #[error("HTTP error {status}: {body}")]
    Http { status: u16, body: String },

    #[error("credit exhausted — monthly limit reached")]
    CreditExhausted,

    #[error("parse error: {0}")]
    Parse(String),

    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("pacer error: {0}")]
    Pacer(#[from] crate::pacer::AcquireSlotError),

    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

impl ProviderError {
    /// True for transient errors (retry may succeed). False for permanent errors.
    ///
    /// SPEC-SCHED-001 worker retry logic consumes this classification, so a permanent
    /// client error (a 4xx other than `408|425|429`) MUST NOT be classified transient —
    /// otherwise the workers spin retrying a request that can never succeed (F-12).
    ///
    /// `Http { status }` is transient only for `408 | 425 | 429 | 500..=599` (timeouts,
    /// too-early, rate-limit, and all 5xx). `RateLimited` and `Network` remain transient
    /// unchanged (REQ-PROV-058/059).
    pub fn is_transient(&self) -> bool {
        match self {
            ProviderError::RateLimited | ProviderError::Network(_) => true,
            ProviderError::Http { status, .. } => matches!(status, 408 | 425 | 429 | 500..=599),
            _ => false,
        }
    }
}

// ── Outcome recording ─────────────────────────────────────────────────────────

/// Outcome of a single provider attempt (REQ-PROV-006).
///
/// In production, these feed `collection_requests_total{provider,capability,outcome}` (SPEC-OBS-001).
/// In tests, they are collected and asserted directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderOutcome {
    Success,
    Failure,
    Unsupported,
}

/// Record of a single provider attempt for metric emission.
#[derive(Debug, Clone)]
pub struct AttemptRecord {
    pub provider: String,
    pub capability: Capability,
    pub outcome: ProviderOutcome,
}

// ── Provider trait ─────────────────────────────────────────────────────────────

/// Async data-acquisition trait implemented by every provider (REQ-PROV-001).
///
/// Providers normalise responses into shared internal types (`SpotQuote`, `OhlcCandle`, etc.)
/// with `Decimal` numeric fields and UTC timestamps (REQ-PROV-012/030/032).
///
// @MX:ANCHOR: [AUTO] Provider trait — cross-provider contract for all data acquisition
// @MX:REASON: CoinGeckoProvider, BinanceProvider, CoinbaseProvider, KrakenProvider all implement
//             this trait. The chain orchestrator and all workers program against Provider only.
//             Adding/removing methods is a breaking change for all implementations and callers.
//             fan_in >= 3 (chain, workers, tests). REQ-PROV-001.
// @MX:NOTE: [AUTO] Every fetch_* method carries a capability-derived default body
//           (Err(NotSupported(<capability>))); the search pair (search_coins,
//           fetch_coin_tickers) defaults to Ok(vec![]) (Opt-A). Implementors override ONLY
//           the capabilities they genuinely serve; name()/supports() stay mandatory
//           (SPEC-REFACTOR-001 M1, F-50).
// @MX:SPEC: SPEC-PROV-001 REQ-PROV-001/003/004 SPEC-REFACTOR-001 REQ-REFACTOR-010
#[async_trait]
pub trait Provider: Send + Sync {
    /// Provider identifier (e.g. `"coingecko"`, `"binance"`).
    fn name(&self) -> &str;

    /// True if this provider can fulfil the given capability.
    fn supports(&self, cap: Capability) -> bool;

    /// Fetch a live spot quote for the given market.
    ///
    /// Default: `Err(ProviderError::NotSupported(Capability::Spot))` — providers that do
    /// not serve spot quotes rely on this default (SPEC-REFACTOR-001 M1, F-50).
    async fn fetch_spot(&self, _market: &MarketQuery) -> Result<SpotQuote, ProviderError> {
        Err(ProviderError::NotSupported(Capability::Spot))
    }

    /// Fetch OHLC candles. `days` selects the lookback window; `interval_secs` is the
    /// desired candle granularity.
    ///
    /// Each provider snaps `interval_secs` to the nearest granularity it natively supports
    /// and stores that string on every returned `OhlcCandle.interval`.
    ///
    /// CoinGecko note: granularity and lookback are coupled on the free tier — the snapped
    /// granularity overrides the `days` band when they conflict.
    ///
    /// REQ-PROV-013: CoinGecko candles have `volume = None`.
    ///
    /// Default: `Err(ProviderError::NotSupported(Capability::Ohlc))`.
    async fn fetch_ohlc(
        &self,
        _market: &MarketQuery,
        _days: u32,
        _interval_secs: i64,
    ) -> Result<Vec<OhlcCandle>, ProviderError> {
        Err(ProviderError::NotSupported(Capability::Ohlc))
    }

    /// Fetch one page of OHLC candles at-or-after `start` and before `end`, ordered
    /// ascending, capped at the provider's per-call page limit (REQ-PROV-001 backfill).
    ///
    /// Unlike `fetch_ohlc` (which windows relative to "now"), this method targets an
    /// arbitrary historical `[start, end)` range — the primitive multi-year backfill
    /// needs. Callers page through a wide range across repeated calls (see
    /// `collectors::backfill`'s cursor-advance loop); a single call does not need to
    /// return the whole window.
    ///
    /// Default: `Err(ProviderError::NotSupported(Capability::OhlcRange))`. Providers
    /// that cannot serve an arbitrary historical window (stubs, tier-gated CoinGecko
    /// Demo) rely on this default and need no override.
    async fn fetch_ohlc_range(
        &self,
        _market: &MarketQuery,
        _start: DateTime<Utc>,
        _end: DateTime<Utc>,
        _interval_secs: i64,
    ) -> Result<Vec<OhlcCandle>, ProviderError> {
        Err(ProviderError::NotSupported(Capability::OhlcRange))
    }

    /// Fetch slowly-changing coin metadata (descriptions, links, supply cap).
    ///
    /// Default: `Err(ProviderError::NotSupported(Capability::CoinMetadata))`.
    async fn fetch_coin_metadata(&self, _coin_id: &str) -> Result<CoinMeta, ProviderError> {
        Err(ProviderError::NotSupported(Capability::CoinMetadata))
    }

    /// Fetch continuously-changing coin market aggregates (price, cap, supply, FDV).
    ///
    /// Default: `Err(ProviderError::NotSupported(Capability::CoinMarket))`.
    async fn fetch_coin_market(
        &self,
        _coin_id: &str,
        _vs_currency: &str,
    ) -> Result<CoinMarket, ProviderError> {
        Err(ProviderError::NotSupported(Capability::CoinMarket))
    }

    /// Fetch the latest derivative tick (funding rate, OI, mark/index, basis).
    ///
    /// Default: `Err(ProviderError::NotSupported(Capability::Derivatives))`.
    async fn fetch_derivatives(&self, _market: &MarketQuery) -> Result<DerivTick, ProviderError> {
        Err(ProviderError::NotSupported(Capability::Derivatives))
    }

    /// Search for coins by name / symbol (SPEC-PROV-001 REQ-PROV-005).
    ///
    /// Returns up to `cap` results. Providers that do not support coin search return `Ok(vec![])`.
    /// Upstream non-success responses degrade to empty (REQ-PROV-005) and are WARN-logged by the
    /// client; callers should treat `Err` from this method as a network-level failure and may
    /// choose to degrade to empty rather than propagate.
    ///
    /// Default (Opt-A per SPEC-REFACTOR-001 DEC-3): `Ok(vec![])` — a non-directory provider
    /// reports no search results rather than an error, keeping the search pair on this single
    /// `Provider` trait (a separate `CoinDirectory` trait is deferred to SPEC-COINDIR-001).
    async fn search_coins(
        &self,
        _q: &str,
        _cap: usize,
    ) -> Result<Vec<CoinSearchResult>, ProviderError> {
        Ok(vec![])
    }

    /// Fetch trading pairs for a resolved coin ID from the provider (SPEC-PROV-001 REQ-PROV-005).
    ///
    /// Returns up to `cap` results ordered by converted USD volume descending, with stale and
    /// anomaly tickers excluded. Providers that do not support ticker fetching return `Ok(vec![])`.
    /// Upstream non-success responses degrade to empty (REQ-PROV-005) and are WARN-logged by the
    /// client; callers should treat `Err` as a network-level failure and may degrade to empty.
    ///
    /// Default (Opt-A per SPEC-REFACTOR-001 DEC-3): `Ok(vec![])`.
    async fn fetch_coin_tickers(
        &self,
        _coin_id: &str,
        _cap: usize,
    ) -> Result<Vec<MarketSearchResult>, ProviderError> {
        Ok(vec![])
    }
}

// ── Chain builder ─────────────────────────────────────────────────────────────

/// Build the ordered provider chain from a list of names (REQ-PROV-002/003).
///
/// Fails fast if any name is unknown — returns an error naming the offending value
/// and listing all valid names. Declared order equals fallback priority.
///
/// Valid names: `coingecko`, `binance`, `bitstamp`, `coinbase`, `kraken`.
///
/// `bitstamp` is a candle-only provider whose value is deep history: place it AFTER
/// `binance` (e.g. `coingecko,binance,bitstamp`) so Binance serves recent candles and
/// Bitstamp only fills windows Binance cannot (pre-2017-08 daily) — see
/// `chain_fetch_ohlc_range`'s continue-on-empty fallthrough.
///
// @MX:ANCHOR: [AUTO] build_chain — ordered fail-fast provider chain constructor
// @MX:REASON: Every worker and the chain orchestrator depends on this for data acquisition.
//             Startup invariant: unknown name = immediate error (REQ-PROV-002).
//             Declared order IS the fallback priority (REQ-PROV-003).
//             fan_in >= 3: main startup, SPEC-SCHED-001 workers, integration tests.
// @MX:NOTE: [AUTO] Valid provider names: coingecko, binance, bitstamp, coinbase, kraken
// @MX:SPEC: SPEC-PROV-001 REQ-PROV-002/003
pub fn build_chain(
    names: &[String],
    coingecko_config: CoinGeckoConfig,
    pool: PgPool,
) -> anyhow::Result<Vec<Arc<dyn Provider>>> {
    const VALID_NAMES: &[&str] = &["coingecko", "binance", "bitstamp", "coinbase", "kraken"];

    // Fail-fast validation (REQ-PROV-002)
    for name in names {
        if !VALID_NAMES.contains(&name.as_str()) {
            return Err(anyhow!(
                "unknown provider: {name:?}. Valid names: coingecko, binance, bitstamp, coinbase, kraken"
            ));
        }
    }

    let mut chain: Vec<Arc<dyn Provider>> = Vec::with_capacity(names.len());
    for name in names {
        let provider: Arc<dyn Provider> = match name.as_str() {
            "coingecko" => Arc::new(CoinGeckoProvider::new(
                coingecko_config.clone(),
                pool.clone(),
            )),
            "binance" => Arc::new(BinanceProvider::new(None, pool.clone())),
            "bitstamp" => Arc::new(BitstampProvider::new(None, pool.clone())),
            "coinbase" => Arc::new(CoinbaseProvider::new(pool.clone())),
            "kraken" => Arc::new(KrakenProvider::new(pool.clone())),
            _ => unreachable!("validated above"),
        };
        chain.push(provider);
    }
    Ok(chain)
}

// ── Chain orchestration ───────────────────────────────────────────────────────

/// Try providers in declared order for `fetch_ohlc`; return first success.
///
/// Records an `AttemptRecord` for each provider tried (REQ-PROV-006).
/// Returns `Err` only when ALL providers fail (caller falls back to last-persisted data).
///
/// `interval_secs` is the desired candle granularity; each provider snaps it to the
/// nearest supported interval (see `Provider::fetch_ohlc`).
///
/// `registry`, when present, is poked cheaply (O(1), no I/O) at each attempt so the
/// SPEC-ALARM-001 reconciler can derive `provider-unreachable`/`all-providers-down`
/// desired state: a provider success resets its failure streak, a
/// `ProviderError::Network` bumps it (REQ-ALARM-020), and the chain outcome (all
/// attempted providers failed vs. any success) updates the chain-down flag
/// (REQ-ALARM-022). `None` (the feature-gate default) makes this a pure no-op.
pub async fn chain_fetch_ohlc(
    chain: &[Arc<dyn Provider>],
    market: &MarketQuery,
    days: u32,
    interval_secs: i64,
    registry: Option<&crate::alarm::HealthRegistry>,
) -> (Result<Vec<OhlcCandle>, ProviderError>, Vec<AttemptRecord>) {
    let mut records = Vec::new();
    // F-26: track whether any provider was actually attempted (vs skipped as Unsupported) so
    // a non-empty all-unsupported chain reports "no capable provider" instead of the
    // misleading "empty provider chain" (REQ-PROV-080). None until a real attempt fails.
    let mut last_err: Option<ProviderError> = None;

    for provider in chain {
        if !provider.supports(Capability::Ohlc) {
            records.push(AttemptRecord {
                provider: provider.name().to_string(),
                capability: Capability::Ohlc,
                outcome: ProviderOutcome::Unsupported,
            });
            continue;
        }

        match provider.fetch_ohlc(market, days, interval_secs).await {
            Ok(candles) => {
                if let Some(reg) = registry {
                    reg.record_provider_success(provider.name());
                }
                records.push(AttemptRecord {
                    provider: provider.name().to_string(),
                    capability: Capability::Ohlc,
                    outcome: ProviderOutcome::Success,
                });
                if let Some(reg) = registry {
                    reg.observe_chain_records(&records);
                }
                return (Ok(candles), records);
            }
            Err(e) => {
                if let Some(reg) = registry {
                    if matches!(e, ProviderError::Network(_)) {
                        reg.record_provider_network_failure(provider.name());
                    }
                }
                records.push(AttemptRecord {
                    provider: provider.name().to_string(),
                    capability: Capability::Ohlc,
                    outcome: ProviderOutcome::Failure,
                });
                last_err = Some(e);
            }
        }
    }

    if let Some(reg) = registry {
        reg.observe_chain_records(&records);
    }
    // Resolve the error: a real provider failure surfaces as-is; otherwise a non-empty chain
    // that was entirely Unsupported reports "no capable provider" (F-26, REQ-PROV-080), while
    // a genuinely-empty chain keeps the "empty provider chain" label.
    let err = match last_err {
        Some(e) => e,
        None if chain.is_empty() => ProviderError::Other(anyhow!("empty provider chain")),
        None => ProviderError::NoCapableProvider(Capability::Ohlc),
    };
    (Err(err), records)
}

/// Try providers in declared order for `fetch_ohlc_range`; return the first provider
/// that returns a NON-EMPTY page.
///
/// Mirrors `chain_fetch_ohlc`, but dispatches on `Capability::OhlcRange` and calls the
/// range-bounded fetch. Providers that do not support `OhlcRange` (checked via
/// `supports`) are skipped and recorded as `Unsupported`, letting e.g. CoinGecko Demo
/// fall through to Binance in the declared fallback order.
///
/// **Continue-on-empty (backfill completeness):** unlike the live `chain_fetch_ohlc`,
/// a provider returning `Ok(vec![])` here is treated as "this provider has no data for
/// this historical window" and the chain advances to the next provider — a wider
/// history source can then fill it. This is what routes a pre-2017 window (empty from
/// Binance, whose BTC/USDT klines start 2017-08) to Bitstamp (daily BTC/USD from 2011).
///
/// Result resolution after the walk:
/// - a non-empty page short-circuits and returns immediately (first data wins);
/// - **`Ok(vec![])` only when EVERY range-capable provider returned `Ok(empty)`** — a
///   genuine "no data anywhere", so the backfill worker's empty-page-forward-skip
///   advances the cursor;
/// - **`Err` when ANY provider errored** (even if an earlier one returned `Ok(empty)`) —
///   an error is not proof of "no data", so the chunk must retry and surface the error
///   rather than silently skip history. This is deliberately stricter than a plain
///   "Err only if all error": masking a deep-history source's failure behind a shallow
///   source's empty page is exactly the bug that hid a missing Bitstamp pacer row.
///
// @MX:ANCHOR: [AUTO] chain_fetch_ohlc_range — date-range OHLC dispatch for historical backfill
// @MX:REASON: fan_in >= 3: backfill worker process_chunk, provider chain tests, future callers
//             needing bounded historical windows. Tier-gating invariant: skips providers whose
//             `supports(OhlcRange)` is false (e.g. CoinGecko Demo). Continue-on-empty invariant:
//             an Ok(empty) advances to the next provider (deep-history fallthrough to Bitstamp),
//             NOT short-circuit as in the live path. Error-surfacing invariant: ANY provider
//             error yields Err (retry) — never masked by an earlier Ok(empty) (REQ-PROV-003/004).
// @MX:SPEC: SPEC-PROV-001 SPEC-SCHED-001
///
/// `registry` follows the same optional, no-op-when-`None` contract as
/// [`chain_fetch_ohlc`] (REQ-ALARM-020/022).
pub async fn chain_fetch_ohlc_range(
    chain: &[Arc<dyn Provider>],
    market: &MarketQuery,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    interval_secs: i64,
    registry: Option<&crate::alarm::HealthRegistry>,
) -> (Result<Vec<OhlcCandle>, ProviderError>, Vec<AttemptRecord>) {
    let mut records = Vec::new();
    let mut last_err: Option<ProviderError> = None;

    for provider in chain {
        if !provider.supports(Capability::OhlcRange) {
            records.push(AttemptRecord {
                provider: provider.name().to_string(),
                capability: Capability::OhlcRange,
                outcome: ProviderOutcome::Unsupported,
            });
            continue;
        }

        match provider
            .fetch_ohlc_range(market, start, end, interval_secs)
            .await
        {
            Ok(candles) => {
                if let Some(reg) = registry {
                    reg.record_provider_success(provider.name());
                }
                records.push(AttemptRecord {
                    provider: provider.name().to_string(),
                    capability: Capability::OhlcRange,
                    outcome: ProviderOutcome::Success,
                });
                if !candles.is_empty() {
                    if let Some(reg) = registry {
                        reg.observe_chain_records(&records);
                    }
                    return (Ok(candles), records);
                }
                // Empty: this provider has no data for the window — try the next.
            }
            Err(e) => {
                if let Some(reg) = registry {
                    if matches!(e, ProviderError::Network(_)) {
                        reg.record_provider_network_failure(provider.name());
                    }
                }
                records.push(AttemptRecord {
                    provider: provider.name().to_string(),
                    capability: Capability::OhlcRange,
                    outcome: ProviderOutcome::Failure,
                });
                last_err = Some(e);
            }
        }
    }

    if let Some(reg) = registry {
        reg.observe_chain_records(&records);
    }

    // Any error along the way must surface (retry) rather than be masked as "no data".
    match last_err {
        Some(e) => (Err(e), records),
        None => (Ok(vec![]), records),
    }
}

/// Generic ordered-fallback helper for the four non-OHLC provider chains: try each
/// capability-supporting provider in declared order and return the first success
/// (SPEC-REFACTOR-001 REQ-REFACTOR-020, F-53a).
///
/// This is the single implementation behind the former `chain_fetch_spot`,
/// `chain_fetch_spot_local`, `chain_fetch_coin_metadata`, and `chain_fetch_coin_market`
/// loops. It owns the `HealthRegistry` bookkeeping (per-provider success, per-provider
/// network-failure, chain success, chain all-failed) exactly as those four loops did.
///
/// The OHLC chains ([`chain_fetch_ohlc`], [`chain_fetch_ohlc_range`]) are deliberately NOT
/// routed through here — they have distinct continue-on-empty / error-surfacing semantics
/// (SPEC-REFACTOR-001 DEC-1/D5) and remain separate, unmodified functions.
///
/// # Per-provider pacing (F-16, INTENDED behavior change (a), REQ-REFACTOR-021)
///
/// `pace` is invoked for EACH attempted provider immediately before its fetch, so the pacer
/// slot is acquired for — and charged to — the provider that actually serves the request,
/// replacing the prior behavior of keying `acquire_slot` on the *first* capability-supporting
/// member before the loop (the F-16 mis-attribution). Production wires
/// `pace = |name| Box::pin(acquire_slot(pool, name))`. A provider that is paced out
/// (cooldown / credit exhaustion) is SKIPPED and the next provider is tried, so a cooled-down
/// primary no longer blocks a fallback that still has capacity. Per-provider `signal_cooldown`
/// on a 429 continues to be handled inside each provider's own `transport::paced` postlude
/// (SPEC-PROV-002), which — because `chain_try` now attempts the actual serving provider —
/// fires for the serving provider rather than the first-capable one.
///
/// # Error resolution
///
/// - a provider success short-circuits and returns immediately (first success wins);
/// - if any provider was actually fetched and every fetch failed, the last fetch error is
///   returned and `record_chain_all_failed` fires (a genuine chain failure);
/// - if every capability-supporting provider was paced out (no fetch attempted), the pacer
///   error is surfaced as [`ProviderError::Pacer`] so the worker soft-skips WITHOUT recording
///   a chain failure or consuming its retry budget (backpressure is not a failure);
/// - a non-empty chain whose members are all unsupported yields
///   [`ProviderError::NoCapableProvider`], distinct from the genuinely-empty-chain label
///   (REQ-REFACTOR-023), mirroring the OHLC chains' F-26 distinction.
///
/// `registry` follows the same optional, no-op-when-`None` contract as [`chain_fetch_ohlc`].
///
// @MX:ANCHOR: [AUTO] chain_try — the single non-OHLC ordered-fallback + per-provider-pacing helper
// @MX:REASON: fan_in >= 3 — the spot (live_poller + collection_queue), metadata, and market
//             callers plus the characterization tests all dispatch through this one function.
//             Declared order IS the fallback priority (D2/REQ-PROV-003); it MUST iterate in
//             chain order. It owns the HealthRegistry bookkeeping the four former loops carried,
//             and the empty-vs-all-unsupported error distinction (REQ-REFACTOR-020/023).
// @MX:WARN: [AUTO] `pace` is the fleet-wide egress-governor enforcement point — it now runs
//           per ATTEMPTED provider inside the loop (F-16), NOT once on first_provider_for_cap
//           before it. Do NOT hoist pacing back out of the loop: that reintroduces the
//           mis-attribution where fallback traffic is unpaced and the failing primary's
//           credits are burned. A paced-out provider is skipped, never treated as a chain
//           failure (must not fire record_chain_all_failed on pure backpressure).
// @MX:REASON: This helper governs upstream egress for every non-OHLC provider call; charging
//             the wrong provider's credit (or skipping the whole coin when a fallback has
//             capacity) is the F-16 bug this consolidation exists to fix.
// @MX:SPEC: SPEC-REFACTOR-001 REQ-REFACTOR-020 REQ-REFACTOR-021 REQ-REFACTOR-023 SPEC-PROV-001 REQ-PROV-003
pub async fn chain_try<'e, T>(
    chain: &'e [Arc<dyn Provider>],
    capability: Capability,
    registry: Option<&'e crate::alarm::HealthRegistry>,
    mut pace: impl FnMut(
        &'e str,
    ) -> Pin<
        Box<dyn Future<Output = Result<(), crate::pacer::AcquireSlotError>> + Send + 'e>,
    >,
    mut fetch: impl FnMut(
        &'e Arc<dyn Provider>,
    ) -> Pin<Box<dyn Future<Output = Result<T, ProviderError>> + Send + 'e>>,
) -> Result<T, ProviderError> {
    let mut last_fetch_err: Option<ProviderError> = None;
    let mut last_pace_err: Option<crate::pacer::AcquireSlotError> = None;

    for provider in chain {
        if !provider.supports(capability) {
            // Unsupported members are skipped and not recorded (matching the former loops).
            continue;
        }

        // Per-attempted-provider pacer acquire (F-16): charged to THIS provider — the one
        // about to serve — not to the first capability-supporting member.
        match pace(provider.name()).await {
            Ok(()) => {}
            Err(pace_err) => {
                // Paced out (cooldown / credit) or a pacer-layer error: skip THIS provider and
                // try the next. Backpressure is NOT a chain failure and is never recorded.
                last_pace_err = Some(pace_err);
                continue;
            }
        }

        match fetch(provider).await {
            Ok(v) => {
                if let Some(reg) = registry {
                    reg.record_provider_success(provider.name());
                    reg.record_chain_success();
                }
                return Ok(v);
            }
            Err(e) => {
                if let Some(reg) = registry {
                    if matches!(e, ProviderError::Network(_)) {
                        reg.record_provider_network_failure(provider.name());
                    }
                }
                last_fetch_err = Some(e);
            }
        }
    }

    match last_fetch_err {
        // At least one provider was actually fetched and every attempt failed.
        Some(e) => {
            if let Some(reg) = registry {
                reg.record_chain_all_failed();
            }
            Err(e)
        }
        None => match last_pace_err {
            // Every capability-supporting provider was paced out — surface the pacer error so
            // the worker soft-skips (releases without consuming its retry budget).
            Some(pace_err) => Err(ProviderError::Pacer(pace_err)),
            // No provider supported the capability. Distinguish empty vs all-unsupported
            // (REQ-REFACTOR-023 / F-26): a non-empty all-unsupported chain reports
            // NoCapableProvider, never the misleading "empty provider chain" label.
            None if chain.is_empty() => Err(ProviderError::Other(anyhow!("empty provider chain"))),
            None => Err(ProviderError::NoCapableProvider(capability)),
        },
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Scenario 4 (REQ-PROV-058/059): transient classification matrix (pure) ──

    #[test]
    fn is_transient_status_matrix() {
        // Transient: request-timeout (408), too-early (425), rate-limit (429), all 5xx.
        for status in [408u16, 425, 429, 500, 502, 503, 504, 599] {
            let e = ProviderError::Http {
                status,
                body: String::new(),
            };
            assert!(
                e.is_transient(),
                "HTTP {status} must be classified transient"
            );
        }
        // Permanent: 4xx other than 408/425/429 (REQ-PROV-059). This FAILS against the
        // pre-fix code where every `Http { .. }` (incl. 404) was transient — RED-first.
        for status in [400u16, 401, 403, 404, 409, 410, 422] {
            let e = ProviderError::Http {
                status,
                body: String::new(),
            };
            assert!(
                !e.is_transient(),
                "permanent HTTP {status} must NOT be classified transient"
            );
        }
        // RateLimited stays transient unchanged. (Network(_) shares the same match arm —
        // `RateLimited | Network(_) => true` — so it is covered by construction; a
        // `reqwest::Error` has no public constructor to assert it directly here.)
        assert!(ProviderError::RateLimited.is_transient());
        // Non-HTTP permanent errors are not transient.
        assert!(!ProviderError::CreditExhausted.is_transient());
        assert!(!ProviderError::Parse("x".to_string()).is_transient());
        assert!(!ProviderError::NotSupported(Capability::Spot).is_transient());
    }

    fn test_pool() -> PgPool {
        // Lazy pool: parses URL but does not connect. Providers' name() never touches the DB.
        sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://postgres@localhost/crypto_collector_test")
            .expect("lazy pool")
    }

    fn demo_config() -> CoinGeckoConfig {
        CoinGeckoConfig {
            base_url: "https://api.coingecko.com".to_string(),
            api_key: None,
            tier: crate::config::Tier::Demo,
        }
    }

    // ── Scenario 1 (REQ-PROV-002): unknown name fails fast ───────────────────

    #[tokio::test]
    async fn build_chain_unknown_name_fails_fast() {
        let names = vec!["coingecko".to_string(), "notreal".to_string()];
        let result = build_chain(&names, demo_config(), test_pool());
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error for unknown provider name, got Ok"),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("notreal"),
            "error must name the unknown value; got: {msg}"
        );
        // Must list valid names
        assert!(
            msg.contains("coingecko"),
            "must list valid names; got: {msg}"
        );
        assert!(msg.contains("binance"), "must list valid names; got: {msg}");
        assert!(
            msg.contains("coinbase"),
            "must list valid names; got: {msg}"
        );
        assert!(msg.contains("kraken"), "must list valid names; got: {msg}");
    }

    #[tokio::test]
    async fn build_chain_empty_list_returns_empty_chain() {
        let chain = build_chain(&[], demo_config(), test_pool()).expect("empty chain");
        assert!(chain.is_empty());
    }

    // ── Scenario 2 (REQ-PROV-003): declared order is fallback priority ────────

    #[tokio::test]
    async fn build_chain_preserves_declared_order() {
        let names = vec![
            "coingecko".to_string(),
            "binance".to_string(),
            "coinbase".to_string(),
            "kraken".to_string(),
        ];
        let chain = build_chain(&names, demo_config(), test_pool()).expect("chain");
        assert_eq!(chain[0].name(), "coingecko");
        assert_eq!(chain[1].name(), "binance");
        assert_eq!(chain[2].name(), "coinbase");
        assert_eq!(chain[3].name(), "kraken");
    }

    #[tokio::test]
    async fn build_chain_single_coingecko() {
        let names = vec!["coingecko".to_string()];
        let chain = build_chain(&names, demo_config(), test_pool()).expect("chain");
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].name(), "coingecko");
    }

    // ── Scenario 3 (REQ-PROV-004/006): fallback on primary failure ───────────

    struct AlwaysFailProvider;
    struct AlwaysSucceedProvider {
        candles: Vec<OhlcCandle>,
    }

    #[async_trait]
    impl Provider for AlwaysFailProvider {
        fn name(&self) -> &str {
            "stub_fail"
        }
        fn supports(&self, _cap: Capability) -> bool {
            true
        }
        // Only fetch_ohlc is exercised (via chain_fetch_ohlc). fetch_ohlc_range relies on the
        // trait default (Err(NotSupported(OhlcRange))), which is exactly what the
        // error-not-masked range test asserts. Every other fetch method relies on trait
        // defaults and is never called here.
        async fn fetch_ohlc(
            &self,
            _m: &MarketQuery,
            _days: u32,
            _interval_secs: i64,
        ) -> Result<Vec<OhlcCandle>, ProviderError> {
            Err(ProviderError::Http {
                status: 500,
                body: "stub error".to_string(),
            })
        }
    }

    #[async_trait]
    impl Provider for AlwaysSucceedProvider {
        fn name(&self) -> &str {
            "stub_success"
        }
        fn supports(&self, _cap: Capability) -> bool {
            true
        }
        // Only fetch_ohlc is exercised (via chain_fetch_ohlc); every other fetch method
        // relies on trait defaults and is never called here.
        async fn fetch_ohlc(
            &self,
            _m: &MarketQuery,
            _days: u32,
            _interval_secs: i64,
        ) -> Result<Vec<OhlcCandle>, ProviderError> {
            Ok(self.candles.clone())
        }
    }

    fn stub_market() -> MarketQuery {
        MarketQuery::MarketKeyed {
            market_id: 1,
            coin_id: Some("bitcoin".to_string()),
            base: "BTC".to_string(),
            quote: "USD".to_string(),
            venue: None,
            vs_currency: "usd".to_string(),
        }
    }

    #[tokio::test]
    async fn chain_advances_to_secondary_on_primary_failure() {
        let candle = OhlcCandle {
            market_id: 1,
            interval: "4h".to_string(),
            ts: Utc::now(),
            open: rust_decimal_macros::dec!(90000),
            high: rust_decimal_macros::dec!(91000),
            low: rust_decimal_macros::dec!(89000),
            close: rust_decimal_macros::dec!(90500),
            volume: None,
            vs_currency: "usd".to_string(),
            source: "stub_success".to_string(),
        };

        let chain: Vec<Arc<dyn Provider>> = vec![
            Arc::new(AlwaysFailProvider),
            Arc::new(AlwaysSucceedProvider {
                candles: vec![candle.clone()],
            }),
        ];

        let market = stub_market();
        // Use the global default interval (60 s) for stub tests.
        let (result, records) = chain_fetch_ohlc(&chain, &market, 7, 60, None).await;

        // Result: secondary's candles
        let candles = result.expect("should return secondary's candles");
        assert_eq!(candles.len(), 1);

        // Records: primary=Failure, secondary=Success (REQ-PROV-006)
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].provider, "stub_fail");
        assert_eq!(records[0].outcome, ProviderOutcome::Failure);
        assert_eq!(records[1].provider, "stub_success");
        assert_eq!(records[1].outcome, ProviderOutcome::Success);
    }

    // ── Scenario 4 (REQ-PROV-005): all fail → chain returns error ────────────

    #[tokio::test]
    async fn chain_returns_error_when_all_providers_fail() {
        let chain: Vec<Arc<dyn Provider>> =
            vec![Arc::new(AlwaysFailProvider), Arc::new(AlwaysFailProvider)];
        let market = stub_market();
        let (result, records) = chain_fetch_ohlc(&chain, &market, 7, 60, None).await;

        assert!(result.is_err(), "must return error when all providers fail");
        assert_eq!(records.len(), 2);
        assert!(records
            .iter()
            .all(|r| r.outcome == ProviderOutcome::Failure));
    }

    // Unsupported capability is recorded correctly
    #[tokio::test]
    async fn chain_records_unsupported_outcome() {
        struct UnsupportedProvider;

        #[async_trait]
        impl Provider for UnsupportedProvider {
            fn name(&self) -> &str {
                "stub_unsupported"
            }
            fn supports(&self, _cap: Capability) -> bool {
                false // supports nothing
            }
            // No fetch method is exercised: chain_fetch_ohlc skips this provider on
            // supports(Ohlc)=false, so every fetch method relies on the trait defaults.
        }

        let chain: Vec<Arc<dyn Provider>> = vec![Arc::new(UnsupportedProvider)];
        let market = stub_market();
        let (result, records) = chain_fetch_ohlc(&chain, &market, 7, 60, None).await;

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, ProviderOutcome::Unsupported);

        // F-26 (REQ-PROV-080): a NON-empty chain whose every member is Unsupported reports
        // "no capable provider" — NOT the misleading "empty provider chain" label.
        match result {
            Err(ProviderError::NoCapableProvider(Capability::Ohlc)) => {}
            other => panic!("expected NoCapableProvider(Ohlc) for a non-empty all-unsupported chain, got: {other:?}"),
        }
    }

    /// F-26 (REQ-PROV-080): the genuinely-empty-chain case is preserved — it still reports
    /// "empty provider chain", distinct from the non-empty all-unsupported case above.
    #[tokio::test]
    async fn chain_fetch_ohlc_empty_chain_still_reports_empty() {
        let chain: Vec<Arc<dyn Provider>> = vec![];
        let market = stub_market();
        let (result, records) = chain_fetch_ohlc(&chain, &market, 7, 60, None).await;

        assert!(records.is_empty());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("empty provider chain"),
            "a genuinely empty chain must still report 'empty provider chain', got: {msg}"
        );
    }

    // ── chain_fetch_ohlc_range: skips non-range providers, returns first success ──

    struct RangeIncapableProvider;
    struct RangeCapableProvider {
        candles: Vec<OhlcCandle>,
    }

    #[async_trait]
    impl Provider for RangeIncapableProvider {
        fn name(&self) -> &str {
            "stub_no_range"
        }
        fn supports(&self, cap: Capability) -> bool {
            matches!(cap, Capability::Ohlc) // Ohlc yes, OhlcRange no
        }
        // Skipped by chain_fetch_ohlc_range (supports(OhlcRange)=false), so no fetch method
        // is exercised — every one relies on the trait defaults (fetch_ohlc_range's default
        // is Err(NotSupported(OhlcRange))).
    }

    #[async_trait]
    impl Provider for RangeCapableProvider {
        fn name(&self) -> &str {
            "stub_range"
        }
        fn supports(&self, cap: Capability) -> bool {
            matches!(cap, Capability::Ohlc | Capability::OhlcRange)
        }
        // Only fetch_ohlc_range is exercised; every other fetch method relies on trait
        // defaults and is never called here.
        async fn fetch_ohlc_range(
            &self,
            _m: &MarketQuery,
            _start: DateTime<Utc>,
            _end: DateTime<Utc>,
            _interval_secs: i64,
        ) -> Result<Vec<OhlcCandle>, ProviderError> {
            Ok(self.candles.clone())
        }
    }

    #[tokio::test]
    async fn chain_fetch_ohlc_range_skips_non_range_provider_and_returns_first_success() {
        let candle = OhlcCandle {
            market_id: 1,
            interval: "1d".to_string(),
            ts: Utc::now(),
            open: rust_decimal_macros::dec!(1),
            high: rust_decimal_macros::dec!(2),
            low: rust_decimal_macros::dec!(1),
            close: rust_decimal_macros::dec!(1.5),
            volume: None,
            vs_currency: "usd".to_string(),
            source: "stub_range".to_string(),
        };

        let chain: Vec<Arc<dyn Provider>> = vec![
            Arc::new(RangeIncapableProvider),
            Arc::new(RangeCapableProvider {
                candles: vec![candle],
            }),
        ];

        let market = stub_market();
        let start = Utc::now() - chrono::Duration::days(30);
        let end = Utc::now();
        let (result, records) =
            chain_fetch_ohlc_range(&chain, &market, start, end, 86_400, None).await;

        let candles = result.expect("should fall through to range-capable provider");
        assert_eq!(candles.len(), 1);

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].provider, "stub_no_range");
        assert_eq!(records[0].outcome, ProviderOutcome::Unsupported);
        assert_eq!(records[1].provider, "stub_range");
        assert_eq!(records[1].outcome, ProviderOutcome::Success);
    }

    // ── continue-on-empty: an empty earlier provider falls through to a wider source ──

    /// A range provider that always returns `Ok(vec![])` (e.g. Binance for a pre-2017
    /// window: symbol exists, no candles that far back).
    struct RangeEmptyProvider {
        provider_name: &'static str,
    }

    #[async_trait]
    impl Provider for RangeEmptyProvider {
        fn name(&self) -> &str {
            self.provider_name
        }
        fn supports(&self, cap: Capability) -> bool {
            matches!(cap, Capability::Ohlc | Capability::OhlcRange)
        }
        // Only fetch_ohlc_range is exercised — it returns Ok(empty) to drive the
        // continue-on-empty fallthrough. Every other fetch method relies on trait defaults.
        async fn fetch_ohlc_range(
            &self,
            _m: &MarketQuery,
            _start: DateTime<Utc>,
            _end: DateTime<Utc>,
            _i: i64,
        ) -> Result<Vec<OhlcCandle>, ProviderError> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn chain_fetch_ohlc_range_falls_through_empty_provider_to_data_source() {
        let candle = OhlcCandle {
            market_id: 1,
            interval: "1d".to_string(),
            ts: Utc::now(),
            open: rust_decimal_macros::dec!(10),
            high: rust_decimal_macros::dec!(12),
            low: rust_decimal_macros::dec!(9),
            close: rust_decimal_macros::dec!(11),
            volume: Some(rust_decimal_macros::dec!(1)),
            vs_currency: "usd".to_string(),
            source: "stub_range".to_string(),
        };

        // Mirrors production: binance (empty for pre-2017) then bitstamp (has data).
        let chain: Vec<Arc<dyn Provider>> = vec![
            Arc::new(RangeEmptyProvider {
                provider_name: "binance",
            }),
            Arc::new(RangeCapableProvider {
                candles: vec![candle],
            }),
        ];

        let market = stub_market();
        let start = Utc::now() - chrono::Duration::days(3000);
        let end = start + chrono::Duration::days(30);
        let (result, records) =
            chain_fetch_ohlc_range(&chain, &market, start, end, 86_400, None).await;

        let candles = result.expect("empty binance must fall through to the data source");
        assert_eq!(candles.len(), 1, "must return the second provider's candle");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].provider, "binance");
        assert_eq!(records[0].outcome, ProviderOutcome::Success); // Ok(empty) is a success attempt
        assert_eq!(records[1].provider, "stub_range");
        assert_eq!(records[1].outcome, ProviderOutcome::Success);
    }

    #[tokio::test]
    async fn chain_fetch_ohlc_range_all_empty_returns_ok_empty_not_err() {
        let chain: Vec<Arc<dyn Provider>> = vec![
            Arc::new(RangeEmptyProvider {
                provider_name: "binance",
            }),
            Arc::new(RangeEmptyProvider {
                provider_name: "bitstamp",
            }),
        ];
        let market = stub_market();
        let start = Utc::now() - chrono::Duration::days(6000);
        let end = start + chrono::Duration::days(30);
        let (result, _records) =
            chain_fetch_ohlc_range(&chain, &market, start, end, 86_400, None).await;
        // No provider had data for the window → Ok(empty), so the worker's
        // empty-page-forward-skip advances the cursor rather than failing the chunk.
        assert!(
            result
                .expect("all-empty must be Ok(empty), not Err")
                .is_empty(),
            "all-empty range walk must resolve to an empty page"
        );
    }

    #[tokio::test]
    async fn chain_fetch_ohlc_range_error_not_masked_by_earlier_empty() {
        // binance returns Ok(empty) (pre-2017 window), then the deep-history source
        // ERRORS. The error MUST surface so the chunk retries — it must NOT be masked as
        // Ok(empty), which would silently forward-skip and lose the history (the real
        // production bug: a missing Bitstamp pacer row made its fetch error out).
        // AlwaysFailProvider reports supports(_) = true and has no fetch_ohlc_range
        // override, so the range dispatch calls it and gets the trait-default
        // Err(NotSupported) — an "errored" attempt for this test's purpose.
        let chain: Vec<Arc<dyn Provider>> = vec![
            Arc::new(RangeEmptyProvider {
                provider_name: "binance",
            }),
            Arc::new(AlwaysFailProvider),
        ];
        let market = stub_market();
        let start = Utc::now() - chrono::Duration::days(4000);
        let end = start + chrono::Duration::days(30);
        let (result, _records) =
            chain_fetch_ohlc_range(&chain, &market, start, end, 86_400, None).await;
        assert!(
            result.is_err(),
            "a provider error after an earlier Ok(empty) must surface as Err, not Ok(empty)"
        );
    }

    // ── SPEC-REFACTOR-001 M1 (F-50): capability-derived trait defaults ────────────

    /// AC-REFACTOR-011a — object-safety preserved: `Arc<dyn Provider>` construction and
    /// `build_chain` compile after the trait gains default bodies. No default introduces a
    /// generic type parameter, so `dyn Provider` remains constructible and the chain the
    /// workers consume still builds.
    #[tokio::test]
    async fn provider_trait_object_safety_and_chain_build() {
        fn assert_object_safe(_p: &dyn Provider) {}

        let names = vec![
            "coingecko".to_string(),
            "coinbase".to_string(),
            "kraken".to_string(),
        ];
        let chain: Vec<Arc<dyn Provider>> = build_chain(&names, demo_config(), test_pool())
            .expect("build_chain must construct an Arc<dyn Provider> chain");
        assert_eq!(chain.len(), 3);
        for provider in &chain {
            // Exercise the trait-object vtable — proves `dyn Provider` is object-safe.
            assert_object_safe(provider.as_ref());
        }
    }

    /// AC-REFACTOR-010 / AC-REFACTOR-014a — characterization (behavior preservation): after
    /// coinbase.rs / kraken.rs shed their explicit stub bodies, both STILL return exactly the
    /// prior behavior via the trait defaults — `Err(NotSupported(<capability>))` for every
    /// fetch method and `Ok(vec![])` for the Opt-A search pair. (This test lives in mod.rs, not
    /// in coinbase.rs / kraken.rs, so the AC-REFACTOR-012a `grep -c NotSupported` anchor on
    /// those two files stays 0.)
    #[tokio::test]
    async fn coinbase_and_kraken_fetch_methods_default_to_prior_behavior() {
        let pool = test_pool();
        let coinbase = CoinbaseProvider::new(pool.clone());
        let kraken = KrakenProvider::new(pool.clone());
        let m = stub_market();
        let now = Utc::now();

        let cases: [(&str, &dyn Provider); 2] = [
            ("coinbase", &coinbase as &dyn Provider),
            ("kraken", &kraken as &dyn Provider),
        ];

        for (label, p) in cases {
            assert!(
                matches!(
                    p.fetch_spot(&m).await,
                    Err(ProviderError::NotSupported(Capability::Spot))
                ),
                "{label} fetch_spot must default to NotSupported(Spot)"
            );
            assert!(
                matches!(
                    p.fetch_ohlc(&m, 7, 60).await,
                    Err(ProviderError::NotSupported(Capability::Ohlc))
                ),
                "{label} fetch_ohlc must default to NotSupported(Ohlc)"
            );
            assert!(
                matches!(
                    p.fetch_ohlc_range(&m, now, now, 60).await,
                    Err(ProviderError::NotSupported(Capability::OhlcRange))
                ),
                "{label} fetch_ohlc_range must default to NotSupported(OhlcRange)"
            );
            assert!(
                matches!(
                    p.fetch_coin_metadata("bitcoin").await,
                    Err(ProviderError::NotSupported(Capability::CoinMetadata))
                ),
                "{label} fetch_coin_metadata must default to NotSupported(CoinMetadata)"
            );
            assert!(
                matches!(
                    p.fetch_coin_market("bitcoin", "usd").await,
                    Err(ProviderError::NotSupported(Capability::CoinMarket))
                ),
                "{label} fetch_coin_market must default to NotSupported(CoinMarket)"
            );
            assert!(
                matches!(
                    p.fetch_derivatives(&m).await,
                    Err(ProviderError::NotSupported(Capability::Derivatives))
                ),
                "{label} fetch_derivatives must default to NotSupported(Derivatives)"
            );
            // Opt-A search pair defaults to Ok(vec![]) (DEC-3 / AC-REFACTOR-014a).
            assert!(
                p.search_coins("btc", 5)
                    .await
                    .expect("search_coins default must be Ok")
                    .is_empty(),
                "{label} search_coins must default to Ok(vec![])"
            );
            assert!(
                p.fetch_coin_tickers("bitcoin", 5)
                    .await
                    .expect("fetch_coin_tickers default must be Ok")
                    .is_empty(),
                "{label} fetch_coin_tickers must default to Ok(vec![])"
            );
        }
    }

    // ── SPEC-REFACTOR-001 M2 (F-53a, F-16): chain_try characterization ───────────
    //
    // These are PURE (no DB): chain_try's fallback loop + registry bookkeeping is exercised
    // via injected `pace` / `fetch` closures, so both pacing and fetching are fully under
    // test control. Production wires `pace = |name| Box::pin(acquire_slot(pool, name))` and
    // `fetch = |p| Box::pin(p.fetch_spot(&mq))`; here they are canned so the branch behavior
    // is asserted without a live PostgreSQL or a live upstream.

    /// Minimal capability-configurable test double. Only `name()` / `supports()` are
    /// exercised — every fetch is supplied by chain_try's injected `fetch` closure, so the
    /// fetch methods rely on the trait defaults and are never called.
    struct CapProvider {
        nm: &'static str,
        caps: &'static [Capability],
    }

    #[async_trait]
    impl Provider for CapProvider {
        fn name(&self) -> &str {
            self.nm
        }
        fn supports(&self, cap: Capability) -> bool {
            self.caps.contains(&cap)
        }
    }

    fn spot_stub(source: &str) -> SpotQuote {
        SpotQuote {
            market_id: 0,
            ts: Utc::now(),
            price: rust_decimal_macros::dec!(100),
            bid: None,
            ask: None,
            volume_24h: None,
            vs_currency: "usd".to_string(),
            source: source.to_string(),
        }
    }

    /// AC-REFACTOR-020b(i): primary success returns the primary result and records
    /// per-provider + chain success; only the primary is attempted.
    #[tokio::test]
    async fn chain_try_primary_success_records_success() {
        let chain: Vec<Arc<dyn Provider>> = vec![
            Arc::new(CapProvider {
                nm: "coingecko",
                caps: &[Capability::Spot],
            }),
            Arc::new(CapProvider {
                nm: "binance",
                caps: &[Capability::Spot],
            }),
        ];
        let reg = crate::alarm::HealthRegistry::new();
        let paced = std::cell::RefCell::new(Vec::<String>::new());

        let result = chain_try(
            &chain,
            Capability::Spot,
            Some(&reg),
            |name| {
                paced.borrow_mut().push(name.to_string());
                Box::pin(async { Ok::<(), crate::pacer::AcquireSlotError>(()) })
            },
            |p| {
                let nm = p.name().to_string();
                Box::pin(async move { Ok(spot_stub(&nm)) })
            },
        )
        .await;

        assert_eq!(
            result.expect("primary must succeed").source,
            "coingecko",
            "the primary provider's result must be returned"
        );
        // First success wins — only the primary is attempted (and paced).
        assert_eq!(*paced.borrow(), vec!["coingecko".to_string()]);
        assert!(!reg.all_providers_down(), "chain success recorded");
        assert!(reg.provider_snapshot("coingecko").last_success_at.is_some());
    }

    /// AC-REFACTOR-020b(ii) + AC-REFACTOR-021a: primary error then fallback success. Proves
    /// the F-16 fix — the pacer slot is acquired for EACH attempted provider (both coingecko
    /// AND binance), so the serving fallback is charged; under the prior first_provider_for_cap
    /// behavior binance (the fallback) was never paced.
    #[tokio::test]
    async fn chain_try_falls_back_on_primary_error_and_paces_each_attempt() {
        let chain: Vec<Arc<dyn Provider>> = vec![
            Arc::new(CapProvider {
                nm: "coingecko",
                caps: &[Capability::Spot],
            }),
            Arc::new(CapProvider {
                nm: "binance",
                caps: &[Capability::Spot],
            }),
        ];
        let reg = crate::alarm::HealthRegistry::new();
        let paced = std::cell::RefCell::new(Vec::<String>::new());

        let result = chain_try(
            &chain,
            Capability::Spot,
            Some(&reg),
            |name| {
                paced.borrow_mut().push(name.to_string());
                Box::pin(async { Ok::<(), crate::pacer::AcquireSlotError>(()) })
            },
            |p| {
                let nm = p.name().to_string();
                Box::pin(async move {
                    if nm == "coingecko" {
                        Err(ProviderError::Http {
                            status: 500,
                            body: "boom".to_string(),
                        })
                    } else {
                        Ok(spot_stub(&nm))
                    }
                })
            },
        )
        .await;

        assert_eq!(
            result.expect("fallback must serve").source,
            "binance",
            "the fallback provider's result must be returned"
        );
        // INTENDED CHANGE (a): the slot is acquired for the provider that actually serves.
        // Both attempted providers were paced, in declared order.
        assert_eq!(
            *paced.borrow(),
            vec!["coingecko".to_string(), "binance".to_string()],
            "each attempted provider's slot must be acquired (F-16 attribution)"
        );
        assert!(!reg.all_providers_down(), "fallback served → chain success");
        assert!(reg.provider_snapshot("binance").last_success_at.is_some());
    }

    /// AC-REFACTOR-020b(iii): the first capable-by-order member is unsupported for the
    /// requested capability; chain_try skips it (no pace, no fetch) and serves from the next.
    #[tokio::test]
    async fn chain_try_skips_unsupported_first_member() {
        let chain: Vec<Arc<dyn Provider>> = vec![
            Arc::new(CapProvider {
                nm: "coingecko",
                caps: &[Capability::CoinMetadata], // does NOT support Spot
            }),
            Arc::new(CapProvider {
                nm: "binance",
                caps: &[Capability::Spot],
            }),
        ];
        let paced = std::cell::RefCell::new(Vec::<String>::new());

        let result = chain_try(
            &chain,
            Capability::Spot,
            None,
            |name| {
                paced.borrow_mut().push(name.to_string());
                Box::pin(async { Ok::<(), crate::pacer::AcquireSlotError>(()) })
            },
            |p| {
                let nm = p.name().to_string();
                Box::pin(async move { Ok(spot_stub(&nm)) })
            },
        )
        .await;

        assert_eq!(result.expect("binance serves").source, "binance");
        // The unsupported member is neither paced nor fetched.
        assert_eq!(*paced.borrow(), vec!["binance".to_string()]);
    }

    /// AC-REFACTOR-020b(iv) + REQ-REFACTOR-023: a non-empty chain whose members are all
    /// unsupported yields NoCapableProvider — not the misleading empty-chain label.
    #[tokio::test]
    async fn chain_try_all_unsupported_reports_no_capable_provider() {
        let chain: Vec<Arc<dyn Provider>> = vec![
            Arc::new(CapProvider {
                nm: "coingecko",
                caps: &[Capability::CoinMetadata],
            }),
            Arc::new(CapProvider {
                nm: "binance",
                caps: &[Capability::Ohlc],
            }),
        ];
        let result = chain_try(
            &chain,
            Capability::Spot,
            None,
            |_name| Box::pin(async { Ok::<(), crate::pacer::AcquireSlotError>(()) }),
            |p| {
                let nm = p.name().to_string();
                Box::pin(async move { Ok(spot_stub(&nm)) })
            },
        )
        .await;
        match result {
            Err(ProviderError::NoCapableProvider(Capability::Spot)) => {}
            other => panic!("expected NoCapableProvider(Spot), got: {other:?}"),
        }
    }

    /// REQ-REFACTOR-023: a genuinely empty chain still reports the empty-chain label,
    /// distinct from the non-empty all-unsupported case above.
    #[tokio::test]
    async fn chain_try_empty_chain_reports_empty() {
        let chain: Vec<Arc<dyn Provider>> = vec![];
        let result: Result<SpotQuote, _> = chain_try(
            &chain,
            Capability::Spot,
            None,
            |_name| Box::pin(async { Ok::<(), crate::pacer::AcquireSlotError>(()) }),
            |p| {
                let nm = p.name().to_string();
                Box::pin(async move { Ok(spot_stub(&nm)) })
            },
        )
        .await;
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("empty provider chain"),
            "a genuinely empty chain must report 'empty provider chain', got: {msg}"
        );
    }

    /// AC-REFACTOR-021a companion (intended change (a)): a cooled-down primary must NOT block
    /// a fallback that still has capacity — per-provider pacing lets the fallback serve.
    #[tokio::test]
    async fn chain_try_paced_out_primary_falls_through_to_fallback() {
        let chain: Vec<Arc<dyn Provider>> = vec![
            Arc::new(CapProvider {
                nm: "coingecko",
                caps: &[Capability::Spot],
            }),
            Arc::new(CapProvider {
                nm: "binance",
                caps: &[Capability::Spot],
            }),
        ];
        let reg = crate::alarm::HealthRegistry::new();
        let result = chain_try(
            &chain,
            Capability::Spot,
            Some(&reg),
            |name| {
                let nm = name.to_string();
                Box::pin(async move {
                    if nm == "coingecko" {
                        Err(crate::pacer::AcquireSlotError::Cooldown(nm, Utc::now()))
                    } else {
                        Ok(())
                    }
                })
            },
            |p| {
                let nm = p.name().to_string();
                Box::pin(async move { Ok(spot_stub(&nm)) })
            },
        )
        .await;
        assert_eq!(
            result
                .expect("fallback serves past cooled-down primary")
                .source,
            "binance"
        );
        // A paced-out provider is backpressure, NOT a chain failure.
        assert!(!reg.all_providers_down());
    }

    /// Every capability-supporting provider is paced out → chain_try surfaces a Pacer error so
    /// the worker soft-skips, and it does NOT record a chain failure (backpressure ≠ failure).
    #[tokio::test]
    async fn chain_try_all_paced_out_surfaces_pacer_error() {
        let chain: Vec<Arc<dyn Provider>> = vec![
            Arc::new(CapProvider {
                nm: "coingecko",
                caps: &[Capability::Spot],
            }),
            Arc::new(CapProvider {
                nm: "binance",
                caps: &[Capability::Spot],
            }),
        ];
        let reg = crate::alarm::HealthRegistry::new();
        let result: Result<SpotQuote, _> = chain_try(
            &chain,
            Capability::Spot,
            Some(&reg),
            |name| {
                let nm = name.to_string();
                Box::pin(
                    async move { Err(crate::pacer::AcquireSlotError::Cooldown(nm, Utc::now())) },
                )
            },
            |p| {
                let nm = p.name().to_string();
                Box::pin(async move { Ok(spot_stub(&nm)) })
            },
        )
        .await;
        match result {
            Err(ProviderError::Pacer(crate::pacer::AcquireSlotError::Cooldown(..))) => {}
            other => panic!("all-paced-out must surface a Pacer error, got: {other:?}"),
        }
        assert!(
            !reg.all_providers_down(),
            "a paced-out chain must NOT record chain_all_failed"
        );
    }

    /// Every attempted provider's fetch fails → the last error is returned and the chain-all-
    /// failed signal is stamped (REQ-REFACTOR-020 registry bookkeeping).
    #[tokio::test]
    async fn chain_try_all_fail_records_chain_all_failed() {
        let chain: Vec<Arc<dyn Provider>> = vec![
            Arc::new(CapProvider {
                nm: "coingecko",
                caps: &[Capability::Spot],
            }),
            Arc::new(CapProvider {
                nm: "binance",
                caps: &[Capability::Spot],
            }),
        ];
        let reg = crate::alarm::HealthRegistry::new();
        let result: Result<SpotQuote, _> = chain_try(
            &chain,
            Capability::Spot,
            Some(&reg),
            |_name| Box::pin(async { Ok::<(), crate::pacer::AcquireSlotError>(()) }),
            |_p| {
                Box::pin(async {
                    Err(ProviderError::Http {
                        status: 500,
                        body: "down".to_string(),
                    })
                })
            },
        )
        .await;
        assert!(result.is_err(), "all fetches failed → error");
        assert!(
            reg.all_providers_down(),
            "every attempted provider failed → chain_all_failed stamped"
        );
    }
}
