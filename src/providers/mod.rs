//! Provider trait, capability enum, and chain builder (SPEC-PROV-001).
//!
//! The chain is ordered (declared order = fallback priority) and fail-fast on unknown names.
//! Mirrors `ticker-collector`'s `providers/mod.rs::build_chain` pattern (research §2.5).

use anyhow::anyhow;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use sqlx::PgPool;
use std::sync::Arc;
use thiserror::Error;

pub mod binance;
pub mod coinbase;
pub mod coingecko;
pub mod kraken;

pub use binance::BinanceProvider;
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

/// Market search result returned from a provider search (SPEC-PROV-001 REQ-PROV-005).
///
/// Shared between the provider layer and the API layer so that
/// `CoinGeckoClient::search_markets` and `GET /v1/markets/search` operate on one type.
/// Fields map to CoinGecko `/search` `exchanges[]`: base/target/market.identifier.
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
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            ProviderError::RateLimited | ProviderError::Network(_) | ProviderError::Http { .. }
        )
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
// @MX:SPEC: SPEC-PROV-001 REQ-PROV-001/003/004
#[async_trait]
pub trait Provider: Send + Sync {
    /// Provider identifier (e.g. `"coingecko"`, `"binance"`).
    fn name(&self) -> &str;

    /// True if this provider can fulfil the given capability.
    fn supports(&self, cap: Capability) -> bool;

    /// Fetch a live spot quote for the given market.
    async fn fetch_spot(&self, market: &MarketQuery) -> Result<SpotQuote, ProviderError>;

    /// Fetch OHLC candles. `days` selects the lookback window.
    ///
    /// REQ-PROV-013: CoinGecko candles have `volume = None`.
    async fn fetch_ohlc(
        &self,
        market: &MarketQuery,
        days: u32,
    ) -> Result<Vec<OhlcCandle>, ProviderError>;

    /// Fetch slowly-changing coin metadata (descriptions, links, supply cap).
    async fn fetch_coin_metadata(&self, coin_id: &str) -> Result<CoinMeta, ProviderError>;

    /// Fetch continuously-changing coin market aggregates (price, cap, supply, FDV).
    async fn fetch_coin_market(
        &self,
        coin_id: &str,
        vs_currency: &str,
    ) -> Result<CoinMarket, ProviderError>;

    /// Fetch the latest derivative tick (funding rate, OI, mark/index, basis).
    async fn fetch_derivatives(&self, market: &MarketQuery) -> Result<DerivTick, ProviderError>;

    /// Search for coins by name / symbol (SPEC-PROV-001 REQ-PROV-005).
    ///
    /// Returns up to `cap` results. Providers that do not support coin search return `Ok(vec![])`.
    /// Upstream non-success responses degrade to empty (REQ-PROV-005) and are WARN-logged by the
    /// client; callers should treat `Err` from this method as a network-level failure and may
    /// choose to degrade to empty rather than propagate.
    async fn search_coins(
        &self,
        q: &str,
        cap: usize,
    ) -> Result<Vec<CoinSearchResult>, ProviderError>;

    /// Search for market pairs (exchanges) by query string (SPEC-PROV-001 REQ-PROV-005).
    ///
    /// Returns up to `cap` results. Providers that do not support market search return `Ok(vec![])`.
    /// Upstream non-success responses degrade to empty (REQ-PROV-005) and are WARN-logged by the
    /// client; callers should treat `Err` from this method as a network-level failure and may
    /// choose to degrade to empty rather than propagate.
    async fn search_markets(
        &self,
        q: &str,
        cap: usize,
    ) -> Result<Vec<MarketSearchResult>, ProviderError>;
}

// ── Chain builder ─────────────────────────────────────────────────────────────

