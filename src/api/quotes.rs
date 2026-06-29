//! Coin-keyed spot quote read handlers (SPEC-API-002 REQ-API-131/132).
//!
//! Routes:
//! - `GET /v1/coins/{coin_id}/quotes/latest` → get_latest_quote
//! - `GET /v1/coins/{coin_id}/quotes`        → list_quotes (keyset-paginated, time-range)

use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::{
    cursor::{decode_keyset_cursor, encode_keyset_cursor, validate_limit, TsKey},
    dto::{CoinQuoteDto, Page},
    ApiError, ApiResult, AppState,
};

// ── Query parameter types ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ListQuotesParams {
    pub cursor: Option<String>,
    pub limit: Option<i64>,
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// `GET /v1/coins/{coin_id}/quotes/latest` — newest spot quote for a coin (REQ-API-131).
pub async fn get_latest_quote(
    State(state): State<AppState>,
    Path(coin_id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    ensure_coin_exists(&state.pool, &coin_id).await?;

    let quote: Option<crate::models::quote::CoinQuote> = sqlx::query_as(
        "SELECT coin_id, vs_currency, ts, price, source \
         FROM coin_quotes \
         WHERE coin_id = $1 \
         ORDER BY ts DESC \
         LIMIT 1",
    )
    .bind(&coin_id)
    .fetch_optional(&state.pool)
    .await?;

    match quote {
        Some(q) => Ok(Json(CoinQuoteDto::from(q)).into_response()),
        None => Err(ApiError::NotFound(format!(
            "no quotes found for coin '{coin_id}'"
        ))),
    }
}

/// `GET /v1/coins/{coin_id}/quotes` — keyset-paginated quote history (REQ-API-132).
pub async fn list_quotes(
    State(state): State<AppState>,
    Path(coin_id): Path<String>,
    Query(params): Query<ListQuotesParams>,
) -> ApiResult<impl IntoResponse> {
    let limit = validate_limit(params.limit).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let cursor_ts: Option<DateTime<Utc>> = params
        .cursor
        .as_deref()
        .map(|c| decode_keyset_cursor::<TsKey>(c).map(|k| k.ts))
        .transpose()
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    ensure_coin_exists(&state.pool, &coin_id).await?;

    let items: Vec<crate::models::quote::CoinQuote> = sqlx::query_as(
        "SELECT coin_id, vs_currency, ts, price, source \
         FROM coin_quotes \
         WHERE coin_id = $1 \
           AND ($2::TIMESTAMPTZ IS NULL OR ts <= $2) \
           AND ($3::TIMESTAMPTZ IS NULL OR ts >= $3) \
           AND ($4::TIMESTAMPTZ IS NULL OR ts < $4) \
         ORDER BY ts DESC \
         LIMIT $5",
    )
    .bind(&coin_id)
    .bind(params.end)
    .bind(params.start)
    .bind(cursor_ts)
    .bind(limit + 1)
    .fetch_all(&state.pool)
    .await?;

    let (items, next_cursor) = paginate_ts(items, limit, |q| q.ts);
    Ok(Json(Page {
        items: items.into_iter().map(CoinQuoteDto::from).collect(),
        next_cursor,
    }))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Check that a coin_id exists; return 404 if not.
pub async fn ensure_coin_exists(pool: &sqlx::PgPool, coin_id: &str) -> ApiResult<()> {
    let exists: Option<(String,)> =
        sqlx::query_as("SELECT coin_id FROM tracked_coins WHERE coin_id = $1")
            .bind(coin_id)
            .fetch_optional(pool)
            .await?;
    if exists.is_none() {
        return Err(ApiError::NotFound(format!("coin '{coin_id}' not found")));
    }
    Ok(())
}

/// Generic keyset paginator for time-series rows ordered `ts DESC`.
pub fn paginate_ts<T, F>(mut items: Vec<T>, limit: i64, get_ts: F) -> (Vec<T>, Option<String>)
where
    F: Fn(&T) -> DateTime<Utc>,
{
    let has_more = items.len() as i64 > limit;
    if has_more {
        items.truncate(limit as usize);
    }
    let next_cursor = has_more.then(|| {
        let last = items.last().expect("non-empty when has_more");
        encode_keyset_cursor(&TsKey { ts: get_ts(last) })
    });
    (items, next_cursor)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, h, 0, 0).unwrap()
    }

    fn make_coin_quote(h: u32, price_str: &str) -> crate::models::quote::CoinQuote {
        use rust_decimal::Decimal;
        use std::str::FromStr;
        crate::models::quote::CoinQuote {
            coin_id: "bitcoin".into(),
            vs_currency: "usd".into(),
            ts: ts(h),
            price: Decimal::from_str(price_str).unwrap(),
            source: "test".into(),
        }
    }

    // paginate_ts: has_more → next_cursor encodes last item ts
    #[test]
    fn paginate_ts_has_more_returns_cursor() {
        let items = vec![
            make_coin_quote(12, "100"),
            make_coin_quote(11, "99"),
            make_coin_quote(10, "98"),
        ];

        let (trimmed, next_cursor) = paginate_ts(items, 2, |q| q.ts);
        assert_eq!(trimmed.len(), 2);
        assert!(next_cursor.is_some());
        let key: TsKey = decode_keyset_cursor(next_cursor.as_ref().unwrap()).unwrap();
        assert_eq!(key.ts, ts(11), "cursor must encode last returned row ts");
    }

    #[test]
    fn paginate_ts_no_more_returns_null_cursor() {
        let items = vec![make_coin_quote(12, "100")];
        let (_, next_cursor) = paginate_ts(items, 100, |q| q.ts);
        assert!(next_cursor.is_none());
    }

    // DB-gated integration tests
    #[tokio::test]
    #[ignore]
    async fn db_latest_quote_unknown_coin_returns_404() {
        use axum_test::TestServer;
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::db::connect(&url).await.expect("db connect");
        let state = crate::api::AppState {
            pool,
            chain: std::sync::Arc::new(vec![]),
            search_provider: "coingecko".into(),
            coingecko_base_url: "https://api.coingecko.com".into(),
            http_client: reqwest::Client::new(),
            coin_quote_tx: tokio::sync::broadcast::channel(16).0,
            coin_candle_tx: tokio::sync::broadcast::channel(16).0,
        };
        let server = TestServer::new(crate::api::build_api_router(state));
        let resp = server.get("/v1/coins/no-such-coin-xyz/quotes/latest").await;
        assert_eq!(resp.status_code(), 404);
    }
}
