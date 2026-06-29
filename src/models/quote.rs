use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::{postgres::types::PgInterval, FromRow};

/// Registry entry for a tracked trading pair (base/quote/venue?).
///
/// Unique on `(base, quote, COALESCE(venue, ''))` so an aggregator-level pair (NULL venue)
/// and venue-specific pairs for the same `(base, quote)` coexist without collision (REQ-DB-003).
///
/// # Live-poller contract columns
///
/// The three columns below are the schema contract that SPEC-SCHED-001's poller consumes.
/// They must not be renamed, retyped, or removed without updating SPEC-SCHED-001.
///
/// @MX:ANCHOR: [AUTO] live-poller contract — last_polled_at, live_poll_claimed_until, live_poll_interval
/// @MX:REASON: SPEC-SCHED-001 REQ-SCHED-003 poller claim query reads these columns and the partial
///             index (last_polled_at) WHERE status='active'. Any rename/retype breaks the poller.
#[derive(Debug, Clone, FromRow)]
pub struct TrackedMarket {
    pub id: i64,
    pub base: String,
    pub quote: String,
    pub venue: Option<String>,
    pub coin_id: Option<String>,
    pub kind: String,
    /// status domain: active | paused | error (enforced by CHECK constraint, REQ-DB-002).
    pub status: String,
    pub registered_at: DateTime<Utc>,
    pub last_collected_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    /// When this market was last successfully polled (live-poller contract).
    pub last_polled_at: Option<DateTime<Utc>>,
    /// Self-expiring in-flight claim marker; NULL when not claimed (live-poller contract).
    pub live_poll_claimed_until: Option<DateTime<Utc>>,
    /// Per-market cadence override; NULL = use global `LIVE_QUOTE_POLL_INTERVAL_SECS` (live-poller contract).
    /// `PgInterval` does not implement serde — SPEC-API-001 will add a DTO with custom serialization.
    pub live_poll_interval: Option<PgInterval>,
}

/// Live spot quote snapshot for a tracked market.
///
/// Partitioned by `ts` (monthly RANGE). PK: `(market_id, ts)`.
/// All price/size/volume columns are `NUMERIC` mapped to `Decimal` (REQ-DB-040).
///
/// @MX:ANCHOR: [AUTO] live_quotes partition+index contract — btree(market_id, ts DESC) + BRIN(ts)
/// @MX:REASON: All read paths perform market_id-scoped + ts-range queries on this index shape.
///             Changing the partition key or removing the btree index breaks keyset pagination.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct LiveQuote {
    pub market_id: i64,
    pub ts: DateTime<Utc>,
    /// Provider quote instant (may differ from the capture `ts`).
    pub as_of: Option<DateTime<Utc>>,
    pub price: Decimal,
    pub bid: Option<Decimal>,
    pub ask: Option<Decimal>,
    pub bid_size: Option<Decimal>,
    pub ask_size: Option<Decimal>,
    pub volume_24h: Option<Decimal>,
    pub vs_currency: String,
    pub source: String,
}

/// OHLCV candle for a tracked market.
///
/// Partitioned by `ts` (monthly RANGE). PK: `(market_id, interval, ts)`.
/// `interval` is in the PK so `1m` and `1d` candles coexist for the same `(market_id, ts)`.
/// `volume` is nullable: CoinGecko `/coins/{id}/ohlc` returns no volume (research §2.2, REQ-DB-011).
///
/// @MX:ANCHOR: [AUTO] candles partition+index contract — btree(market_id, interval, ts DESC) + BRIN(ts)
/// @MX:REASON: All OHLCV read paths use this index shape. The interval in the PK is invariant;
///             removing it would make 1m and 1d candles collide on the same (market_id, ts) PK.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Candle {
    pub market_id: i64,
    pub interval: String,
    pub ts: DateTime<Utc>,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    /// Nullable: CoinGecko OHLC endpoint does not include per-candle volume (REQ-DB-011).
    pub volume: Option<Decimal>,
    pub vs_currency: String,
    pub source: String,
}

// ── Coin-keyed time-series models (SPEC-API-002) ─────────────────────────────

/// Coin-keyed spot price quote (SPEC-API-002 REQ-API-131/132).
///
/// Partitioned by `ts` (monthly RANGE). PK: `(coin_id, vs_currency, ts)`.
/// All price fields are `NUMERIC` mapped to `Decimal` (REQ-DB-040, REQ-PROV-012).
///
/// @MX:ANCHOR: [AUTO] coin_quotes partition+index contract — btree(coin_id, vs_currency, ts DESC) + BRIN(ts)
/// @MX:REASON: All coin-keyed quote read paths depend on this index shape (REQ-DB-015).
///             Changing partition key or removing btree breaks keyset pagination handlers in quotes.rs.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct CoinQuote {
    pub coin_id: String,
    pub vs_currency: String,
    pub ts: DateTime<Utc>,
    pub price: Decimal,
    pub source: String,
}

/// Coin-keyed OHLCV candle (SPEC-API-002 REQ-API-141/142).
///
/// Partitioned by `ts` (monthly RANGE). PK: `(coin_id, vs_currency, interval, ts)`.
/// `volume` is nullable: CoinGecko OHLC has no per-candle volume (REQ-DB-011).
///
/// @MX:ANCHOR: [AUTO] coin_candles partition+index contract — btree(coin_id, vs_currency, interval, ts DESC) + BRIN(ts)
/// @MX:REASON: All coin-keyed candle read paths depend on this index shape (REQ-DB-015).
///             The interval column is invariant in the PK; removing it collapses 1m and 1d into one row.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct CoinCandle {
    pub coin_id: String,
    pub vs_currency: String,
    pub interval: String,
    pub ts: DateTime<Utc>,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    /// Nullable: CoinGecko OHLC has no volume (REQ-DB-011).
    pub volume: Option<Decimal>,
    pub source: String,
}
