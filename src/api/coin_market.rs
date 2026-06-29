//! Coin market aggregate read handlers (SPEC-API-001 REQ-API-051/052).
//!
//! Routes:
//! - `GET /v1/coins/{coin_id}/market/latest` → get_coin_market_latest
//! - `GET /v1/coins/{coin_id}/market`        → list_coin_market (keyset-paginated)

use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::{
    cursor::{decode_keyset_cursor, validate_limit, TsKey},
    dto::{CoinMarketSnapshotDto, Page},
    metadata::ensure_coin_exists,
    quotes::paginate_ts,
    ApiError, ApiResult, AppState,
};

// ── Query parameter types ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct GetCoinMarketLatestParams {
    /// Required quote currency (e.g. `usd`, `btc`).
    pub vs_currency: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListCoinMarketParams {
    pub vs_currency: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<i64>,
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// `GET /v1/coins/{coin_id}/market/latest` — newest market snapshot (REQ-API-051).
pub async fn get_coin_market_latest(
    State(state): State<AppState>,
    Path(coin_id): Path<String>,
    Query(params): Query<GetCoinMarketLatestParams>,
) -> ApiResult<impl IntoResponse> {
    let vs_currency = params
        .vs_currency
        .as_deref()
        .unwrap_or("usd")
        .to_lowercase();

    ensure_coin_exists(&state.pool, &coin_id).await?;

    let snapshot: Option<crate::models::coin::CoinMarketSnapshot> = sqlx::query_as(
        "SELECT coin_id, vs_currency, ts, price, market_cap, fully_diluted_valuation, \
                circulating_supply, total_supply, volume_24h, source \
         FROM coin_market_snapshots \
         WHERE coin_id = $1 AND vs_currency = $2 \
         ORDER BY ts DESC \
         LIMIT 1",
    )
    .bind(&coin_id)
    .bind(&vs_currency)
    .fetch_optional(&state.pool)
    .await?;

    match snapshot {
        Some(s) => Ok(Json(CoinMarketSnapshotDto::from(s)).into_response()),
        None => Err(ApiError::NotFound(format!(
            "no market snapshot found for coin '{coin_id}' in '{vs_currency}'"
        ))),
    }
}

/// `GET /v1/coins/{coin_id}/market` — keyset-paginated market snapshot history (REQ-API-052).
pub async fn list_coin_market(
    State(state): State<AppState>,
    Path(coin_id): Path<String>,
    Query(params): Query<ListCoinMarketParams>,
) -> ApiResult<impl IntoResponse> {
    let vs_currency = params
        .vs_currency
        .as_deref()
        .unwrap_or("usd")
        .to_lowercase();

    let limit = validate_limit(params.limit).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let cursor_ts: Option<DateTime<Utc>> = params
        .cursor
        .as_deref()
        .map(|c| decode_keyset_cursor::<TsKey>(c).map(|k| k.ts))
        .transpose()
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    ensure_coin_exists(&state.pool, &coin_id).await?;

    let items: Vec<crate::models::coin::CoinMarketSnapshot> = sqlx::query_as(
        "SELECT coin_id, vs_currency, ts, price, market_cap, fully_diluted_valuation, \
                circulating_supply, total_supply, volume_24h, source \
         FROM coin_market_snapshots \
         WHERE coin_id = $1 \
           AND vs_currency = $2 \
           AND ($3::TIMESTAMPTZ IS NULL OR ts <= $3) \
           AND ($4::TIMESTAMPTZ IS NULL OR ts >= $4) \
           AND ($5::TIMESTAMPTZ IS NULL OR ts < $5) \
         ORDER BY ts DESC \
         LIMIT $6",
    )
    .bind(&coin_id)
    .bind(&vs_currency)
    .bind(params.end)
    .bind(params.start)
    .bind(cursor_ts)
    .bind(limit + 1)
    .fetch_all(&state.pool)
    .await?;

    let (items, next_cursor) = paginate_ts(items, limit, |s| s.ts);
    Ok(Json(Page {
        items: items.into_iter().map(CoinMarketSnapshotDto::from).collect(),
        next_cursor,
    }))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    // DB-gated integration tests
    #[tokio::test]
    #[ignore]
    async fn db_coin_market_latest_unknown_coin_returns_404() {
        use axum_test::TestServer;
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::db::connect(&url).await.expect("db connect");
        let (coin_quote_tx, _) = tokio::sync::broadcast::channel(16);
        let (coin_candle_tx, _) = tokio::sync::broadcast::channel(16);
        let state = crate::api::AppState {
            pool,
            chain: std::sync::Arc::new(vec![]),

            search_provider: "coingecko".into(),
            coingecko_base_url: "https://api.coingecko.com".into(),
            http_client: reqwest::Client::new(),
            coin_quote_tx,
            coin_candle_tx,
        };
        let server = TestServer::new(crate::api::build_api_router(state));
        let resp = server.get("/v1/coins/no-such-coin-xyz/market/latest").await;
        assert_eq!(resp.status_code(), 404);
    }
}
