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

/// Context for market-level provider calls.
#[derive(Debug, Clone)]
pub struct MarketQuery {
    /// Internal market registry ID (used to tag normalised models).
    pub market_id: i64,
    /// CoinGecko coin identifier (e.g. `"bitcoin"`); `None` for exchange-only providers.
    pub coin_id: Option<String>,
    /// Base asset symbol (e.g. `"BTC"`).
    pub base: String,
    /// Quote asset symbol (e.g. `"USDT"`).
    pub quote: String,
    /// Trading venue (e.g. `"binance"`); `None` = aggregator/CoinGecko source.
    pub venue: Option<String>,
    /// Price vs-currency (e.g. `"usd"`).
    pub vs_currency: String,
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
        MarketQuery {
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
}
