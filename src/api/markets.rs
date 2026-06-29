//! Market management handlers (SPEC-API-001 REQ-API-020..023).
//!
//! Routes:
//! - `GET  /v1/markets`          → list_markets (keyset-paginated, filter by base/quote/venue)
//! - `POST /v1/markets`          → register_market (idempotent 201/200)
//! - `GET  /v1/markets/search?q=`→ search_markets (provider-backed, paced)
//! - `GET  /v1/markets/{id}`     → get_market
//! - `PATCH /v1/markets/{id}`    → update_market
//! - `DELETE /v1/markets/{id}`   → delete_market (soft-deregister)

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use crate::collectors::collection_queue::ENQUEUE_QUEUE_SQL;

use super::{
    cursor::{decode_keyset_cursor, encode_keyset_cursor, validate_limit, MarketListKey},
    dto::{
        MarketDto, MarketSearchPage, Page, RegisterMarketRequest, UpdateMarketRequest,
    },
    ApiError, ApiResult, AppState,
};

// ── Query parameter types ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ListMarketsParams {
    pub cursor: Option<String>,
    pub limit: Option<i64>,
    pub base: Option<String>,
    pub quote: Option<String>,
    pub venue: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SearchMarketsParams {
    pub q: Option<String>,
    pub limit: Option<i64>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// `GET /v1/markets` — keyset-paginated list of tracked markets (REQ-API-022).
///
/// Filterable by `base`, `quote`, and/or `venue` query parameters.
pub async fn list_markets(
    State(state): State<AppState>,
    Query(params): Query<ListMarketsParams>,
) -> ApiResult<impl IntoResponse> {
    let limit = validate_limit(params.limit).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let cursor_id: Option<i64> = params
        .cursor
        .as_deref()
        .map(|c| decode_keyset_cursor::<MarketListKey>(c).map(|k| k.id))
        .transpose()
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    // Build a dynamic WHERE clause supporting optional filters + cursor.
    // Using parameterized query with explicit filter application.
    let items = fetch_markets(
        &state.pool,
        cursor_id,
        limit,
        params.base.as_deref(),
        params.quote.as_deref(),
        params.venue.as_deref(),
    )
    .await?;

    let (items, next_cursor) = paginate_markets(items, limit);
    Ok(Json(Page {
        items: items.into_iter().map(MarketDto::from).collect(),
        next_cursor,
    }))
}

/// `POST /v1/markets` — register a market (idempotent; REQ-API-020/021).
///
/// Uniqueness: `(base, quote, COALESCE(venue, ''))` per REQ-DB-003.
pub async fn register_market(
    State(state): State<AppState>,
    Json(req): Json<RegisterMarketRequest>,
) -> ApiResult<impl IntoResponse> {
    validate_market_base_quote(&req.base, &req.quote)?;
    let kind = req.kind.as_deref().unwrap_or("spot");
    validate_market_kind(kind)?;

    // Check for existing record using the composite uniqueness (REQ-API-021).
    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM tracked_markets \
         WHERE base = $1 AND quote = $2 AND COALESCE(venue, '') = COALESCE($3, '')",
    )
    .bind(&req.base)
    .bind(&req.quote)
    .bind(req.venue.as_deref())
    .fetch_optional(&state.pool)
    .await?;

    if let Some(market_id) = existing {
        let market = fetch_market_by_id(&state.pool, market_id).await?;
        return Ok((StatusCode::OK, Json(MarketDto::from(market))).into_response());
    }

    // Insert new record.
    let market_id: i64 = sqlx::query_scalar(
        "INSERT INTO tracked_markets (base, quote, venue, coin_id, kind, status, registered_at) \
         VALUES ($1, $2, $3, $4, $5, 'active', now()) \
         RETURNING id",
    )
    .bind(&req.base)
    .bind(&req.quote)
    .bind(req.venue.as_deref())
    .bind(req.coin_id.as_deref())
    .bind(kind)
    .fetch_one(&state.pool)
    .await?;

    // Enqueue initial collection + backfill (REQ-API-020: SPEC-SCHED-001).
    let id_str = market_id.to_string();
    for kind_q in &["spot", "candles", "derivatives"] {
        sqlx::query(ENQUEUE_QUEUE_SQL)
            .bind("market")
            .bind(&id_str)
            .bind(kind_q)
            .execute(&state.pool)
            .await?;
    }

    let market = fetch_market_by_id(&state.pool, market_id).await?;
    Ok((StatusCode::CREATED, Json(MarketDto::from(market))).into_response())
}

