//! Derivatives read handlers (SPEC-API-001 REQ-API-060/061).
//!
//! Routes:
//! - `GET /v1/markets/{id}/derivatives/latest` → get_latest_derivative
//! - `GET /v1/markets/{id}/derivatives`        → list_derivatives (keyset-paginated)

use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::{
    cursor::{decode_keyset_cursor, validate_limit, TsKey},
    dto::{DerivativesQuoteDto, Page},
    quotes::{ensure_market_exists, paginate_ts},
    ApiError, ApiResult, AppState,
};

// ── Query parameter types ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ListDerivativesParams {
    pub cursor: Option<String>,
    pub limit: Option<i64>,
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// `GET /v1/markets/{id}/derivatives/latest` — newest derivatives tick (REQ-API-060).
pub async fn get_latest_derivative(
    State(state): State<AppState>,
    Path(market_id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    ensure_market_exists(&state.pool, market_id).await?;

    let quote: Option<crate::models::derivatives::DerivativesQuote> = sqlx::query_as(
        "SELECT market_id, ts, funding_rate, open_interest, open_interest_usd, \
                mark_price, index_price, basis, volume_24h, contract_type, venue, source \
         FROM derivatives_quotes \
         WHERE market_id = $1 \
         ORDER BY ts DESC \
         LIMIT 1",
    )
    .bind(market_id)
    .fetch_optional(&state.pool)
    .await?;

    match quote {
        Some(q) => Ok(Json(DerivativesQuoteDto::from(q)).into_response()),
        None => Err(ApiError::NotFound(format!(
            "no derivatives found for market id {market_id}"
        ))),
    }
}

/// `GET /v1/markets/{id}/derivatives` — keyset-paginated derivatives history (REQ-API-061).
pub async fn list_derivatives(
    State(state): State<AppState>,
    Path(market_id): Path<i64>,
    Query(params): Query<ListDerivativesParams>,
) -> ApiResult<impl IntoResponse> {
    let limit = validate_limit(params.limit).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let cursor_ts: Option<DateTime<Utc>> = params
        .cursor
        .as_deref()
        .map(|c| decode_keyset_cursor::<TsKey>(c).map(|k| k.ts))
        .transpose()
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    ensure_market_exists(&state.pool, market_id).await?;

    let items: Vec<crate::models::derivatives::DerivativesQuote> = sqlx::query_as(
        "SELECT market_id, ts, funding_rate, open_interest, open_interest_usd, \
                mark_price, index_price, basis, volume_24h, contract_type, venue, source \
         FROM derivatives_quotes \
         WHERE market_id = $1 \
           AND ($2::TIMESTAMPTZ IS NULL OR ts <= $2) \
           AND ($3::TIMESTAMPTZ IS NULL OR ts >= $3) \
           AND ($4::TIMESTAMPTZ IS NULL OR ts < $4) \
         ORDER BY ts DESC \
         LIMIT $5",
    )
    .bind(market_id)
    .bind(params.end)
    .bind(params.start)
    .bind(cursor_ts)
    .bind(limit + 1)
    .fetch_all(&state.pool)
    .await?;

    let (items, next_cursor) = paginate_ts(items, limit, |d| d.ts);
    Ok(Json(Page {
        items: items.into_iter().map(DerivativesQuoteDto::from).collect(),
        next_cursor,
    }))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #[tokio::test]
    #[ignore]
    async fn db_derivatives_latest_unknown_market_returns_404() {
        use axum_test::TestServer;
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
        let resp = server.get("/v1/markets/99999999/derivatives/latest").await;
        assert_eq!(resp.status_code(), 404);
    }
}
