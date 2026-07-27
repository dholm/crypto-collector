//! Kraken exchange provider stub (SPEC-PROV-001).
//!
//! Valid member of the provider chain (REQ-PROV-002/003) that supports no capability in
//! this SPEC scope: every fetch method resolves through the `Provider` trait's
//! capability-derived defaults (SPEC-REFACTOR-001 M1, F-50). Full implementation is
//! deferred to a future SPEC.

use super::{Capability, Provider};
use async_trait::async_trait;
use sqlx::PgPool;

/// Kraken provider stub — valid chain member, supports no capability (all fetch methods
/// resolve to the `Provider` trait defaults).
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
