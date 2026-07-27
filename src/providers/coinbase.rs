//! Coinbase exchange provider stub (SPEC-PROV-001).
//!
//! Valid member of the provider chain (REQ-PROV-002/003) that supports no capability in
//! this SPEC scope: every fetch method resolves through the `Provider` trait's
//! capability-derived defaults (SPEC-REFACTOR-001 M1, F-50). Full implementation is
//! deferred to a future SPEC.

use super::{Capability, Provider};
use async_trait::async_trait;
use sqlx::PgPool;

/// Coinbase provider stub — valid chain member, supports no capability (all fetch methods
/// resolve to the `Provider` trait defaults).
pub struct CoinbaseProvider {
    _pool: PgPool,
}

impl CoinbaseProvider {
    pub fn new(pool: PgPool) -> Self {
        Self { _pool: pool }
    }
}

#[async_trait]
impl Provider for CoinbaseProvider {
    fn name(&self) -> &str {
        "coinbase"
    }

    fn supports(&self, _cap: Capability) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn coinbase_name_is_coinbase() {
        let pool =
            sqlx::PgPool::connect_lazy("postgres://postgres@localhost/crypto_collector_test")
                .expect("lazy pool");
        let p = CoinbaseProvider::new(pool);
        assert_eq!(p.name(), "coinbase");
    }

    #[tokio::test]
    async fn coinbase_supports_nothing() {
        let pool =
            sqlx::PgPool::connect_lazy("postgres://postgres@localhost/crypto_collector_test")
                .expect("lazy pool");
        let p = CoinbaseProvider::new(pool);
        for cap in [
            Capability::Spot,
            Capability::Ohlc,
            Capability::CoinMetadata,
            Capability::CoinMarket,
            Capability::Derivatives,
        ] {
            assert!(!p.supports(cap), "Coinbase stub must not support {cap:?}");
        }
    }
}
