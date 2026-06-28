use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// Per-tick derivatives snapshot (perpetuals and futures).
///
/// All continuously-moving derivative observables — funding rate, open interest, mark/index
/// price, and basis — are captured in ONE atomic row per `(market_id, ts)`. There is no
/// separate funding-rate table or open-interest table (REQ-DB-013, research §1.4).
///
/// Matches the CoinGecko `/derivatives/tickers` response shape, which returns all of these
/// fields together per derivative ticker.
///
/// Partitioned by `ts` (monthly RANGE). PK: `(market_id, ts)`.
/// All quantity columns are `NUMERIC` mapped to `Decimal` (REQ-DB-040).
///
/// @MX:ANCHOR: [AUTO] derivatives_quotes partition+index contract — btree(market_id, ts DESC) + BRIN(ts)
/// @MX:REASON: All derivatives read paths depend on this index shape. The single-table design
///             (no separate funding/OI tables) is invariant — altering it requires migrating
///             all consumers (research §1.4, REQ-DB-013).
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct DerivativesQuote {
    pub market_id: i64,
    pub ts: DateTime<Utc>,
    /// Periodic payment rate between longs and shorts; can be negative (research §1.4).
    pub funding_rate: Option<Decimal>,
    /// Total notional value of open positions (in contract units).
    pub open_interest: Option<Decimal>,
    /// Open interest denominated in USD.
    pub open_interest_usd: Option<Decimal>,
    /// Exchange's mark price (used for liquidation calculations).
    pub mark_price: Option<Decimal>,
    /// Broad index average price (spot reference).
    pub index_price: Option<Decimal>,
    /// Difference between mark price and index price (mark - index).
    pub basis: Option<Decimal>,
    pub volume_24h: Option<Decimal>,
    /// e.g. "perpetual", "futures"
    pub contract_type: Option<String>,
    /// Exchange/venue where this derivative trades (may differ from market.venue).
    pub venue: Option<String>,
    pub source: String,
}
