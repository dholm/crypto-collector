//! Request / response DTOs for the `/v1` REST API (SPEC-API-001).
//!
//! Every monetary or quantity field is serialized as a JSON string using
//! `rust_decimal::serde::str` (or `str_option`) to guarantee lossless round-trips
//! without any f64 intermediate conversion (REQ-API-073, OR-API-2 resolved: strings).
//!
// @MX:NOTE: [AUTO] Decimal → JSON string convention (OR-API-2 resolved)
// @MX:REASON: All monetary/quantity Decimal fields serialize as JSON strings (e.g. "0.00000000001234").
//             Never use f64 in the API path — f64 has only 53-bit mantissa precision (REQ-API-073).
//             Callers must treat these as strings; arbitrary-precision JSON libraries can parse them.
// @MX:SPEC: SPEC-API-001 REQ-API-073

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::models::{
    coin::{CoinMarketSnapshot, CoinMetadata, TrackedCoin},
    derivatives::DerivativesQuote,
    quote::{Candle, LiveQuote, TrackedMarket},
};

// ── Shared pagination wrapper ─────────────────────────────────────────────────

/// Generic paginated response page.
#[derive(Debug, Serialize)]
pub struct Page<T: Serialize> {
    pub items: Vec<T>,
    /// Opaque cursor for the next page; `null` when exhausted (REQ-API-070).
    pub next_cursor: Option<String>,
}

// ── Uniform error body (REQ-API-074) ─────────────────────────────────────────

/// Uniform JSON error body for 400 / 404 / 422 / 500 responses.
#[derive(Debug, Serialize)]
pub struct ApiErrorBody {
    /// Machine-readable error code (e.g. `"NOT_FOUND"`, `"BAD_REQUEST"`).
    pub code: &'static str,
    /// Human-readable description.
    pub message: String,
}

// ── Coin management DTOs ──────────────────────────────────────────────────────

