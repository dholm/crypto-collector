//! Database layer: connection pool, migrations, and upsert helpers (SPEC-DB-001).
//!
//! # Shared batched candle upsert (SPEC-REFACTOR-001 M4)
//!
//! [`batched_upsert_coin_candles`] is the single UNNEST-based batch write shared by the
//! candles dispatch path (`collectors::collection_queue`) and the backfill page-write
//! path (`collectors::backfill`). It carries a [`CandleConflictPolicy`] parameter that
//! keeps two conflict policies deliberately distinct: `NativeUnconditional` (the native
//! write path, unconditional `DO UPDATE`) and `RollupNativeWins` (the rollup path's
//! `WHERE coin_candles.source LIKE 'rollup:%'` guard) — the two are never conflated
//! (D1). A [`CandleNotifyPolicy`] parameter controls whether the write emits
//! `pg_notify`: the live-poll path emits one per event; the backfill path emits none.

pub mod candles;
pub mod pool;
pub mod upserts;

pub use candles::interval_coverage;
pub use pool::{connect, connect_lazy, connect_lazy_with, max_connections, migrate_with_retry};
pub use upserts::{
    batched_upsert_coin_candles, metadata_has_changed, upsert_coin_market_snapshot,
    upsert_coin_metadata, CandleConflictPolicy, CandleNotifyPolicy, LatestMetadata,
};
