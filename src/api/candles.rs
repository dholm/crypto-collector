//! OHLCV candle read handler (SPEC-API-001 REQ-API-040..042).
//!
//! Route:
//! - `GET /v1/markets/{id}/candles` → list_candles (interval required, keyset-paginated)
//!
//! OR-API-1 resolved: supported intervals are `1m`, `5m`, `15m`, `1h`, `4h`, `1d`, `1w`.
//! `interval` is required; absent or invalid → 400 (REQ-API-041).
//! `volume` is nullable in the response (CoinGecko OHLC; REQ-API-042).

use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::{
    cursor::{decode_keyset_cursor, validate_limit, TsKey},
    dto::{CandleDto, Page},
    quotes::paginate_ts,
    ApiError, ApiResult, AppState,
};

/// Supported candle intervals (OR-API-1 resolved).
///
// @MX:NOTE: [AUTO] Supported candle intervals: 1m, 5m, 15m, 1h, 4h, 1d, 1w (OR-API-1)
// @MX:SPEC: SPEC-API-001 OR-API-1 REQ-API-041
pub const SUPPORTED_INTERVALS: &[&str] = &["1m", "5m", "15m", "1h", "4h", "1d", "1w"];

// ── Query parameter types ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ListCandlesParams {
    /// Required: must be one of SUPPORTED_INTERVALS (REQ-API-041).
    pub interval: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<i64>,
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// `GET /v1/markets/{id}/candles` — keyset-paginated OHLCV candles (REQ-API-040..042).
pub async fn list_candles(
    State(state): State<AppState>,
    Path(market_id): Path<i64>,
    Query(params): Query<ListCandlesParams>,
) -> ApiResult<impl IntoResponse> {
    // `interval` is required (REQ-API-041).
    let interval = params
        .interval
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("'interval' query parameter is required".into()))?;
    validate_interval(interval)?;

    let limit = validate_limit(params.limit).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let cursor_ts: Option<DateTime<Utc>> = params
        .cursor
        .as_deref()
        .map(|c| decode_keyset_cursor::<TsKey>(c).map(|k| k.ts))
        .transpose()
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    super::quotes::ensure_market_exists(&state.pool, market_id).await?;

    let items: Vec<crate::models::quote::Candle> = sqlx::query_as(
        "SELECT market_id, interval, ts, open, high, low, close, volume, \
                vs_currency, source \
         FROM candles \
         WHERE market_id = $1 \
           AND interval   = $2 \
           AND ($3::TIMESTAMPTZ IS NULL OR ts <= $3) \
           AND ($4::TIMESTAMPTZ IS NULL OR ts >= $4) \
           AND ($5::TIMESTAMPTZ IS NULL OR ts < $5) \
         ORDER BY ts DESC \
         LIMIT $6",
    )
    .bind(market_id)
    .bind(interval)
    .bind(params.end)
    .bind(params.start)
    .bind(cursor_ts)
    .bind(limit + 1)
    .fetch_all(&state.pool)
    .await?;

    let (items, next_cursor) = paginate_ts(items, limit, |c| c.ts);
    Ok(Json(Page {
        items: items.into_iter().map(CandleDto::from).collect(),
        next_cursor,
    }))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Validate `interval` against the supported set (REQ-API-041).