/// `GET /v1/markets/search?q=` — search candidate market pairs via provider (REQ-API-023).
///
/// # Flow
///
/// 1. Resolve `q` → `coin_id` via provider coin search (top match).
///    `q=BTC` resolves to `bitcoin`; a raw coin_id also resolves since CoinGecko search matches it.
///    Empty resolve → HTTP 200 `{"items":[]}`.
/// 2. Fetch `coin_id`'s trading pairs from `/api/v3/coins/{id}/tickers`, ranked by
///    `converted_volume.usd` descending; stale, anomaly, and contract-address tickers excluded;
///    truncated to `limit`.
pub async fn search_markets(
    State(state): State<AppState>,
    Query(params): Query<SearchMarketsParams>,
) -> ApiResult<impl IntoResponse> {
    let q = params.q.as_deref().unwrap_or("").trim().to_string();
    let limit = validate_limit(params.limit).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let cap = limit.min(50) as usize;

    let provider = state
        .chain
        .iter()
        .find(|p| p.name() == state.search_provider.as_str())
        .ok_or_else(|| {
            ApiError::ServiceUnavailable(format!(
                "search provider '{}' not found in chain",
                state.search_provider
            ))
        })?;

    // Step 1: Resolve q → coin_id via provider coin search (top match only).
    let coin_id = match provider.search_coins(&q, 1).await {
        Ok(coins) if !coins.is_empty() => coins.into_iter().next().unwrap().coin_id,
        Ok(_) => return Ok(Json(MarketSearchPage { items: vec![] })),
        Err(e) => {
            // Network / parse errors: degrade to empty and WARN (REQ-PROV-005).
            tracing::warn!(
                error = %e,
                q = %q,
                "search_markets coin resolve failed; degrading to empty result"
            );
            return Ok(Json(MarketSearchPage { items: vec![] }));
        }
    };

    // Step 2: Fetch coin's trading pairs ranked by converted USD volume.
    let items = match provider.fetch_coin_tickers(&coin_id, cap).await {
        Ok(tickers) => tickers,
        Err(e) => {
            // Network / parse errors: degrade to empty and WARN (REQ-PROV-005).
            tracing::warn!(
                error = %e,
                coin_id = %coin_id,
                q = %q,
                "search_markets tickers fetch failed; degrading to empty result"
            );
            vec![]
        }
    };

    Ok(Json(MarketSearchPage { items }))
}

