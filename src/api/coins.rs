//! Coin management handlers (SPEC-API-001 REQ-API-010..013, SPEC-API-002 REQ-API-112/113/114).
//!
//! Routes:
//! - `GET  /v1/coins`            → list_coins (keyset-paginated)
//! - `POST /v1/coins`            → register_coin (idempotent 201/200)
//! - `GET  /v1/coins/search?q=`  → search_coins (provider-backed, paced)
//! - `GET  /v1/coins/{coin_id}`  → get_coin
//! - `PATCH /v1/coins/{coin_id}` → update_coin (tri-state live_poll_interval; REQ-API-112/114)
//! - `DELETE /v1/coins/{coin_id}`→ delete_coin (soft-deregister)

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};

use crate::collectors::collection_queue::ENQUEUE_QUEUE_SQL;
use crate::config;
use crate::pacer::AcquireSlotError;
use crate::providers::ProviderError;

use super::{
    cursor::{decode_keyset_cursor, encode_keyset_cursor, validate_limit, CoinListKey},
    dto::{CoinDto, CoinSearchPage, Page, RegisterCoinRequest, UpdateCoinRequest},
    extract::{ApiJson, ApiPath, ApiQuery},
    poll_interval, ApiError, ApiResult, AppState,
};

// ── SELECT column list ─────────────────────────────────────────────────────────
//
// sqlx 0.9 SqlSafeStr requires &'static str — format!() yields &String which does not
// implement that bound. The column list is inlined in every query literal instead.
// live_poll_interval::TEXT casts INTERVAL → Option<String> on TrackedCoin (REQ-API-112).

// ── Query parameter types ─────────────────────────────────────────────────────

