//! Request / response DTOs for the `/v1` REST API (SPEC-API-001, SPEC-API-002).
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
use serde::{Deserialize, Deserializer, Serialize};

use crate::models::{
    coin::{CoinMarketSnapshot, CoinMetadata, TrackedCoin},
    cycle_overlay::CycleOverlayPoint,
    quote::{CoinCandle, CoinQuote},
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

/// Response DTO for a tracked coin (SPEC-API-002 REQ-API-112).
#[derive(Debug, Serialize)]
pub struct CoinDto {
    pub coin_id: String,
    pub symbol: String,
    pub name: String,
    pub status: String,
    pub registered_at: DateTime<Utc>,
    pub last_collected_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    /// Per-coin live-poll cadence override (e.g. "5m", "1h30m").
    /// `null` = use global `LIVE_QUOTE_POLL_INTERVAL_SECS`.
    pub live_poll_interval: Option<String>,
}

impl From<TrackedCoin> for CoinDto {
    fn from(c: TrackedCoin) -> Self {
        // Normalize raw PG interval text (e.g. "00:05:00") to human-readable (e.g. "5m").
        let live_poll_interval = c
            .live_poll_interval
            .as_deref()
            .and_then(super::poll_interval::normalize_pg_interval);
        Self {
            coin_id: c.coin_id,
            symbol: c.symbol,
            name: c.name,
            status: c.status,
            registered_at: c.registered_at,
            last_collected_at: c.last_collected_at,
            error: c.error,
            live_poll_interval,
        }
    }
}

/// Request body for `POST /v1/coins` (SPEC-API-002 REQ-API-112).
#[derive(Debug, Deserialize)]
pub struct RegisterCoinRequest {
    pub coin_id: String,
    pub symbol: String,
    pub name: String,
    /// Optional per-coin live-poll interval (e.g. "5m"). Must satisfy bounds (REQ-API-114).
    pub live_poll_interval: Option<String>,
}

/// Request body for `PATCH /v1/coins/{coin_id}` (SPEC-API-002 REQ-API-112).
///
/// # Tri-state semantics for `live_poll_interval`
///
/// - Absent in JSON (field not present): leave existing value unchanged.
/// - `null` in JSON: reset to global default (set DB column to NULL).
/// - String value: parse, validate bounds, set new per-coin interval.
///
/// This uses `Option<Option<String>>` where:
/// - `None` (outer) = field was absent from request body.
/// - `Some(None)` = field was explicitly set to `null`.
/// - `Some(Some(s))` = field was set to a string value.
#[derive(Debug, Deserialize)]
pub struct UpdateCoinRequest {
    pub status: Option<String>,
    pub error: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub live_poll_interval: Option<Option<String>>,
}

/// Custom deserializer for tri-state `Option<Option<T>>` fields.
///
/// When the JSON key is absent, serde uses the `#[serde(default)]` → `None`.
/// When the JSON key is present (even if `null`), this deserializer is invoked and returns `Some(...)`.
fn deserialize_optional_field<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Ok(Some(Option::deserialize(deserializer)?))
}

// CoinSearchResult is defined in the providers layer so that `CoinGeckoClient::search_coins`
// and the API handler share one type without a circular dependency.
pub use crate::providers::CoinSearchResult;

/// Response DTO for coin search page.
#[derive(Debug, Serialize)]
pub struct CoinSearchPage {
    pub items: Vec<CoinSearchResult>,
}

// ── Coin spot quote DTOs (SPEC-API-002 REQ-API-131/132) ───────────────────────

/// Response DTO for a coin-keyed spot price quote.
///
/// `price` serializes as a JSON string (REQ-API-073, OR-API-2).
#[derive(Debug, Serialize)]
pub struct CoinQuoteDto {
    pub coin_id: String,
    pub vs_currency: String,
    pub ts: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    pub source: String,
}

