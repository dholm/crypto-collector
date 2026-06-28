/// Crypto Collector library crate.
///
/// SPEC-DB-001: db (pool + migration runner) and models (schema-mapped structs).
/// SPEC-PROV-001: config (env-var loading), pacer (credit-aware upstream throttle),
///                providers (Provider trait + CoinGecko/Binance/Coinbase/Kraken chain).
pub mod config;
pub mod db;
pub mod models;
pub mod pacer;
pub mod providers;
