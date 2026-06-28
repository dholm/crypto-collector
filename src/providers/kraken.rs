//! Kraken exchange provider stub (SPEC-PROV-001).
//!
//! Valid member of the provider chain (REQ-PROV-002/003) but returns `NotSupported` for all
//! capabilities in this SPEC scope. Full implementation is deferred to a future SPEC.

use super::{
    Capability, CoinMarket, CoinMeta, CoinSearchResult, DerivTick, MarketQuery, MarketSearchResult,
    OhlcCandle, Provider, ProviderError, SpotQuote,
};
use async_trait::async_trait;
use sqlx::PgPool;

/// Kraken provider stub — valid chain member, all capabilities `NotSupported`.
pub struct KrakenProvider {
    _pool: PgPool,
}

impl KrakenProvider {
    pub fn new(pool: PgPool) -> Self {
        Self { _pool: pool }
    }
}

#[async_trait]
impl Provider for KrakenProvider {
    fn name(&self) -> &str {
        "kraken"
    }

    fn supports(&self, _cap: Capability) -> bool {
        false
    }

    async fn fetch_spot(&self, _market: &MarketQuery) -> Result<SpotQuote, ProviderError> {
        Err(ProviderError::NotSupported(Capability::Spot))
    }

    async fn fetch_ohlc(
        &self,
        _market: &MarketQuery,
        _days: u32,
    ) -> Result<Vec<OhlcCandle>, ProviderError> {
        Err(ProviderError::NotSupported(Capability::Ohlc))
    }

    async fn fetch_coin_metadata(&self, _coin_id: &str) -> Result<CoinMeta, ProviderError> {
        Err(ProviderError::NotSupported(Capability::CoinMetadata))
    }

    async fn fetch_coin_market(
        &self,
        _coin_id: &str,
        _vs_currency: &str,
    ) -> Result<CoinMarket, ProviderError> {
        Err(ProviderError::NotSupported(Capability::CoinMarket))
    }

    async fn fetch_derivatives(&self, _market: &MarketQuery) -> Result<DerivTick, ProviderError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn kraken_name_is_kraken() {
        let pool =
            sqlx::PgPool::connect_lazy("postgres://postgres@localhost/crypto_collector_test")
                .expect("lazy pool");
        let p = KrakenProvider::new(pool);
        assert_eq!(p.name(), "kraken");
    }

    #[tokio::test]
    async fn kraken_supports_nothing() {
        let pool =
            sqlx::PgPool::connect_lazy("postgres://postgres@localhost/crypto_collector_test")
                .expect("lazy pool");
        let p = KrakenProvider::new(pool);
        for cap in [
            Capability::Spot,
            Capability::Ohlc,
            Capability::CoinMetadata,
            Capability::CoinMarket,
            Capability::Derivatives,
        ] {
            assert!(!p.supports(cap), "Kraken stub must not support {cap:?}");
        }
    }
}