/// Response DTO for a tracked coin.
#[derive(Debug, Serialize)]
pub struct CoinDto {
    pub coin_id: String,
    pub symbol: String,
    pub name: String,
    pub status: String,
    pub registered_at: DateTime<Utc>,
    pub last_collected_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

impl From<TrackedCoin> for CoinDto {
    fn from(c: TrackedCoin) -> Self {
        Self {
            coin_id: c.coin_id,
            symbol: c.symbol,
            name: c.name,
            status: c.status,
            registered_at: c.registered_at,
            last_collected_at: c.last_collected_at,
            error: c.error,
        }
    }
}

/// Request body for `POST /v1/coins`.
#[derive(Debug, Deserialize)]
pub struct RegisterCoinRequest {
    pub coin_id: String,
    pub symbol: String,
    pub name: String,
}

/// Request body for `PATCH /v1/coins/{coin_id}`.
#[derive(Debug, Deserialize)]
pub struct UpdateCoinRequest {
    pub status: Option<String>,
    pub error: Option<String>,
}

// CoinSearchResult is defined in the providers layer so that `CoinGeckoClient::search_coins`
// and the API handler share one type without a circular dependency.
pub use crate::providers::CoinSearchResult;

/// Response DTO for coin search page.
#[derive(Debug, Serialize)]
pub struct CoinSearchPage {
    pub items: Vec<CoinSearchResult>,
}

// ── Market management DTOs ────────────────────────────────────────────────────

/// Response DTO for a tracked market.
///
/// `TrackedMarket` uses `PgInterval` for `live_poll_interval` which does not implement
/// `Serialize`; this DTO converts it to milliseconds (SPEC-API-001 plan.md DTO note).
#[derive(Debug, Serialize)]
pub struct MarketDto {
    pub id: i64,
    pub base: String,
    pub quote: String,
    pub venue: Option<String>,
    pub coin_id: Option<String>,
    pub kind: String,
    pub status: String,
    pub registered_at: DateTime<Utc>,
    pub last_collected_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    /// Per-market live poll interval in milliseconds; `null` = use global default.
    pub live_poll_interval_ms: Option<i64>,
}

impl From<TrackedMarket> for MarketDto {
    fn from(m: TrackedMarket) -> Self {
        let live_poll_interval_ms = m.live_poll_interval.map(|iv| {
            // PgInterval: months + days + microseconds → milliseconds
            let months_ms = i64::from(iv.months) * 30 * 24 * 3600 * 1000;
            let days_ms = i64::from(iv.days) * 24 * 3600 * 1000;
            let us_ms = iv.microseconds / 1000;
            months_ms + days_ms + us_ms
        });
        Self {
            id: m.id,
            base: m.base,
            quote: m.quote,
            venue: m.venue,
            coin_id: m.coin_id,
            kind: m.kind,
            status: m.status,
            registered_at: m.registered_at,
            last_collected_at: m.last_collected_at,
            error: m.error,
            live_poll_interval_ms,
        }
    }
}

/// Request body for `POST /v1/markets`.
#[derive(Debug, Deserialize)]
pub struct RegisterMarketRequest {
    pub base: String,
    pub quote: String,
    pub venue: Option<String>,
    pub coin_id: Option<String>,
    pub kind: Option<String>,
}

/// Request body for `PATCH /v1/markets/{id}`.
#[derive(Debug, Deserialize)]
pub struct UpdateMarketRequest {
    pub status: Option<String>,
    pub error: Option<String>,
    /// Per-market poll interval in milliseconds; `null` = revert to global default.
    pub live_poll_interval_ms: Option<i64>,
}

// MarketSearchResult is defined in the providers layer so that `CoinGeckoClient::search_markets`
// and the API handler share one type without a circular dependency.
pub use crate::providers::MarketSearchResult;

/// Response DTO for market search page.
#[derive(Debug, Serialize)]
pub struct MarketSearchPage {
    pub items: Vec<MarketSearchResult>,
}

// ── Spot quote DTOs ───────────────────────────────────────────────────────────

/// Response DTO for a spot quote.
#[derive(Debug, Serialize)]
pub struct QuoteDto {
    pub market_id: i64,
    pub ts: DateTime<Utc>,
    pub as_of: Option<DateTime<Utc>>,
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub bid: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub ask: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub bid_size: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub ask_size: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub volume_24h: Option<Decimal>,
    pub vs_currency: String,
    pub source: String,
}

impl From<LiveQuote> for QuoteDto {
    fn from(q: LiveQuote) -> Self {
        Self {
            market_id: q.market_id,
            ts: q.ts,
            as_of: q.as_of,
            price: q.price,
            bid: q.bid,
            ask: q.ask,
            bid_size: q.bid_size,
            ask_size: q.ask_size,
            volume_24h: q.volume_24h,
            vs_currency: q.vs_currency,
            source: q.source,
        }
    }
}

// ── Candle DTOs ───────────────────────────────────────────────────────────────

/// Response DTO for an OHLCV candle.
///
/// `volume` is nullable (CoinGecko OHLC has no per-candle volume; REQ-API-042).
#[derive(Debug, Serialize)]
pub struct CandleDto {
    pub market_id: i64,
    pub interval: String,
    pub ts: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::str")]
    pub open: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub high: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub low: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub close: Decimal,
    /// Nullable: `null` for CoinGecko-sourced candles (REQ-API-042).
    #[serde(with = "rust_decimal::serde::str_option")]
    pub volume: Option<Decimal>,
    pub vs_currency: String,
    pub source: String,
}

impl From<Candle> for CandleDto {
    fn from(c: Candle) -> Self {
        Self {
            market_id: c.market_id,
            interval: c.interval,
            ts: c.ts,
            open: c.open,
            high: c.high,
            low: c.low,
            close: c.close,
            volume: c.volume,
            vs_currency: c.vs_currency,
            source: c.source,
        }
    }
}

// ── Coin metadata DTOs ────────────────────────────────────────────────────────

/// Response DTO for coin metadata.
#[derive(Debug, Serialize)]
pub struct CoinMetadataDto {
    pub coin_id: String,
    pub revision: i32,
    pub name: String,
    pub symbol: String,
    pub categories: Option<Vec<String>>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub links: Option<serde_json::Value>,
    pub contract_addresses: Option<serde_json::Value>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub max_supply: Option<Decimal>,
    pub genesis_date: Option<chrono::NaiveDate>,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

impl From<CoinMetadata> for CoinMetadataDto {
    fn from(m: CoinMetadata) -> Self {
        Self {
            coin_id: m.coin_id,
            revision: m.revision,
            name: m.name,
            symbol: m.symbol,
            categories: m.categories,
            description: m.description,
            homepage: m.homepage,
            links: m.links,
            contract_addresses: m.contract_addresses,
            max_supply: m.max_supply,
            genesis_date: m.genesis_date,
            first_seen_at: m.first_seen_at,
            last_seen_at: m.last_seen_at,
        }
    }
}

// ── Coin market aggregate DTOs ────────────────────────────────────────────────

/// Response DTO for a coin market snapshot.
#[derive(Debug, Serialize)]
pub struct CoinMarketSnapshotDto {
    pub coin_id: String,
    pub vs_currency: String,
    pub ts: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub market_cap: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub fully_diluted_valuation: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub circulating_supply: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub total_supply: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub volume_24h: Option<Decimal>,
    pub source: String,
}

impl From<CoinMarketSnapshot> for CoinMarketSnapshotDto {
    fn from(s: CoinMarketSnapshot) -> Self {
        Self {
            coin_id: s.coin_id,
            vs_currency: s.vs_currency,
            ts: s.ts,
            price: s.price,
            market_cap: s.market_cap,
            fully_diluted_valuation: s.fully_diluted_valuation,
            circulating_supply: s.circulating_supply,
            total_supply: s.total_supply,
            volume_24h: s.volume_24h,
            source: s.source,
        }
    }
}

// ── Derivatives DTOs ──────────────────────────────────────────────────────────

/// Response DTO for a derivatives tick.
#[derive(Debug, Serialize)]
pub struct DerivativesQuoteDto {
    pub market_id: i64,
    pub ts: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub funding_rate: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub open_interest: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub open_interest_usd: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub mark_price: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub index_price: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub basis: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub volume_24h: Option<Decimal>,
    pub contract_type: Option<String>,
    pub venue: Option<String>,
    pub source: String,
}

impl From<DerivativesQuote> for DerivativesQuoteDto {
    fn from(d: DerivativesQuote) -> Self {
        Self {
            market_id: d.market_id,
            ts: d.ts,
            funding_rate: d.funding_rate,
            open_interest: d.open_interest,
            open_interest_usd: d.open_interest_usd,
            mark_price: d.mark_price,
            index_price: d.index_price,
            basis: d.basis,
            volume_24h: d.volume_24h,
            contract_type: d.contract_type,
            venue: d.venue,
            source: d.source,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    // Scenario 12 (REQ-API-073): Decimal fields serialize as strings, not numbers.
    #[test]
    fn quote_dto_decimal_serializes_as_string() {
        let dto = QuoteDto {
            market_id: 1,
            ts: chrono::Utc::now(),
            as_of: None,
            price: dec!(0.00000000001234),
            bid: None,
            ask: None,
            bid_size: None,
            ask_size: None,
            volume_24h: Some(dec!(589000000000000)),
            vs_currency: "usd".into(),
            source: "test".into(),
        };
        let json = serde_json::to_string(&dto).unwrap();
        // Price must appear as string, not a number
        assert!(
            json.contains(r#""price":"0.00000000001234""#),
            "price must serialize as JSON string; got: {json}"
        );
        // Large supply must also be exact string
        assert!(
            json.contains(r#""volume_24h":"589000000000000""#),
            "large decimal must serialize as JSON string; got: {json}"
        );
    }

    // Candle volume null → serializes as null, not "0"
    #[test]
    fn candle_dto_null_volume_serializes_as_null() {
        let dto = CandleDto {
            market_id: 1,
            interval: "1h".into(),
            ts: chrono::Utc::now(),
            open: dec!(100),
            high: dec!(110),
            low: dec!(90),
            close: dec!(105),
            volume: None,
            vs_currency: "usd".into(),
            source: "coingecko".into(),
        };
        let json = serde_json::to_string(&dto).unwrap();
        assert!(
            json.contains(r#""volume":null"#),
            "null volume must serialize as null; got: {json}"
        );
    }

    // PgInterval → milliseconds conversion
    #[test]
    fn market_dto_converts_pg_interval_to_ms() {
        use sqlx::postgres::types::PgInterval;
        let market = TrackedMarket {
            id: 1,
            base: "BTC".into(),
            quote: "USD".into(),
            venue: None,
            coin_id: None,
            kind: "spot".into(),
            status: "active".into(),
            registered_at: chrono::Utc::now(),
            last_collected_at: None,
            error: None,
            last_polled_at: None,
            live_poll_claimed_until: None,
            live_poll_interval: Some(PgInterval {
                months: 0,
                days: 0,
                microseconds: 60_000_000, // 60s = 60000ms
            }),
        };
        let dto = MarketDto::from(market);
        assert_eq!(dto.live_poll_interval_ms, Some(60_000));
    }

    // PgInterval None → None
    #[test]
    fn market_dto_null_interval_stays_null() {
        use sqlx::postgres::types::PgInterval;
        let market = TrackedMarket {
            id: 2,
            base: "ETH".into(),
            quote: "USD".into(),
            venue: None,
            coin_id: None,
            kind: "spot".into(),
            status: "active".into(),
            registered_at: chrono::Utc::now(),
            last_collected_at: None,
            error: None,
            last_polled_at: None,
            live_poll_claimed_until: None,
            live_poll_interval: None::<PgInterval>,
        };
        let dto = MarketDto::from(market);
        assert_eq!(dto.live_poll_interval_ms, None);
    }

    // CoinMarketSnapshotDto Decimal fields serialize as strings
    #[test]
    fn coin_market_snapshot_dto_price_is_string() {
        let dto = CoinMarketSnapshotDto {
            coin_id: "bitcoin".into(),
            vs_currency: "usd".into(),
            ts: chrono::Utc::now(),
            price: dec!(67123.456789),
            market_cap: Some(dec!(1320000000000)),
            fully_diluted_valuation: None,
            circulating_supply: None,
            total_supply: None,
            volume_24h: None,
            source: "coingecko".into(),
        };
        let json = serde_json::to_string(&dto).unwrap();
        assert!(
            json.contains(r#""price":"67123.456789""#),
            "price must be a JSON string; got: {json}"
        );
    }
}