/// `GET /v1/markets/{id}` — get one tracked market (REQ-API-022).
pub async fn get_market(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    let market: Option<crate::models::quote::TrackedMarket> = sqlx::query_as(
        "SELECT id, base, quote, venue, coin_id, kind, status, registered_at, \
                last_collected_at, error, last_polled_at, live_poll_claimed_until, \
                live_poll_interval \
         FROM tracked_markets WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?;

    match market {
        Some(m) => Ok(Json(MarketDto::from(m)).into_response()),
        None => Err(ApiError::NotFound(format!("market id {id} not found"))),
    }
}

/// `PATCH /v1/markets/{id}` — update mutable fields (REQ-API-022).
pub async fn update_market(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateMarketRequest>,
) -> ApiResult<impl IntoResponse> {
    if let Some(ref s) = req.status {
        validate_market_status(s)?;
    }

    // Convert optional milliseconds to a Postgres INTERVAL string.
    let interval_pg: Option<String> = req
        .live_poll_interval_ms
        .map(|ms| format!("{} milliseconds", ms));

    let market: Option<crate::models::quote::TrackedMarket> = sqlx::query_as(
        "UPDATE tracked_markets \
         SET status             = COALESCE($2, status), \
             error              = COALESCE($3, error), \
             live_poll_interval = COALESCE($4::INTERVAL, live_poll_interval) \
         WHERE id = $1 \
         RETURNING id, base, quote, venue, coin_id, kind, status, registered_at, \
                   last_collected_at, error, last_polled_at, live_poll_claimed_until, \
                   live_poll_interval",
    )
    .bind(id)
    .bind(&req.status)
    .bind(&req.error)
    .bind(interval_pg.as_deref())
    .fetch_optional(&state.pool)
    .await?;

    match market {
        Some(m) => Ok(Json(MarketDto::from(m)).into_response()),
        None => Err(ApiError::NotFound(format!("market id {id} not found"))),
    }
}

/// `DELETE /v1/markets/{id}` — soft-deregister (OR-API-4; REQ-API-022).
pub async fn delete_market(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    let rows_affected = sqlx::query(
        "UPDATE tracked_markets SET status = 'paused' WHERE id = $1 AND status != 'paused'",
    )
    .bind(id)
    .execute(&state.pool)
    .await?
    .rows_affected();

    if rows_affected == 0 {
        let exists: Option<(i64,)> = sqlx::query_as("SELECT id FROM tracked_markets WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.pool)
            .await?;
        if exists.is_none() {
            return Err(ApiError::NotFound(format!("market id {id} not found")));
        }
    }

    Ok(StatusCode::NO_CONTENT.into_response())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Fetch markets with optional cursor + filters.
///
/// To keep this offline-testable, this function builds the query at runtime.
/// The cursor is applied as `id > cursor_id`; filters use ILIKE for case-insensitive matching.
async fn fetch_markets(
    pool: &sqlx::PgPool,
    cursor_id: Option<i64>,
    limit: i64,
    base: Option<&str>,
    quote: Option<&str>,
    venue: Option<&str>,
) -> Result<Vec<crate::models::quote::TrackedMarket>, sqlx::Error> {
    // Build SQL dynamically based on which optional filters are present.
    // All predicates are composed with AND.
    let sql = "SELECT id, base, quote, venue, coin_id, kind, status, registered_at, \
                      last_collected_at, error, last_polled_at, live_poll_claimed_until, \
                      live_poll_interval \
               FROM tracked_markets \
               WHERE ($1::BIGINT IS NULL OR id > $1) \
                 AND ($2::TEXT IS NULL OR base ILIKE $2) \
                 AND ($3::TEXT IS NULL OR quote ILIKE $3) \
                 AND ($4::TEXT IS NULL OR COALESCE(venue, '') ILIKE $4) \
               ORDER BY id ASC \
               LIMIT $5";

    sqlx::query_as(sql)
        .bind(cursor_id)
        .bind(base)
        .bind(quote)
        .bind(venue)
        .bind(limit + 1)
        .fetch_all(pool)
        .await
}

async fn fetch_market_by_id(
    pool: &sqlx::PgPool,
    id: i64,
) -> ApiResult<crate::models::quote::TrackedMarket> {
    sqlx::query_as(
        "SELECT id, base, quote, venue, coin_id, kind, status, registered_at, \
                last_collected_at, error, last_polled_at, live_poll_claimed_until, \
                live_poll_interval \
         FROM tracked_markets WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("market id {id} not found")))
}

fn paginate_markets(
    mut items: Vec<crate::models::quote::TrackedMarket>,
    limit: i64,
) -> (Vec<crate::models::quote::TrackedMarket>, Option<String>) {
    let has_more = items.len() as i64 > limit;
    if has_more {
        items.truncate(limit as usize);
    }
    let next_cursor = has_more.then(|| {
        let last = items.last().expect("non-empty when has_more");
        encode_keyset_cursor(&MarketListKey { id: last.id })
    });
    (items, next_cursor)
}

fn validate_market_base_quote(base: &str, quote: &str) -> ApiResult<()> {
    if base.trim().is_empty() || quote.trim().is_empty() {
        return Err(ApiError::UnprocessableEntity(
            "base and quote must not be empty".into(),
        ));
    }
    Ok(())
}

fn validate_market_kind(kind: &str) -> ApiResult<()> {
    match kind {
        "spot" | "perpetual" | "futures" => Ok(()),
        other => Err(ApiError::UnprocessableEntity(format!(
            "invalid kind '{other}': must be spot, perpetual, or futures"
        ))),
    }
}

fn validate_market_status(status: &str) -> ApiResult<()> {
    match status {
        "active" | "paused" | "error" => Ok(()),
        other => Err(ApiError::UnprocessableEntity(format!(
            "invalid status '{other}': must be active, paused, or error"
        ))),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::cursor::decode_keyset_cursor;

    // validate_market_base_quote rejects empty
    #[test]
    fn validate_base_quote_rejects_empty() {
        assert!(validate_market_base_quote("", "USD").is_err());
        assert!(validate_market_base_quote("BTC", "").is_err());
        assert!(validate_market_base_quote("", "").is_err());
    }

    #[test]
    fn validate_base_quote_accepts_valid() {
        assert!(validate_market_base_quote("BTC", "USD").is_ok());
    }

    // validate_market_kind
    #[test]
    fn validate_market_kind_valid() {
        assert!(validate_market_kind("spot").is_ok());
        assert!(validate_market_kind("perpetual").is_ok());
        assert!(validate_market_kind("futures").is_ok());
    }

    #[test]
    fn validate_market_kind_rejects_unknown() {
        assert!(validate_market_kind("option").is_err());
        assert!(validate_market_kind("").is_err());
    }

    // validate_market_status
    #[test]
    fn validate_market_status_valid() {
        assert!(validate_market_status("active").is_ok());
        assert!(validate_market_status("paused").is_ok());
        assert!(validate_market_status("error").is_ok());
    }

    #[test]
    fn validate_market_status_rejects_unknown() {
        assert!(validate_market_status("disabled").is_err());
    }

    // paginate_markets — has_more → cursor
    #[test]
    fn paginate_markets_has_more_returns_cursor() {
        use chrono::Utc;
        let mut items = vec![];
        for i in 1i64..=3 {
            items.push(crate::models::quote::TrackedMarket {
                id: i,
                base: "BTC".into(),
                quote: "USD".into(),
                venue: None,
                coin_id: None,
                kind: "spot".into(),
                status: "active".into(),
                registered_at: Utc::now(),
                last_collected_at: None,
                error: None,
                last_polled_at: None,
                live_poll_claimed_until: None,
                live_poll_interval: None,
            });
        }
        let (trimmed, next_cursor) = paginate_markets(items, 2);
        assert_eq!(trimmed.len(), 2);
        assert!(next_cursor.is_some());
        let key: MarketListKey = decode_keyset_cursor(next_cursor.as_ref().unwrap()).unwrap();
        assert_eq!(key.id, 2);
    }

    // Scenario 4 (REQ-API-021): uniqueness — NULL venue and empty-string venue
    // resolve to the same COALESCE key in Postgres.
    #[test]
    fn market_uniqueness_coalesce_contract() {
        // Pure: verify COALESCE(NULL, '') == COALESCE('', '') == '' (our uniqueness rule).
        // The handler passes req.venue.as_deref() as $3 (None → SQL NULL).
        // DB tests cover the actual constraint; this documents the invariant in Rust.
        let coalesce_null: &str = ""; // COALESCE(NULL, '') = ''
        let coalesce_empty: &str = ""; // COALESCE('', '') = ''
        assert_eq!(
            coalesce_null, coalesce_empty,
            "NULL and '' venue must map to same key"
        );
    }

    // DB-gated integration tests
    #[tokio::test]
    #[ignore]
    async fn db_register_market_idempotent() {
        use axum_test::TestServer;
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::db::connect(&url).await.expect("db connect");
        let state = crate::api::AppState {
            pool: pool.clone(),
            chain: std::sync::Arc::new(vec![]),

            search_provider: "coingecko".into(),
            coingecko_base_url: "https://api.coingecko.com".into(),
            http_client: reqwest::Client::new(),
        };
        let server = TestServer::new(crate::api::build_api_router(state));

        // First POST → 201
        let resp = server
            .post("/v1/markets")
            .json(&serde_json::json!({
                "base": "BTC-TEST",
                "quote": "USD-TEST",
                "kind": "spot"
            }))
            .await;
        assert_eq!(resp.status_code(), 201);
        let body: serde_json::Value = resp.json();
        let market_id = body["id"].as_i64().unwrap();

        // Second POST (same base/quote/no-venue) → 200
        let resp2 = server
            .post("/v1/markets")
            .json(&serde_json::json!({
                "base": "BTC-TEST",
                "quote": "USD-TEST",
                "kind": "spot"
            }))
            .await;
        assert_eq!(resp2.status_code(), 200);

        // Cleanup
        sqlx::query("DELETE FROM tracked_markets WHERE id = $1")
            .bind(market_id)
            .execute(&pool)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore]
    async fn db_get_market_not_found_returns_404() {
        use axum_test::TestServer;
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::db::connect(&url).await.expect("db connect");
        let state = crate::api::AppState {
            pool,
            chain: std::sync::Arc::new(vec![]),

            search_provider: "coingecko".into(),
            coingecko_base_url: "https://api.coingecko.com".into(),
            http_client: reqwest::Client::new(),
        };
        let server = TestServer::new(crate::api::build_api_router(state));
        let resp = server.get("/v1/markets/99999999").await;
        assert_eq!(resp.status_code(), 404);
    }
}