pub fn validate_interval(interval: &str) -> ApiResult<()> {
    if SUPPORTED_INTERVALS.contains(&interval) {
        Ok(())
    } else {
        Err(ApiError::BadRequest(format!(
            "unsupported interval '{interval}': must be one of {:?}",
            SUPPORTED_INTERVALS
        )))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum_test::TestServer;

    fn test_server() -> TestServer {
        use crate::api::{build_api_router, deny_search_slot_fn, AppState};
        use std::sync::Arc;

        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/crypto_collector_test")
            .expect("lazy pool");

        let state = AppState {
            pool,
            chain: Arc::new(vec![]),
            search_slot_fn: deny_search_slot_fn(),
            search_provider: "coingecko".to_string(),
            coingecko_base_url: "https://api.coingecko.com".to_string(),
            http_client: reqwest::Client::new(),
        };

        TestServer::new(build_api_router(state))
    }

    // Scenario 6 (REQ-API-041): absent interval → 400.
    #[tokio::test]
    async fn list_candles_missing_interval_returns_400() {
        let server = test_server();
        let resp = server.get("/v1/markets/1/candles").await;
        assert_eq!(resp.status_code(), 400);
        let body: serde_json::Value = resp.json();
        assert_eq!(body["code"], "BAD_REQUEST");
    }

    // Scenario 6 (REQ-API-041): unknown interval → 400.
    #[tokio::test]
    async fn list_candles_unknown_interval_returns_400() {
        let server = test_server();
        let resp = server
            .get("/v1/markets/1/candles")
            .add_query_param("interval", "3h")
            .await;
        assert_eq!(resp.status_code(), 400);
    }

    // Scenario 6: all valid intervals are accepted (validate_interval).
    #[test]
    fn all_supported_intervals_are_valid() {
        for &iv in SUPPORTED_INTERVALS {
            assert!(
                validate_interval(iv).is_ok(),
                "interval '{iv}' must be valid"
            );
        }
    }

    // Scenario 6: invalid interval rejected.
    #[test]
    fn invalid_interval_is_rejected() {
        assert!(validate_interval("3h").is_err());
        assert!(validate_interval("").is_err());
        assert!(validate_interval("1hour").is_err());
    }

    // Scenario 10 (REQ-API-071): invalid cursor → 400 on candles endpoint.
    #[tokio::test]
    async fn list_candles_invalid_cursor_returns_400() {
        let server = test_server();
        let resp = server
            .get("/v1/markets/1/candles")
            .add_query_param("interval", "1h")
            .add_query_param("cursor", "NOT_VALID!!!")
            .await;
        assert_eq!(resp.status_code(), 400);
    }

    // Scenario 11 (REQ-API-072): limit above max → 400.
    #[tokio::test]
    async fn list_candles_limit_too_large_returns_400() {
        let server = test_server();
        let resp = server
            .get("/v1/markets/1/candles")
            .add_query_param("interval", "1d")
            .add_query_param("limit", "9999999")
            .await;
        assert_eq!(resp.status_code(), 400);
    }

    // Scenario 6 (REQ-API-042): CandleDto has nullable volume field.
    #[test]
    fn candle_dto_has_nullable_volume() {
        use crate::api::dto::CandleDto;
        use rust_decimal_macros::dec;

        let candle = crate::models::quote::Candle {
            market_id: 1,
            interval: "1h".into(),
            ts: chrono::Utc::now(),
            open: dec!(100),
            high: dec!(110),
            low: dec!(90),
            close: dec!(105),
            volume: None, // CoinGecko: no volume
            vs_currency: "usd".into(),
            source: "coingecko".into(),
        };
        let dto = CandleDto::from(candle);
        assert!(dto.volume.is_none(), "CoinGecko candle volume must be null");
    }

    // DB-gated tests
    #[tokio::test]
    #[ignore]
    async fn db_list_candles_unknown_market_returns_404() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::db::connect(&url).await.expect("db connect");
        let state = crate::api::AppState {
            pool,
            chain: std::sync::Arc::new(vec![]),
            search_slot_fn: crate::api::deny_search_slot_fn(),
            search_provider: "coingecko".into(),
            coingecko_base_url: "https://api.coingecko.com".into(),
            http_client: reqwest::Client::new(),
        };
        let server = TestServer::new(crate::api::build_api_router(state));
        let resp = server
            .get("/v1/markets/99999999/candles")
            .add_query_param("interval", "1h")
            .await;
        assert_eq!(resp.status_code(), 404);
    }
}