/// Build the ordered provider chain from a list of names (REQ-PROV-002/003).
///
/// Fails fast if any name is unknown — returns an error naming the offending value
/// and listing all valid names. Declared order equals fallback priority.
///
/// Valid names: `coingecko`, `binance`, `coinbase`, `kraken`.
///
// @MX:ANCHOR: [AUTO] build_chain — ordered fail-fast provider chain constructor
// @MX:REASON: Every worker and the chain orchestrator depends on this for data acquisition.
//             Startup invariant: unknown name = immediate error (REQ-PROV-002).
//             Declared order IS the fallback priority (REQ-PROV-003).
//             fan_in >= 3: main startup, SPEC-SCHED-001 workers, integration tests.
// @MX:NOTE: [AUTO] Valid provider names: coingecko, binance, coinbase, kraken
// @MX:SPEC: SPEC-PROV-001 REQ-PROV-002/003
pub fn build_chain(
    names: &[String],
    coingecko_config: CoinGeckoConfig,
    pool: PgPool,
) -> anyhow::Result<Vec<Arc<dyn Provider>>> {
    const VALID_NAMES: &[&str] = &["coingecko", "binance", "coinbase", "kraken"];

    // Fail-fast validation (REQ-PROV-002)
    for name in names {
        if !VALID_NAMES.contains(&name.as_str()) {
            return Err(anyhow!(
                "unknown provider: {name:?}. Valid names: coingecko, binance, coinbase, kraken"
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
pub async fn chain_fetch_ohlc(
    chain: &[Arc<dyn Provider>],
    market: &MarketQuery,
    days: u32,
) -> (Result<Vec<OhlcCandle>, ProviderError>, Vec<AttemptRecord>) {
    let mut records = Vec::new();
    let mut last_err = ProviderError::Other(anyhow!("empty provider chain"));

    for provider in chain {
        if !provider.supports(Capability::Ohlc) {
            records.push(AttemptRecord {
                provider: provider.name().to_string(),
                capability: Capability::Ohlc,
                outcome: ProviderOutcome::Unsupported,
            });
            continue;
        }

        match provider.fetch_ohlc(market, days).await {
            Ok(candles) => {
                records.push(AttemptRecord {
                    provider: provider.name().to_string(),
                    capability: Capability::Ohlc,
                    outcome: ProviderOutcome::Success,
                });
                return (Ok(candles), records);
            }
            Err(e) => {
                records.push(AttemptRecord {
                    provider: provider.name().to_string(),
                    capability: Capability::Ohlc,
                    outcome: ProviderOutcome::Failure,
                });
                last_err = e;
            }
        }
    }

    (Err(last_err), records)
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
            tier: "demo".to_string(),
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
        async fn fetch_spot(&self, _m: &MarketQuery) -> Result<SpotQuote, ProviderError> {
            Err(ProviderError::Http {
                status: 500,
                body: "stub error".to_string(),
            })
        }
        async fn fetch_ohlc(
            &self,
            _m: &MarketQuery,
            _days: u32,
        ) -> Result<Vec<OhlcCandle>, ProviderError> {
            Err(ProviderError::Http {
                status: 500,
                body: "stub error".to_string(),
            })
        }
        async fn fetch_coin_metadata(&self, _id: &str) -> Result<CoinMeta, ProviderError> {
            Err(ProviderError::Http {
                status: 500,
                body: "stub error".to_string(),
            })
        }
        async fn fetch_coin_market(
            &self,
            _id: &str,
            _vs: &str,
        ) -> Result<CoinMarket, ProviderError> {
            Err(ProviderError::Http {
                status: 500,
                body: "stub error".to_string(),
            })
        }
        async fn fetch_derivatives(&self, _m: &MarketQuery) -> Result<DerivTick, ProviderError> {
            Err(ProviderError::Http {
                status: 500,
                body: "stub error".to_string(),
            })
        }
        async fn search_coins(
            &self,
            _q: &str,
            _cap: usize,
        ) -> Result<Vec<CoinSearchResult>, ProviderError> {
            Ok(vec![])
        }
        async fn search_markets(
            &self,
            _q: &str,
            _cap: usize,
        ) -> Result<Vec<MarketSearchResult>, ProviderError> {
            Ok(vec![])
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
        async fn fetch_spot(&self, m: &MarketQuery) -> Result<SpotQuote, ProviderError> {
            Ok(SpotQuote {
                market_id: m.market_id,
                ts: Utc::now(),
                price: rust_decimal_macros::dec!(100),
                bid: None,
                ask: None,
                volume_24h: None,
                vs_currency: m.vs_currency.clone(),
                source: "stub_success".to_string(),
            })
        }
        async fn fetch_ohlc(
            &self,
            _m: &MarketQuery,
            _days: u32,
        ) -> Result<Vec<OhlcCandle>, ProviderError> {
            Ok(self.candles.clone())
        }
        async fn fetch_coin_metadata(&self, _id: &str) -> Result<CoinMeta, ProviderError> {
            Err(ProviderError::NotSupported(Capability::CoinMetadata))
        }
        async fn fetch_coin_market(
            &self,
            _id: &str,
            _vs: &str,
        ) -> Result<CoinMarket, ProviderError> {
            Err(ProviderError::NotSupported(Capability::CoinMarket))
        }
        async fn fetch_derivatives(&self, _m: &MarketQuery) -> Result<DerivTick, ProviderError> {
            Err(ProviderError::NotSupported(Capability::Derivatives))
        }
        async fn search_coins(
            &self,
            _q: &str,
            _cap: usize,
        ) -> Result<Vec<CoinSearchResult>, ProviderError> {
            Ok(vec![])
        }
        async fn search_markets(
            &self,
            _q: &str,
            _cap: usize,
        ) -> Result<Vec<MarketSearchResult>, ProviderError> {
            Ok(vec![])
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
        let (result, records) = chain_fetch_ohlc(&chain, &market, 7).await;

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
        let (result, records) = chain_fetch_ohlc(&chain, &market, 7).await;

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
            async fn fetch_spot(&self, _m: &MarketQuery) -> Result<SpotQuote, ProviderError> {
                Err(ProviderError::NotSupported(Capability::Spot))
            }
            async fn fetch_ohlc(
                &self,
                _m: &MarketQuery,
                _d: u32,
            ) -> Result<Vec<OhlcCandle>, ProviderError> {
                Err(ProviderError::NotSupported(Capability::Ohlc))
            }
            async fn fetch_coin_metadata(&self, _id: &str) -> Result<CoinMeta, ProviderError> {
                Err(ProviderError::NotSupported(Capability::CoinMetadata))
            }
            async fn fetch_coin_market(
                &self,
                _id: &str,
                _vs: &str,
            ) -> Result<CoinMarket, ProviderError> {
                Err(ProviderError::NotSupported(Capability::CoinMarket))
            }
            async fn fetch_derivatives(
                &self,
                _m: &MarketQuery,
            ) -> Result<DerivTick, ProviderError> {
                Err(ProviderError::NotSupported(Capability::Derivatives))
            }
            async fn search_coins(
                &self,
                _q: &str,
                _cap: usize,
            ) -> Result<Vec<CoinSearchResult>, ProviderError> {
                Ok(vec![])
            }
            async fn search_markets(
                &self,
                _q: &str,
                _cap: usize,
            ) -> Result<Vec<MarketSearchResult>, ProviderError> {
                Ok(vec![])
            }
        }

        let chain: Vec<Arc<dyn Provider>> = vec![Arc::new(UnsupportedProvider)];
        let market = stub_market();
        let (_result, records) = chain_fetch_ohlc(&chain, &market, 7).await;

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, ProviderOutcome::Unsupported);
    }
}