impl From<CoinQuote> for CoinQuoteDto {
    fn from(q: CoinQuote) -> Self {
        Self {
            coin_id: q.coin_id,
            vs_currency: q.vs_currency,
            ts: q.ts,
            price: q.price,
            source: q.source,
        }
    }
}

// ── Coin OHLCV candle DTOs (SPEC-API-002 REQ-API-141/142) ────────────────────

/// Response DTO for a coin-keyed OHLCV candle.
///
/// `volume` is nullable (CoinGecko OHLC has no per-candle volume; REQ-API-042).
/// All price fields serialize as JSON strings (REQ-API-073).
#[derive(Debug, Serialize)]
pub struct CoinCandleDto {
    pub coin_id: String,
    pub vs_currency: String,
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
    pub source: String,
}

impl From<CoinCandle> for CoinCandleDto {
    fn from(c: CoinCandle) -> Self {
        Self {
            coin_id: c.coin_id,
            vs_currency: c.vs_currency,
            interval: c.interval,
            ts: c.ts,
            open: c.open,
            high: c.high,
            low: c.low,
            close: c.close,
            volume: c.volume,
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

// ── Bitcoin halving-cycle overlay DTOs (SPEC-CYCLE-001 REQ-CYCLE-050/051) ────

/// Response DTO for a Bitcoin halving-cycle overlay point.
///
/// `price`, `norm_halving`, `norm_cycle_low` serialize as JSON strings (REQ-CYCLE-024/051,
/// OR-API-2 convention). `halving_baseline_approximate` is `true` when the cycle's
/// halving-day anchor was forward-searched because the exact halving-date candle was
/// absent (D8, REQ-CYCLE-032) — this cycle's `norm_halving` values are still well-defined
/// (anchor day = 1.0) but the anchor is not the true halving date.
#[derive(Debug, Serialize)]
pub struct CycleOverlayPointDto {
    pub coin_id: String,
    pub vs_currency: String,
    pub cycle_number: i32,
    pub halving_date: chrono::NaiveDate,
    pub days_since_halving: i32,
    pub ts: chrono::NaiveDate,
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub norm_halving: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub norm_cycle_low: Decimal,
    pub halving_baseline_approximate: bool,
    /// `true` when this point is a forward projection produced by the composite
    /// projection model (SPEC-CYCLE-001 v0.4.0, REQ-CYCLE-060) rather than a real
    /// observed daily candle. Additive/backward-compatible field.
    pub projected: bool,
    /// P10 confidence band, projected points only (REQ-CYCLE-064); `null` on real points.
    /// Additive/backward-compatible field.
    #[serde(with = "rust_decimal::serde::str_option")]
    pub price_low: Option<Decimal>,
    /// P90 confidence band, projected points only (REQ-CYCLE-064); `null` on real points.
    /// Additive/backward-compatible field.
    #[serde(with = "rust_decimal::serde::str_option")]
    pub price_high: Option<Decimal>,
}

impl From<CycleOverlayPoint> for CycleOverlayPointDto {
    fn from(p: CycleOverlayPoint) -> Self {
        Self {
            coin_id: p.coin_id,
            vs_currency: p.vs_currency,
            cycle_number: p.cycle_number,
            halving_date: p.halving_date,
            days_since_halving: p.days_since_halving,
            ts: p.ts,
            price: p.price,
            norm_halving: p.norm_halving,
            norm_cycle_low: p.norm_cycle_low,
            halving_baseline_approximate: p.halving_baseline_approximate,
            projected: p.projected,
            price_low: p.price_low,
            price_high: p.price_high,
        }
    }
}

// ── Cycle-projection model discovery DTOs (SPEC-CYCLE-001 v0.6.0, REQ-CYCLE-095/096/097) ──

/// One selectable projection model entry in the discovery response.
#[derive(Debug, Serialize)]
pub struct CycleProjectionModelDto {
    /// The `{model}` path value (`"replay"` or `"composite"`).
    pub id: String,
    /// Human-readable description of the model.
    pub description: String,
    /// `true` for `composite` (P10/P90 bands), `false` for `replay` (no bands).
    pub has_confidence_bands: bool,
}

/// `GET /v1/coins/{coin_id}/cycle-projection` (base path) response body (REQ-CYCLE-095/096).
///
/// Lists exactly two selectable models; `real` (the always-included baseline of the data
/// endpoint) is never listed here (REQ-CYCLE-097).
#[derive(Debug, Serialize)]
pub struct CycleProjectionModelsDto {
    pub models: Vec<CycleProjectionModelDto>,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    // Scenario 12 (REQ-API-073): CoinQuoteDto price serializes as string.
    #[test]
    fn coin_quote_dto_price_serializes_as_string() {
        let dto = CoinQuoteDto {
            coin_id: "bitcoin".into(),
            vs_currency: "usd".into(),
            ts: chrono::Utc::now(),
            price: dec!(0.00000000001234),
            source: "test".into(),
        };
        let json = serde_json::to_string(&dto).unwrap();
        assert!(
            json.contains(r#""price":"0.00000000001234""#),
            "price must serialize as JSON string; got: {json}"
        );
    }

    // CoinCandleDto: null volume serializes as null.
    #[test]
    fn coin_candle_dto_null_volume_serializes_as_null() {
        let dto = CoinCandleDto {
            coin_id: "bitcoin".into(),
            vs_currency: "usd".into(),
            interval: "1h".into(),
            ts: chrono::Utc::now(),
            open: dec!(100),
            high: dec!(110),
            low: dec!(90),
            close: dec!(105),
            volume: None,
            source: "coingecko".into(),
        };
        let json = serde_json::to_string(&dto).unwrap();
        assert!(
            json.contains(r#""volume":null"#),
            "null volume must serialize as null; got: {json}"
        );
    }

    // CoinMarketSnapshotDto: Decimal fields serialize as strings.
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

    // UpdateCoinRequest: tri-state deserialization.
    #[test]
    fn update_coin_request_absent_field_is_none_outer() {
        let json = r#"{"status": "active"}"#;
        let req: UpdateCoinRequest = serde_json::from_str(json).unwrap();
        assert!(
            req.live_poll_interval.is_none(),
            "absent field must be None (outer)"
        );
    }

    #[test]
    fn update_coin_request_null_field_is_some_none() {
        let json = r#"{"live_poll_interval": null}"#;
        let req: UpdateCoinRequest = serde_json::from_str(json).unwrap();
        assert_eq!(
            req.live_poll_interval,
            Some(None),
            "null field must be Some(None)"
        );
    }

    #[test]
    fn update_coin_request_string_field_is_some_some() {
        let json = r#"{"live_poll_interval": "5m"}"#;
        let req: UpdateCoinRequest = serde_json::from_str(json).unwrap();
        assert_eq!(
            req.live_poll_interval,
            Some(Some("5m".to_string())),
            "string field must be Some(Some(s))"
        );
    }

    // CoinDto: normalize_pg_interval applied in From<TrackedCoin>.
    #[test]
    fn coin_dto_from_tracked_coin_normalizes_interval() {
        use crate::models::coin::TrackedCoin;
        let coin = TrackedCoin {
            coin_id: "bitcoin".into(),
            symbol: "BTC".into(),
            name: "Bitcoin".into(),
            status: "active".into(),
            registered_at: chrono::Utc::now(),
            last_collected_at: None,
            error: None,
            live_poll_interval: Some("00:05:00".to_string()),
        };
        let dto = CoinDto::from(coin);
        assert_eq!(dto.live_poll_interval, Some("5m".to_string()));
    }

    #[test]
    fn coin_dto_from_tracked_coin_null_interval_stays_null() {
        use crate::models::coin::TrackedCoin;
        let coin = TrackedCoin {
            coin_id: "bitcoin".into(),
            symbol: "BTC".into(),
            name: "Bitcoin".into(),
            status: "active".into(),
            registered_at: chrono::Utc::now(),
            last_collected_at: None,
            error: None,
            live_poll_interval: None,
        };
        let dto = CoinDto::from(coin);
        assert_eq!(dto.live_poll_interval, None);
    }

    // ── CycleOverlayPointDto (SPEC-CYCLE-001) ─────────────────────────────────

    fn sample_cycle_overlay_point() -> CycleOverlayPoint {
        CycleOverlayPoint {
            coin_id: "bitcoin".into(),
            vs_currency: "usd".into(),
            cycle_number: 3,
            halving_date: chrono::NaiveDate::from_ymd_opt(2020, 5, 11).unwrap(),
            days_since_halving: 200,
            ts: chrono::NaiveDate::from_ymd_opt(2020, 11, 27).unwrap(),
            price: dec!(4000),
            norm_halving: dec!(0.465116279069767441860465116),
            norm_cycle_low: dec!(1),
            halving_baseline_approximate: false,
            projected: false,
            price_low: None,
            price_high: None,
        }
    }

    // Scenario 6 (REQ-CYCLE-022/050): both baselines present on every point.
    #[test]
    fn cycle_overlay_dto_serializes_both_baselines_as_strings() {
        let dto = CycleOverlayPointDto::from(sample_cycle_overlay_point());
        let json = serde_json::to_string(&dto).unwrap();
        assert!(json.contains(r#""norm_halving":"0.465116279069767441860465116""#));
        assert!(json.contains(r#""norm_cycle_low":"1""#));
        assert!(json.contains(r#""price":"4000""#));
    }

    // REQ-CYCLE-064: real points carry null bands; projected points carry DecimalStrings.
    #[test]
    fn cycle_overlay_dto_band_fields_serialize_null_and_string() {
        let dto = CycleOverlayPointDto::from(sample_cycle_overlay_point());
        let json = serde_json::to_string(&dto).unwrap();
        assert!(json.contains(r#""price_low":null"#));
        assert!(json.contains(r#""price_high":null"#));

        let mut projected = sample_cycle_overlay_point();
        projected.projected = true;
        projected.price_low = Some(dec!(3000));
        projected.price_high = Some(dec!(6000));
        let json = serde_json::to_string(&CycleOverlayPointDto::from(projected)).unwrap();
        assert!(json.contains(r#""price_low":"3000""#));
        assert!(json.contains(r#""price_high":"6000""#));
    }

    #[test]
    fn cycle_overlay_dto_from_model_preserves_approximate_flag() {
        let mut point = sample_cycle_overlay_point();
        point.halving_baseline_approximate = true;
        let dto = CycleOverlayPointDto::from(point);
        assert!(dto.halving_baseline_approximate);
    }

    // ── CycleProjectionModelsDto (SPEC-CYCLE-001 v0.6.0, REQ-CYCLE-095/096/097) ─────────────

    // Scenario 31 (REQ-CYCLE-096/097): discovery DTO serializes both models with the
    // correct has_confidence_bands and never a "real" entry.
    #[test]
    fn cycle_projection_models_dto_serializes_two_models_no_real() {
        let dto = CycleProjectionModelsDto {
            models: vec![
                CycleProjectionModelDto {
                    id: "replay".to_string(),
                    description: "replay model".to_string(),
                    has_confidence_bands: false,
                },
                CycleProjectionModelDto {
                    id: "composite".to_string(),
                    description: "composite model".to_string(),
                    has_confidence_bands: true,
                },
            ],
        };
        let json = serde_json::to_value(&dto).unwrap();
        let models = json["models"].as_array().expect("models array");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"], "replay");
        assert_eq!(models[0]["has_confidence_bands"], false);
        assert_eq!(models[1]["id"], "composite");
        assert_eq!(models[1]["has_confidence_bands"], true);
        assert!(models.iter().all(|m| m["id"] != "real"));
    }
}
