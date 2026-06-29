use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sqlx::FromRow;

/// Registry entry for a tracked coin (base asset).
///
/// Keyed by `coin_id` (e.g. CoinGecko `"bitcoin"`). This is the unit of metadata and
/// tokenomics collection — not tied to a specific quote currency or trading venue (research §1.3).
///
/// # Live-poller contract columns (REQ-API-112)
///
/// `live_poll_interval` is returned as `TEXT` from DB (cast via `::TEXT` in SELECT) and holds
/// the normalized human-readable interval string (e.g. `"5m"`, `"1h30m"`). `None` = use global.
/// The full poller contract is: `last_polled_at`, `live_poll_claimed_until`, `live_poll_interval`
/// (all added via migration 0010).
///
/// @MX:ANCHOR: [AUTO] TrackedCoin live-poller contract — live_poll_interval (TEXT), last_polled_at, live_poll_claimed_until
/// @MX:REASON: SPEC-API-002 REQ-API-112 SPEC-SCHED-001 REQ-SCHED-003. All SELECT queries must cast
///             live_poll_interval::TEXT to map to Option<String>. Any schema rename breaks the coin poller.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct TrackedCoin {
    pub coin_id: String,
    pub symbol: String,
    pub name: String,
    /// status domain: active | paused | error (enforced by CHECK constraint, REQ-DB-001).
    pub status: String,
    pub registered_at: DateTime<Utc>,
    pub last_collected_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    /// Per-coin live-poll cadence override in human-readable form (e.g. "5m", "1h30m").
    /// `None` = use global `LIVE_QUOTE_POLL_INTERVAL_SECS`.
    /// Stored as PG INTERVAL; returned as TEXT via `::TEXT` cast; normalized by `poll_interval::normalize_pg_interval`.
    #[serde(default)]
    pub live_poll_interval: Option<String>,
}

/// Slowly-changing descriptive coin metadata. Keyed by `(coin_id, revision)`.
///
/// `revision` is 0-based and incremented **only** when a tracked value changes
/// (using `IS NOT DISTINCT FROM` comparison). When metadata is re-collected unchanged,
/// `last_seen_at` advances on the existing revision without a new row (REQ-DB-021).
///
/// Continuously-changing aggregates (price, market_cap, supply, FDV) are stored in
/// `CoinMarketSnapshot`, not here — to avoid revision churn on every poll (REQ-DB-022).
///
/// @MX:WARN: [AUTO] revision insert invariant — insert new revision ONLY on value change
/// @MX:REASON: A new revision must be inserted ONLY when IS NOT DISTINCT FROM detects a change.
///             Otherwise advance last_seen_at on the existing revision. Violating this causes
///             unbounded revision churn and defeats the slowly-changing pattern (REQ-DB-021, research §4.3).
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct CoinMetadata {
    pub coin_id: String,
    /// 0-based revision counter. Incremented only on real value change.
    pub revision: i32,
    pub name: String,
    pub symbol: String,
    pub categories: Option<Vec<String>>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    /// Structured links (social, repos, etc.) stored as JSONB.
    pub links: Option<JsonValue>,
    /// Contract addresses per blockchain, stored as JSONB.
    pub contract_addresses: Option<JsonValue>,
    /// Maximum supply (NULL for assets without a hard cap, e.g. ETH).
    pub max_supply: Option<Decimal>,
    pub genesis_date: Option<NaiveDate>,
    /// When this revision was first observed.
    pub first_seen_at: DateTime<Utc>,
    /// When this revision was last confirmed (advances without a new revision if unchanged).
    pub last_seen_at: DateTime<Utc>,
}

/// Continuously-changing coin market aggregates (price, cap, supply, FDV).
///
/// Partitioned by `ts` (monthly RANGE). PK: `(coin_id, vs_currency, ts)`.
/// These values change on every poll and are stored as time-series rows — NOT as revisions —
/// to avoid churn on the `coin_metadata` revision table (REQ-DB-012/022, research §4.3).
///
/// @MX:ANCHOR: [AUTO] coin_market_snapshots partition+index contract
/// @MX:REASON: btree(coin_id, vs_currency, ts DESC) + BRIN(ts) shape used by all cap/supply reads.
///             Changing the partition key or dropping this index breaks market snapshot queries.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct CoinMarketSnapshot {
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
