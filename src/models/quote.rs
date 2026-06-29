use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

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