// `Serialize` is derived (alongside `Deserialize`) so the F-59 parameter-parity test
// (src/api/mod.rs) can reflect the struct's serde field set.
#[derive(Debug, Deserialize, Serialize)]
pub struct ListCoinsParams {
    pub cursor: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SearchCoinsParams {
    pub q: Option<String>,
    pub limit: Option<i64>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// `GET /v1/coins` — keyset-paginated list of tracked coins (REQ-API-012).
pub async fn list_coins(
    State(state): State<AppState>,
    ApiQuery(params): ApiQuery<ListCoinsParams>,
) -> ApiResult<impl IntoResponse> {
    let limit = validate_limit(params.limit).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let cursor_coin_id: Option<String> = params
        .cursor
        .as_deref()
        .map(|c| decode_keyset_cursor::<CoinListKey>(c).map(|k| k.coin_id))
        .transpose()
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let items =
        match cursor_coin_id {
            None => sqlx::query_as::<_, crate::models::coin::TrackedCoin>(
                "SELECT coin_id, symbol, name, status, registered_at, last_collected_at, error, \
             live_poll_interval::TEXT AS live_poll_interval \
             FROM tracked_coins ORDER BY coin_id ASC LIMIT $1",
            )
            .bind(limit + 1)
            .fetch_all(&state.pool)
            .await?,
            Some(ref after_coin_id) => sqlx::query_as::<_, crate::models::coin::TrackedCoin>(
                "SELECT coin_id, symbol, name, status, registered_at, last_collected_at, error, \
             live_poll_interval::TEXT AS live_poll_interval \
             FROM tracked_coins WHERE coin_id > $1 ORDER BY coin_id ASC LIMIT $2",
            )
            .bind(after_coin_id)
            .bind(limit + 1)
            .fetch_all(&state.pool)
            .await?,
        };

    let (items, next_cursor) = paginate_coins(items, limit);
    Ok(Json(Page {
        items: items.into_iter().map(CoinDto::from).collect(),
        next_cursor,
    }))
}

/// `POST /v1/coins` — register a coin for collection (idempotent; REQ-API-010/011/407/408).
///
/// F-32: the insert uses `ON CONFLICT (coin_id) DO NOTHING RETURNING`, so two concurrent
/// registrations of the same coin are idempotent (201 for the winner, 200 for the loser after a
/// re-select) and never raise a 500 primary-key violation. The insert and the three initial
/// collection enqueues run inside a single transaction, so a coin row is never committed without
/// its enqueues and enqueues are never committed without the coin (REQ-API-408).
pub async fn register_coin(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<RegisterCoinRequest>,
) -> ApiResult<impl IntoResponse> {
    validate_coin_id(&req.coin_id)?;

    // Validate live_poll_interval if provided (REQ-API-114).
    let pg_interval = if let Some(ref iv) = req.live_poll_interval {
        let min_secs = config::live_poll_min_interval_secs();
        let max_secs = config::live_poll_max_interval_secs();
        let global_secs = config::live_quote_poll_interval_secs() as u64;
        let d = poll_interval::parse_live_poll_duration(iv, min_secs, max_secs, global_secs)?;
        Some(poll_interval::duration_to_pg_interval(d))
    } else {
        None
    };

    let mut tx = state.pool.begin().await?;

    // Idempotent insert (REQ-API-407): ON CONFLICT DO NOTHING RETURNING yields the new row on a
    // fresh insert and zero rows on a conflict. Under concurrency the losing INSERT waits for the
    // winner to commit, then returns nothing → the re-select (READ COMMITTED, fresh snapshot per
    // statement) sees the committed row → 200. Never a 500 PK violation.
    let inserted: Option<crate::models::coin::TrackedCoin> = sqlx::query_as(
        "INSERT INTO tracked_coins (coin_id, symbol, name, status, registered_at, live_poll_interval) \
         VALUES ($1, $2, $3, 'active', now(), $4::interval) \
         ON CONFLICT (coin_id) DO NOTHING \
         RETURNING coin_id, symbol, name, status, registered_at, last_collected_at, error, \
         live_poll_interval::TEXT AS live_poll_interval",
    )
    .bind(&req.coin_id)
    .bind(&req.symbol)
    .bind(&req.name)
    .bind(pg_interval)
    .fetch_optional(&mut *tx)
    .await?;

    let coin = match inserted {
        // Conflict (coin already registered): re-select the existing row and return 200.
        None => {
            let existing: crate::models::coin::TrackedCoin = sqlx::query_as(
                "SELECT coin_id, symbol, name, status, registered_at, last_collected_at, error, \
                 live_poll_interval::TEXT AS live_poll_interval \
                 FROM tracked_coins WHERE coin_id = $1",
            )
            .bind(&req.coin_id)
            .fetch_one(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok((StatusCode::OK, Json(CoinDto::from(existing))).into_response());
        }
        Some(c) => c,
    };

    // New coin: enqueue the three initial collections inside the SAME transaction (REQ-API-408:
    // insert + enqueues are atomic — SPEC-SCHED-001).
    for kind in &["metadata", "market", "candles"] {
        sqlx::query(ENQUEUE_QUEUE_SQL)
            .bind("coin")
            .bind(&req.coin_id)
            .bind(kind)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(CoinDto::from(coin))).into_response())
}

/// `GET /v1/coins/search?q=` — search candidate coins via provider (REQ-API-013).
pub async fn search_coins(
    State(state): State<AppState>,
    ApiQuery(params): ApiQuery<SearchCoinsParams>,
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

    let items = match provider.search_coins(&q, cap).await {
        Ok(coins) => coins,
        // F-34/D3 (REQ-API-411): a pacer cooldown / credit-exhaustion error means the provider
        // is unavailable → 503 ServiceUnavailable, distinct from a genuine no-match (Ok(vec![])
        // → 200-empty). Only this LOCKED set maps to 503; adjacent ProviderError variants stay
        // degraded-to-empty (OR-API5-3: do not silently widen the LOCKED set).
        Err(ProviderError::Pacer(
            AcquireSlotError::Cooldown(..) | AcquireSlotError::CreditExhausted(..),
        )) => {
            return Err(ApiError::ServiceUnavailable(
                "search provider is in cooldown or has exhausted its upstream credit; retry later"
                    .into(),
            ));
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                q = %q,
                "search_coins provider call failed; degrading to empty result"
            );
            vec![]
        }
    };

    Ok(Json(CoinSearchPage { items }))
}

/// `GET /v1/coins/{coin_id}` — get one tracked coin (REQ-API-012).
pub async fn get_coin(
    State(state): State<AppState>,
    ApiPath(coin_id): ApiPath<String>,
) -> ApiResult<impl IntoResponse> {
    let coin: Option<crate::models::coin::TrackedCoin> = sqlx::query_as(
        "SELECT coin_id, symbol, name, status, registered_at, last_collected_at, error, \
         live_poll_interval::TEXT AS live_poll_interval \
         FROM tracked_coins WHERE coin_id = $1",
    )
    .bind(&coin_id)
    .fetch_optional(&state.pool)
    .await?;

    match coin {
        Some(c) => Ok(Json(CoinDto::from(c)).into_response()),
        None => Err(ApiError::NotFound(format!("coin '{coin_id}' not found"))),
    }
}

/// `PATCH /v1/coins/{coin_id}` — update mutable fields (REQ-API-012, SPEC-API-002 REQ-API-112/114).
///
/// `live_poll_interval` uses tri-state semantics (REQ-API-112):
/// - Absent (field not in JSON): leave existing value unchanged.
/// - `null`: reset to global default (set DB column to NULL, reset poller cursors).
/// - String: parse, validate bounds (422 on violation; REQ-API-114), set new interval.
pub async fn update_coin(
    State(state): State<AppState>,
    ApiPath(coin_id): ApiPath<String>,
    ApiJson(req): ApiJson<UpdateCoinRequest>,
) -> ApiResult<impl IntoResponse> {
    if let Some(ref s) = req.status {
        validate_coin_status(s)?;
    }

    let coin: Option<crate::models::coin::TrackedCoin> = match req.live_poll_interval {
        // Field absent: update only status/error; leave live_poll_interval unchanged.
        None => {
            sqlx::query_as(
                "UPDATE tracked_coins \
             SET status = COALESCE($2, status), error = COALESCE($3, error) \
             WHERE coin_id = $1 \
             RETURNING coin_id, symbol, name, status, registered_at, last_collected_at, error, \
             live_poll_interval::TEXT AS live_poll_interval",
            )
            .bind(&coin_id)
            .bind(&req.status)
            .bind(&req.error)
            .fetch_optional(&state.pool)
            .await?
        }

        // Field is null: reset per-coin interval to NULL (global default); reset poller cursors.
        Some(None) => {
            sqlx::query_as(
                "UPDATE tracked_coins \
             SET status = COALESCE($2, status), error = COALESCE($3, error), \
                 live_poll_interval = NULL, \
                 last_polled_at = NULL, \
                 live_poll_claimed_until = NULL \
             WHERE coin_id = $1 \
             RETURNING coin_id, symbol, name, status, registered_at, last_collected_at, error, \
             live_poll_interval::TEXT AS live_poll_interval",
            )
            .bind(&coin_id)
            .bind(&req.status)
            .bind(&req.error)
            .fetch_optional(&state.pool)
            .await?
        }

        // Field is a string: parse, validate, set new interval; reset poller cursors.
        Some(Some(ref iv)) => {
            let min_secs = config::live_poll_min_interval_secs();
            let max_secs = config::live_poll_max_interval_secs();
            let global_secs = config::live_quote_poll_interval_secs() as u64;
            let d = poll_interval::parse_live_poll_duration(iv, min_secs, max_secs, global_secs)?;
            let pg_interval = poll_interval::duration_to_pg_interval(d);

            sqlx::query_as(
                "UPDATE tracked_coins \
                 SET status = COALESCE($2, status), error = COALESCE($3, error), \
                     live_poll_interval = $4::interval, \
                     last_polled_at = NULL, \
                     live_poll_claimed_until = NULL \
                 WHERE coin_id = $1 \
                 RETURNING coin_id, symbol, name, status, registered_at, last_collected_at, error, \
                 live_poll_interval::TEXT AS live_poll_interval",
            )
            .bind(&coin_id)
            .bind(&req.status)
            .bind(&req.error)
            .bind(&pg_interval)
            .fetch_optional(&state.pool)
            .await?
        }
    };

    match coin {
        Some(c) => Ok(Json(CoinDto::from(c)).into_response()),
        None => Err(ApiError::NotFound(format!("coin '{coin_id}' not found"))),
    }
}

/// `DELETE /v1/coins/{coin_id}` — soft-deregister (OR-API-4 resolved: soft-delete; REQ-API-012).
///
/// Sets `status = 'paused'` so workers stop collecting but historical data is retained.
pub async fn delete_coin(
    State(state): State<AppState>,
    ApiPath(coin_id): ApiPath<String>,
) -> ApiResult<impl IntoResponse> {
    let rows_affected = sqlx::query(
        "UPDATE tracked_coins SET status = 'paused' WHERE coin_id = $1 AND status != 'paused'",
    )
    .bind(&coin_id)
    .execute(&state.pool)
    .await?
    .rows_affected();

    if rows_affected == 0 {
        let exists: Option<(String,)> =
            sqlx::query_as("SELECT coin_id FROM tracked_coins WHERE coin_id = $1")
                .bind(&coin_id)
                .fetch_optional(&state.pool)
                .await?;
        if exists.is_none() {
            return Err(ApiError::NotFound(format!("coin '{coin_id}' not found")));
        }
    }

    Ok(StatusCode::NO_CONTENT.into_response())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn paginate_coins(
    mut items: Vec<crate::models::coin::TrackedCoin>,
    limit: i64,
) -> (Vec<crate::models::coin::TrackedCoin>, Option<String>) {
    let has_more = items.len() as i64 > limit;
    if has_more {
        items.truncate(limit as usize);
    }
    let next_cursor = has_more.then(|| {
        let last = items.last().expect("non-empty when has_more");
        encode_keyset_cursor(&CoinListKey {
            coin_id: last.coin_id.clone(),
        })
    });
    (items, next_cursor)
}

fn validate_coin_id(coin_id: &str) -> ApiResult<()> {
    if coin_id.trim().is_empty() {
        return Err(ApiError::UnprocessableEntity(
            "coin_id must not be empty".into(),
        ));
    }
    Ok(())
}

fn validate_coin_status(status: &str) -> ApiResult<()> {
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
    use axum_test::TestServer;

    fn test_server() -> TestServer {
        use crate::api::{build_api_router, AppState};
        use std::sync::Arc;
        use tokio::sync::broadcast;

        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/crypto_collector_test")
            .expect("lazy pool");

        let (coin_quote_tx, _) = broadcast::channel(16);
        let (coin_candle_tx, _) = broadcast::channel(16);

        let state = AppState {
            pool,
            chain: Arc::new(vec![]),
            search_provider: "coingecko".to_string(),
            coingecko_base_url: "https://api.coingecko.com".to_string(),
            http_client: reqwest::Client::new(),
            coin_quote_tx,
            coin_candle_tx,
        };

        TestServer::new(build_api_router(state))
    }

    #[tokio::test]
    async fn list_coins_invalid_limit_returns_400() {
        let server = test_server();
        let resp = server
            .get("/v1/coins")
            .add_query_param("limit", "9999999")
            .await;
        assert_eq!(resp.status_code(), 400);
    }

    #[tokio::test]
    async fn list_coins_invalid_cursor_returns_400() {
        let server = test_server();
        let resp = server
            .get("/v1/coins")
            .add_query_param("cursor", "not!!valid!!base64@@")
            .await;
        assert_eq!(resp.status_code(), 400);
    }

    // ── SPEC-API-005 M2 (F-33, REQ-API-409/410): uniform extractor error bodies ──

    // AC-API-409: a malformed JSON body → the documented {code, message} JSON body (not
    // Axum's default text/plain rejection). Extraction fails before the handler → no DB.
    #[tokio::test]
    async fn malformed_json_body_returns_json_error_body() {
        let server = test_server();
        let resp = server
            .post("/v1/coins")
            .content_type("application/json")
            .text("{ this is not valid json")
            .await;
        assert_eq!(resp.status_code(), 400);
        let ct = resp.header("content-type");
        assert!(
            ct.to_str().unwrap().contains("application/json"),
            "malformed body rejection must be application/json, got {ct:?}"
        );
        // Body must be the {code, message} shape — parsing as a JSON object proves it is not
        // Axum's plain-text rejection.
        let body: serde_json::Value = resp.json();
        assert_eq!(body["code"], "BAD_REQUEST");
        assert!(body["message"].is_string());
    }

    // AC-API-409: a malformed query value (limit is i64; "notanumber" fails to deserialize) →
    // {code, message} JSON. The ApiQuery wrapper funnels the QueryRejection through ApiError.
    #[tokio::test]
    async fn malformed_query_value_returns_json_error_body() {
        let server = test_server();
        let resp = server
            .get("/v1/coins")
            .add_query_param("limit", "notanumber")
            .await;
        assert_eq!(resp.status_code(), 400);
        let ct = resp.header("content-type");
        assert!(
            ct.to_str().unwrap().contains("application/json"),
            "malformed query rejection must be application/json, got {ct:?}"
        );
        let body: serde_json::Value = resp.json();
        assert_eq!(body["code"], "BAD_REQUEST");
        assert!(body["message"].is_string());
    }

    // ── SPEC-API-005 M3 (F-34, REQ-API-411): search 503 vs 200-empty ─────────────

    // Minimal stub provider whose search_coins result is configurable, so search_coins can be
    // exercised without a live upstream. Named "coingecko" to match AppState.search_provider.
    enum SearchBehavior {
        Cooldown,
        CreditExhausted,
        Empty,
    }

    struct SearchStubProvider {
        behavior: SearchBehavior,
    }

    #[async_trait::async_trait]
    impl crate::providers::Provider for SearchStubProvider {
        fn name(&self) -> &str {
            "coingecko"
        }
        fn supports(&self, _cap: crate::providers::Capability) -> bool {
            true
        }
        async fn fetch_spot(
            &self,
            _m: &crate::providers::MarketQuery,
        ) -> Result<crate::providers::SpotQuote, ProviderError> {
            Err(ProviderError::NotSupported(
                crate::providers::Capability::Spot,
            ))
        }
        async fn fetch_ohlc(
            &self,
            _m: &crate::providers::MarketQuery,
            _d: u32,
            _i: i64,
        ) -> Result<Vec<crate::providers::OhlcCandle>, ProviderError> {
            Ok(vec![])
        }
        async fn fetch_coin_metadata(
            &self,
            _id: &str,
        ) -> Result<crate::providers::CoinMeta, ProviderError> {
            Err(ProviderError::NotSupported(
                crate::providers::Capability::CoinMetadata,
            ))
        }
        async fn fetch_coin_market(
            &self,
            _id: &str,
            _vs: &str,
        ) -> Result<crate::providers::CoinMarket, ProviderError> {
            Err(ProviderError::NotSupported(
                crate::providers::Capability::CoinMarket,
            ))
        }
        async fn fetch_derivatives(
            &self,
            _m: &crate::providers::MarketQuery,
        ) -> Result<crate::providers::DerivTick, ProviderError> {
            Err(ProviderError::NotSupported(
                crate::providers::Capability::Derivatives,
            ))
        }
        async fn search_coins(
            &self,
            _q: &str,
            _cap: usize,
        ) -> Result<Vec<crate::providers::CoinSearchResult>, ProviderError> {
            match self.behavior {
                SearchBehavior::Cooldown => Err(ProviderError::Pacer(AcquireSlotError::Cooldown(
                    "coingecko".to_string(),
                    chrono::Utc::now(),
                ))),
                SearchBehavior::CreditExhausted => Err(ProviderError::Pacer(
                    AcquireSlotError::CreditExhausted("coingecko".to_string()),
                )),
                SearchBehavior::Empty => Ok(vec![]),
            }
        }
        async fn fetch_coin_tickers(
            &self,
            _coin_id: &str,
            _cap: usize,
        ) -> Result<Vec<crate::providers::MarketSearchResult>, ProviderError> {
            Ok(vec![])
        }
    }

    fn search_stub_server(behavior: SearchBehavior) -> TestServer {
        use crate::api::{build_api_router, AppState};
        use std::sync::Arc;
        use tokio::sync::broadcast;

        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/crypto_collector_test")
            .expect("lazy pool");
        let (coin_quote_tx, _) = broadcast::channel(16);
        let (coin_candle_tx, _) = broadcast::channel(16);
        let state = AppState {
            pool,
            chain: Arc::new(vec![
                Arc::new(SearchStubProvider { behavior }) as Arc<dyn crate::providers::Provider>
            ]),
            search_provider: "coingecko".to_string(),
            coingecko_base_url: "https://api.coingecko.com".to_string(),
            http_client: reqwest::Client::new(),
            coin_quote_tx,
            coin_candle_tx,
        };
        TestServer::new(build_api_router(state))
    }

    // AC-API-411: a pacer cooldown error → 503 ServiceUnavailable with the {code, message} body.
    #[tokio::test]
    async fn search_pacer_cooldown_returns_503() {
        let server = search_stub_server(SearchBehavior::Cooldown);
        let resp = server.get("/v1/coins/search?q=btc").await;
        assert_eq!(resp.status_code(), 503);
        let body: serde_json::Value = resp.json();
        assert_eq!(body["code"], "SERVICE_UNAVAILABLE");
    }

    // AC-API-411: a credit-exhaustion error (the other LOCKED variant) → 503.
    #[tokio::test]
    async fn search_credit_exhausted_returns_503() {
        let server = search_stub_server(SearchBehavior::CreditExhausted);
        let resp = server.get("/v1/coins/search?q=btc").await;
        assert_eq!(resp.status_code(), 503);
    }

    // AC-API-411: a genuine empty result → 200 with an empty result set (not 503).
    #[tokio::test]
    async fn search_empty_result_returns_200_empty() {
        let server = search_stub_server(SearchBehavior::Empty);
        let resp = server.get("/v1/coins/search?q=nomatch").await;
        assert_eq!(resp.status_code(), 200);
        let body: serde_json::Value = resp.json();
        assert_eq!(body["items"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn list_coins_handler_exists() {
        let _ = list_coins;
    }

    #[test]
    fn register_coin_handler_exists() {
        let _ = register_coin;
    }

    #[test]
    fn search_coins_handler_exists() {
        let _ = search_coins;
    }

    #[test]
    fn validate_coin_id_rejects_empty() {
        assert!(validate_coin_id("").is_err());
        assert!(validate_coin_id("   ").is_err());
    }

    #[test]
    fn validate_coin_id_accepts_valid() {
        assert!(validate_coin_id("bitcoin").is_ok());
        assert!(validate_coin_id("ethereum").is_ok());
    }

    #[test]
    fn validate_coin_status_valid() {
        assert!(validate_coin_status("active").is_ok());
        assert!(validate_coin_status("paused").is_ok());
        assert!(validate_coin_status("error").is_ok());
    }

    #[test]
    fn validate_coin_status_rejects_unknown() {
        assert!(validate_coin_status("deleted").is_err());
        assert!(validate_coin_status("ACTIVE").is_err());
    }

    #[test]
    fn paginate_coins_has_more_returns_cursor() {
        use chrono::Utc;
        let mut items = vec![];
        for i in 0..3i64 {
            items.push(crate::models::coin::TrackedCoin {
                coin_id: format!("coin{i}"),
                symbol: "X".into(),
                name: "X".into(),
                status: "active".into(),
                registered_at: Utc::now(),
                last_collected_at: None,
                error: None,
                live_poll_interval: None,
            });
        }
        let (trimmed, next_cursor) = paginate_coins(items, 2);
        assert_eq!(trimmed.len(), 2);
        assert!(next_cursor.is_some());
        let key: CoinListKey = decode_keyset_cursor(next_cursor.as_ref().unwrap()).unwrap();
        assert_eq!(key.coin_id, "coin1");
    }

    #[test]
    fn paginate_coins_no_more_returns_null_cursor() {
        use chrono::Utc;
        let items = vec![crate::models::coin::TrackedCoin {
            coin_id: "bitcoin".into(),
            symbol: "BTC".into(),
            name: "Bitcoin".into(),
            status: "active".into(),
            registered_at: Utc::now(),
            last_collected_at: None,
            error: None,
            live_poll_interval: None,
        }];
        let (trimmed, next_cursor) = paginate_coins(items, 100);
        assert_eq!(trimmed.len(), 1);
        assert!(next_cursor.is_none());
    }

    // DB-gated integration tests
    #[tokio::test]
    #[ignore]
    async fn db_register_coin_returns_201_and_200_on_repeat() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::db::connect(&url).await.expect("db connect");
        use tokio::sync::broadcast;
        let (coin_quote_tx, _) = broadcast::channel(16);
        let (coin_candle_tx, _) = broadcast::channel(16);
        let state = crate::api::AppState {
            pool: pool.clone(),
            chain: std::sync::Arc::new(vec![]),
            search_provider: "coingecko".into(),
            coingecko_base_url: "https://api.coingecko.com".into(),
            http_client: reqwest::Client::new(),
            coin_quote_tx,
            coin_candle_tx,
        };
        let server = TestServer::new(crate::api::build_api_router(state));

        let resp = server
            .post("/v1/coins")
            .json(&serde_json::json!({
                "coin_id": "test-coin-api001",
                "symbol": "TST",
                "name": "Test Coin"
            }))
            .await;
        assert_eq!(resp.status_code(), 201);

        let resp2 = server
            .post("/v1/coins")
            .json(&serde_json::json!({
                "coin_id": "test-coin-api001",
                "symbol": "TST",
                "name": "Test Coin"
            }))
            .await;
        assert_eq!(resp2.status_code(), 200);

        sqlx::query("DELETE FROM tracked_coins WHERE coin_id = 'test-coin-api001'")
            .execute(&pool)
            .await
            .ok();
    }

    // ── SPEC-API-005 M5 (F-32, REQ-API-407/408): idempotent ON CONFLICT + atomic tx ──

    #[cfg(test)]
    fn register_state(pool: sqlx::PgPool) -> crate::api::AppState {
        use tokio::sync::broadcast;
        let (coin_quote_tx, _) = broadcast::channel(16);
        let (coin_candle_tx, _) = broadcast::channel(16);
        crate::api::AppState {
            pool,
            chain: std::sync::Arc::new(vec![]),
            search_provider: "coingecko".into(),
            coingecko_base_url: "https://api.coingecko.com".into(),
            http_client: reqwest::Client::new(),
            coin_quote_tx,
            coin_candle_tx,
        }
    }

    // AC-API-407 [DB-backed]: two CONCURRENT registrations of the same coin_id yield exactly one
    // 201 and one 200 — never a 500 PK violation (ON CONFLICT DO NOTHING RETURNING + re-select).
    #[tokio::test]
    #[ignore]
    async fn db_register_coin_concurrent_duplicate_no_500() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::db::connect(&url).await.expect("db connect");
        let sfx = chrono::Utc::now().timestamp_nanos_opt().unwrap();
        let coin = format!("test-coin-api005-conc-{sfx}");
        sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
            .bind(&coin)
            .execute(&pool)
            .await
            .ok();

        // Two servers on the same pool, fired concurrently.
        let server_a = TestServer::new(crate::api::build_api_router(register_state(pool.clone())));
        let server_b = TestServer::new(crate::api::build_api_router(register_state(pool.clone())));
        let body = serde_json::json!({ "coin_id": coin, "symbol": "TST", "name": "Test Coin" });
        let (ra, rb) = tokio::join!(
            server_a.post("/v1/coins").json(&body),
            server_b.post("/v1/coins").json(&body),
        );
        let codes = [ra.status_code().as_u16(), rb.status_code().as_u16()];
        assert!(
            !codes.contains(&500),
            "concurrent duplicate registration must never 500; got {codes:?}"
        );
        assert!(
            codes.contains(&201) && codes.contains(&200),
            "concurrent duplicate must yield exactly one 201 and one 200; got {codes:?}"
        );

        sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
            .bind(&coin)
            .execute(&pool)
            .await
            .ok();
    }

    // AC-API-408 [DB-backed]: a fresh registration commits the tracked_coins row AND its three
    // collection_queue enqueues atomically — both present after a 201.
    #[tokio::test]
    #[ignore]
    async fn db_register_coin_insert_and_enqueues_are_atomic() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::db::connect(&url).await.expect("db connect");
        let sfx = chrono::Utc::now().timestamp_nanos_opt().unwrap();
        let coin = format!("test-coin-api005-atomic-{sfx}");
        sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
            .bind(&coin)
            .execute(&pool)
            .await
            .ok();

        let server = TestServer::new(crate::api::build_api_router(register_state(pool.clone())));
        let resp = server
            .post("/v1/coins")
            .json(&serde_json::json!({ "coin_id": coin, "symbol": "TST", "name": "Test Coin" }))
            .await;
        assert_eq!(resp.status_code(), 201);

        // The coin row is committed.
        let coin_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM tracked_coins WHERE coin_id = $1")
                .bind(&coin)
                .fetch_one(&pool)
                .await
                .expect("count coin");
        assert_eq!(coin_rows, 1, "the tracked_coins row must be committed");

        // The three initial enqueues (metadata, market, candles) are committed in the same tx.
        let kinds: Vec<String> = sqlx::query_scalar(
            "SELECT kind FROM collection_queue \
             WHERE target_kind = 'coin' AND target_id = $1 ORDER BY kind",
        )
        .bind(&coin)
        .fetch_all(&pool)
        .await
        .expect("select enqueues");
        assert_eq!(
            kinds,
            vec![
                "candles".to_string(),
                "market".to_string(),
                "metadata".to_string()
            ],
            "insert + the three enqueues must commit atomically (REQ-API-408)"
        );

        sqlx::query("DELETE FROM collection_queue WHERE target_kind = 'coin' AND target_id = $1")
            .bind(&coin)
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
            .bind(&coin)
            .execute(&pool)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore]
    async fn db_get_coin_not_found_returns_404() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::db::connect(&url).await.expect("db connect");
        use tokio::sync::broadcast;
        let (coin_quote_tx, _) = broadcast::channel(16);
        let (coin_candle_tx, _) = broadcast::channel(16);
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
        let resp = server.get("/v1/coins/no-such-coin-xyz-9999").await;
        assert_eq!(resp.status_code(), 404);
    }
}
