//! Spot quote read handlers (SPEC-API-001 REQ-API-030/031).
//!
//! Routes:
//! - `GET /v1/markets/{id}/quotes/latest` → get_latest_quote
//! - `GET /v1/markets/{id}/quotes`        → list_quotes (keyset-paginated, time-range)

use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::{
    cursor::{decode_keyset_cursor, encode_keyset_cursor, validate_limit, TsKey},
    dto::{Page, QuoteDto},
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

/// `GET /v1/markets/{id}/quotes/latest` — newest live quote for a market (REQ-API-030).
pub async fn get_latest_quote(
    State(state): State<AppState>,
    Path(market_id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    // Verify market exists first (REQ-API-074: 404 for unknown market).
    ensure_market_exists(&state.pool, market_id).await?;

    let quote: Option<crate::models::quote::LiveQuote> = sqlx::query_as(
        "SELECT market_id, ts, as_of, price, bid, ask, bid_size, ask_size, \
                volume_24h, vs_currency, source \
         FROM live_quotes \
         WHERE market_id = $1 \
         ORDER BY ts DESC \
         LIMIT 1",
    )
    .bind(market_id)
    .fetch_optional(&state.pool)
    .await?;

    match quote {
        Some(q) => Ok(Json(QuoteDto::from(q)).into_response()),
        None => Err(ApiError::NotFound(format!(
            "no quotes found for market id {market_id}"
        ))),
    }
}

/// `GET /v1/markets/{id}/quotes` — keyset-paginated quote history (REQ-API-031).
pub async fn list_quotes(
    State(state): State<AppState>,
    Path(market_id): Path<i64>,
    Query(params): Query<ListQuotesParams>,
) -> ApiResult<impl IntoResponse> {
    let limit = validate_limit(params.limit).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let cursor_ts: Option<DateTime<Utc>> = params
        .cursor
        .as_deref()
        .map(|c| decode_keyset_cursor::<TsKey>(c).map(|k| k.ts))
        .transpose()
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    ensure_market_exists(&state.pool, market_id).await?;

    // The keyset WHERE clause: ts < cursor_ts (for DESC ordering stability).
    let items: Vec<crate::models::quote::LiveQuote> = sqlx::query_as(
        "SELECT market_id, ts, as_of, price, bid, ask, bid_size, ask_size, \
                volume_24h, vs_currency, source \
         FROM live_quotes \
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

    let (items, next_cursor) = paginate_ts(items, limit, |q| q.ts);
    Ok(Json(Page {
        items: items.into_iter().map(QuoteDto::from).collect(),
        next_cursor,
    }))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Check that a market_id exists; return 404 if not.
pub async fn ensure_market_exists(pool: &sqlx::PgPool, market_id: i64) -> ApiResult<()> {
    let exists: Option<(i64,)> = sqlx::query_as("SELECT id FROM tracked_markets WHERE id = $1")
        .bind(market_id)
        .fetch_optional(pool)
        .await?;
    if exists.is_none() {
        return Err(ApiError::NotFound(format!(
            "market id {market_id} not found"
        )));
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

    // paginate_ts: has_more → next_cursor encodes last item ts
    #[test]
    fn paginate_ts_has_more_returns_cursor() {
        use rust_decimal_macros::dec;

        let items = vec![
            crate::models::quote::LiveQuote {
                market_id: 1,
                ts: ts(12),
                as_of: None,
                price: dec!(100),
                bid: None,
                ask: None,
                bid_size: None,
                ask_size: None,
                volume_24h: None,
                vs_currency: "usd".into(),
                source: "test".into(),
            },
            crate::models::quote::LiveQuote {
                market_id: 1,
                ts: ts(11),
                as_of: None,
                price: dec!(99),
                bid: None,
                ask: None,
                bid_size: None,
                ask_size: None,
                volume_24h: None,
                vs_currency: "usd".into(),
                source: "test".into(),
            },
            crate::models::quote::LiveQuote {
                market_id: 1,
                ts: ts(10),
                as_of: None,
                price: dec!(98),
                bid: None,
                ask: None,
                bid_size: None,
                ask_size: None,
                volume_24h: None,
                vs_currency: "usd".into(),
                source: "test".into(),
            },
        ];

        let (trimmed, next_cursor) = paginate_ts(items, 2, |q| q.ts);
        assert_eq!(trimmed.len(), 2);
        assert!(next_cursor.is_some());
        let key: TsKey = decode_keyset_cursor(next_cursor.as_ref().unwrap()).unwrap();
        assert_eq!(key.ts, ts(11), "cursor must encode last returned row ts");
    }

    #[test]
    fn paginate_ts_no_more_returns_null_cursor() {
        use rust_decimal_macros::dec;
        let items = vec![crate::models::quote::LiveQuote {
            market_id: 1,
            ts: ts(12),
            as_of: None,
            price: dec!(100),
            bid: None,
            ask: None,
            bid_size: None,
            ask_size: None,
            volume_24h: None,
            vs_currency: "usd".into(),
            source: "test".into(),
        }];
        let (_, next_cursor) = paginate_ts(items, 100, |q| q.ts);
        assert!(next_cursor.is_none());
    }

    // DB-gated integration tests
    #[tokio::test]
    #[ignore]
    async fn db_latest_quote_unknown_market_returns_404() {
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
        let resp = server.get("/v1/markets/99999999/quotes/latest").await;
        assert_eq!(resp.status_code(), 404);
    }
}
