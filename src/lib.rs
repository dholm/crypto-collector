/// Crypto Collector library crate.
///
/// SPEC-DB-001: db (pool + migration runner) and models (schema-mapped structs).
/// SPEC-PROV-001: config (env-var loading), pacer (credit-aware upstream throttle),
///                providers (Provider trait + CoinGecko/Binance/Coinbase/Kraken chain).
/// SPEC-SCHED-001: collectors (live-quote poller, collection-queue worker, backfill worker).
/// SPEC-API-001: api (REST API server, /v1 router, OpenAPI v3.1 deliverable).
/// SPEC-API-002: listener (PG LISTEN/NOTIFY relay for WebSocket broadcast channels).
/// SPEC-OBS-001: health (liveness/readiness), metrics (Prometheus), telemetry (OTel + JSON logs).
/// SPEC-ALARM-001: alarm (AlarmClient + fingerprint/condition catalogue; Batch 1 scope).
pub mod alarm;
pub mod api;
pub mod collectors;
pub mod config;
pub mod db;
pub mod health;
pub mod listener;
pub mod metrics;
pub mod models;
pub mod pacer;
pub mod providers;
pub mod telemetry;
