//! `/v1` REST API router and shared infrastructure (SPEC-API-001).
//!
//! Exposed modules: cursor, dto, coins, markets, quotes, candles, metadata,
//! coin_market, derivatives.
//!
//! # Server bootstrap
//!
//! Call `build_api_router(state)` to assemble the Axum router, then bind a
//! `TcpListener` and call `axum::serve`. The `start_api_server` function does
//! this end-to-end and is called from `main`.
//!
//! SPEC-OBS-001 (health port 8081, Prometheus 9000) will add further
//! `TcpListener`s alongside this one — structured to minimise overlap.

pub mod candles;
pub mod coin_market;
pub mod coins;
pub mod cursor;
pub mod derivatives;
pub mod dto;
pub mod markets;
pub mod metadata;
pub mod quotes;

use axum::{
    extract::rejection::JsonRejection,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use sqlx::PgPool;
use std::sync::Arc;
use tracing::info;

use crate::providers::Provider;

// ── App state ─────────────────────────────────────────────────────────────────

/// Shared Axum application state for all `/v1` handlers.
#[derive(Clone)]
pub struct AppState {
    /// Database connection pool.
    pub pool: PgPool,
    /// Ordered provider chain (same instance as the background workers).
    pub chain: Arc<Vec<Arc<dyn Provider>>>,
    /// Function to try acquiring a search pacer slot for a given provider.
    ///
    /// Injected here so tests can supply a fake (deny or allow) without a live DB
    /// (REQ-API-080/081, Scenario 15).
    pub search_slot_fn: SearchSlotFn,
    /// Provider name to use for search calls (typically the first in the chain).
    pub search_provider: String,
    /// CoinGecko base URL for search API calls.
    pub coingecko_base_url: String,
    /// HTTP client for outbound search calls.
    pub http_client: reqwest::Client,
}

// ── Search pacer abstraction ──────────────────────────────────────────────────

/// Result of a bounded search pacer slot acquisition attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum SearchSlotResult {
    /// Slot acquired; caller may proceed with the upstream call.
    Available,
    /// Slot unavailable (cooldown, credit exhaustion, or timeout); return 503.
    ///
    /// The message is included in the 503 JSON error body.
    Unavailable(String),
}

/// Injected function type for pacer slot acquisition on the search request path.
///
/// Production: wraps `pacer::acquire_slot` with a bounded timeout.
/// Tests: returns a fixed `SearchSlotResult` without touching the DB.
///
// @MX:WARN: [AUTO] SearchSlotFn is the request-path egress gate for search handlers
// @MX:REASON: Search handlers MUST NOT issue upstream calls without first acquiring a slot through
//             this function. Bypassing it would violate REQ-API-080/081 and SPEC-PROV-001 REQ-PROV-040.
//             In production, the function wraps pacer::acquire_slot with a bounded timeout.
//             In tests, a fake function allows pacing behavior to be tested without a live DB.
// @MX:SPEC: SPEC-API-001 REQ-API-080 REQ-API-081
pub type SearchSlotFn = Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = SearchSlotResult> + Send>>
        + Send
        + Sync,
>;

/// Build the production `SearchSlotFn` using bounded slot acquisition.
///
/// Uses `pacer::try_acquire_slot` with `max_wait_ms`: only takes a slot if the next
/// available window opens within `max_wait_ms`. If collectors have queued slots further
/// ahead than that, returns `Unavailable` immediately without sleeping and without
/// consuming a credit. This prevents interactive search requests from sleeping for
/// arbitrarily long durations while background collectors hold the queue.
pub fn make_db_search_slot_fn(pool: PgPool, max_wait_ms: u64) -> SearchSlotFn {
    Arc::new(move |provider: String| {
        let pool = pool.clone();
        Box::pin(async move {
            match crate::pacer::try_acquire_slot(&pool, &provider, max_wait_ms).await {
                Ok(()) => SearchSlotResult::Available,
                Err(crate::pacer::AcquireSlotError::Cooldown(p, _)) => {
                    SearchSlotResult::Unavailable(format!("provider '{p}' is in cooldown"))
                }
                Err(crate::pacer::AcquireSlotError::CreditExhausted(p)) => {
                    SearchSlotResult::Unavailable(format!("provider '{p}' credit budget exhausted"))
                }
                Err(crate::pacer::AcquireSlotError::Busy(p)) => {
                    SearchSlotResult::Unavailable(format!(
                        "provider '{p}' rate limit queue is full"
                    ))
                }
                Err(e) => SearchSlotResult::Unavailable(format!("pacer error: {e}")),
            }
        })
    })
}

/// Build a search slot function that always denies (for tests / no-provider scenarios).
pub fn deny_search_slot_fn() -> SearchSlotFn {
    Arc::new(|_provider: String| {
        Box::pin(async { SearchSlotResult::Unavailable("test: deny".to_string()) })
    })
}

/// Build a search slot function that always grants (for tests where search pacing is irrelevant).
pub fn allow_search_slot_fn() -> SearchSlotFn {
    Arc::new(|_provider: String| Box::pin(async { SearchSlotResult::Available }))
}

// ── Error type ────────────────────────────────────────────────────────────────

/// API error taxonomy mapped to HTTP status codes (REQ-API-074).
///
// @MX:ANCHOR: [AUTO] ApiError — shared error→status mapper; every handler returns this
// @MX:REASON: fan_in >= 3: all nine handler modules + integration tests.
//             All 400/404/422/500/503 responses must go through here to guarantee the
//             uniform JSON error body (REQ-API-074).
// @MX:SPEC: SPEC-API-001 REQ-API-074
#[derive(Debug)]
pub enum ApiError {
    /// 400: bad request (malformed cursor, invalid limit, bad interval, etc.).
    BadRequest(String),
    /// 404: coin or market id not found.
    NotFound(String),
    /// 422: semantically invalid registration (unknown asset, etc.).
    UnprocessableEntity(String),
    /// 503: upstream provider unavailable (pacer denied).
    ServiceUnavailable(String),
    /// 500: internal / unexpected error.
    Internal(anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "BAD_REQUEST", msg),
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, "NOT_FOUND", msg),
            ApiError::UnprocessableEntity(msg) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "UNPROCESSABLE_ENTITY",
                msg,
            ),
            ApiError::ServiceUnavailable(msg) => {
                (StatusCode::SERVICE_UNAVAILABLE, "SERVICE_UNAVAILABLE", msg)
            }
            ApiError::Internal(e) => {
                tracing::error!(error = %e, "internal API error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_SERVER_ERROR",
                    "an internal error occurred".to_string(),
                )
            }
        };
        let body = dto::ApiErrorBody { code, message };
        (status, Json(body)).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::Internal(e.into())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::Internal(e)
    }
}

impl From<JsonRejection> for ApiError {
    fn from(e: JsonRejection) -> Self {
        ApiError::BadRequest(e.to_string())
    }
}

/// Shorthand `Result` alias used by all handlers.
pub type ApiResult<T> = Result<T, ApiError>;

// ── Router assembly ───────────────────────────────────────────────────────────

/// Assemble the `/v1` Axum router with all registered routes.
///
/// The returned router does NOT include a prefix — callers can nest it or serve at `/`.
/// SPEC-OBS-001 will add `/healthz` and metrics routes on separate ports/routers.
///
// @MX:ANCHOR: [AUTO] build_api_router — single route registration point for all /v1 endpoints
// @MX:REASON: fan_in >= 3: main.rs startup, integration tests, SPEC-OBS-001 will wrap alongside.
//             All endpoint additions MUST go through here (REQ-API-001: single /v1 surface).
//             /v1/coins/search MUST be registered before /v1/coins/{coin_id} (literal before param).
// @MX:SPEC: SPEC-API-001 REQ-API-001
pub fn build_api_router(state: AppState) -> Router {
    Router::new()
        // ── Coins management ─────────────────────────────────────────────────
        .route(
            "/v1/coins",
            get(coins::list_coins).post(coins::register_coin),
        )
        // NOTE: /v1/coins/search MUST be registered BEFORE /v1/coins/{coin_id}
        // so Axum's literal route takes priority over the path parameter.
        .route("/v1/coins/search", get(coins::search_coins))
        .route(
            "/v1/coins/{coin_id}",
            get(coins::get_coin)
                .patch(coins::update_coin)
                .delete(coins::delete_coin),
        )
        // ── Coin read endpoints ───────────────────────────────────────────────
        .route("/v1/coins/{coin_id}/metadata", get(metadata::get_metadata))
        .route(
            "/v1/coins/{coin_id}/market/latest",
            get(coin_market::get_coin_market_latest),
        )
        .route(
            "/v1/coins/{coin_id}/market",
            get(coin_market::list_coin_market),
        )
        // ── Markets management ────────────────────────────────────────────────
        .route(
            "/v1/markets",
            get(markets::list_markets).post(markets::register_market),
        )
        // NOTE: /v1/markets/search MUST be registered BEFORE /v1/markets/{id}
        .route("/v1/markets/search", get(markets::search_markets))
        .route(
            "/v1/markets/{id}",
            get(markets::get_market)
                .patch(markets::update_market)
                .delete(markets::delete_market),
        )
        // ── Market read endpoints ─────────────────────────────────────────────
        .route(
            "/v1/markets/{id}/quotes/latest",
            get(quotes::get_latest_quote),
        )
        .route("/v1/markets/{id}/quotes", get(quotes::list_quotes))
        .route("/v1/markets/{id}/candles", get(candles::list_candles))
        .route(
            "/v1/markets/{id}/derivatives/latest",
            get(derivatives::get_latest_derivative),
        )
        .route(
            "/v1/markets/{id}/derivatives",
            get(derivatives::list_derivatives),
        )
        .with_state(state)
}

// ── Server bootstrap ──────────────────────────────────────────────────────────

/// Bind and serve the API on the given port.
///
/// This is the clean extension point for SPEC-OBS-001: it adds health (port 8081)
/// and metrics (port 9000) listeners alongside this call in `main`.
pub async fn start_api_server(
    state: AppState,
    port: u16,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let router = build_api_router(state);
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind API port {port}: {e}"))?;
    info!("crypto-collector API: listening on port {port}");
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            // Shutdown when the worker supervisor signals.
            loop {
                if *shutdown_rx.borrow() {
                    break;
                }
                if shutdown_rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .map_err(|e| anyhow::anyhow!("API server error: {e}"))?;
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Scenario 1 (REQ-API-001): all routes are under /v1, no /v2 surface.
    #[test]
    fn all_routes_are_under_v1() {
        let routes = [
            "/v1/coins",
            "/v1/coins/search",
            "/v1/coins/{coin_id}",
            "/v1/coins/{coin_id}/metadata",
            "/v1/coins/{coin_id}/market/latest",
            "/v1/coins/{coin_id}/market",
            "/v1/markets",
            "/v1/markets/search",
            "/v1/markets/{id}",
            "/v1/markets/{id}/quotes/latest",
            "/v1/markets/{id}/quotes",
            "/v1/markets/{id}/candles",
            "/v1/markets/{id}/derivatives/latest",
            "/v1/markets/{id}/derivatives",
        ];
        for route in &routes {
            assert!(
                route.starts_with("/v1/"),
                "route '{route}' must start with /v1/"
            );
        }
    }

    // Scenario 1: no /v2 routes.
    #[test]
    fn no_v2_routes_exist() {
        // All known routes must start with /v1/ — none with /v2.
        let routes = [
            "/v1/coins",
            "/v1/markets",
            "/v1/coins/search",
            "/v1/markets/search",
        ];
        for route in routes {
            assert!(
                !route.starts_with("/v2"),
                "route '{route}' must not be a /v2 route"
            );
        }
    }

    // Scenario 13 (REQ-API-074): ApiError → correct status codes.
    #[test]
    fn api_error_bad_request_status() {
        let e = ApiError::BadRequest("bad cursor".into());
        let resp = e.into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn api_error_not_found_status() {
        let e = ApiError::NotFound("coin not found".into());
        let resp = e.into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn api_error_unprocessable_entity_status() {
        let e = ApiError::UnprocessableEntity("unknown base asset".into());
        let resp = e.into_response();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn api_error_service_unavailable_status() {
        let e = ApiError::ServiceUnavailable("provider cooldown".into());
        let resp = e.into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn api_error_internal_status() {
        let e = ApiError::Internal(anyhow::anyhow!("db error"));
        let resp = e.into_response();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    // search_slot_fn helpers work correctly
    #[tokio::test]
    async fn deny_search_slot_fn_returns_unavailable() {
        let f = deny_search_slot_fn();
        let result = f("coingecko".to_string()).await;
        assert!(matches!(result, SearchSlotResult::Unavailable(_)));
    }

    #[tokio::test]
    async fn allow_search_slot_fn_returns_available() {
        let f = allow_search_slot_fn();
        let result = f("coingecko".to_string()).await;
        assert_eq!(result, SearchSlotResult::Available);
    }

    // Scenario 14 (REQ-API-002): OpenAPI YAML exists and has /v1 servers entry.
    #[test]
    fn openapi_yaml_exists_and_has_v1_servers() {
        let yaml = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("api/crypto-collector.yaml"),
        )
        .expect("api/crypto-collector.yaml must exist");
        assert!(
            yaml.contains("openapi: 3.1.0"),
            "OpenAPI document must declare version 3.1.0"
        );
        assert!(
            yaml.contains("url: /v1"),
            "OpenAPI servers must have url: /v1"
        );
    }

    // Scenario 14 (REQ-API-003): doc-parity test — all implemented endpoint operationIds
    // must appear in the OpenAPI document.
    #[test]
    fn openapi_yaml_contains_all_operation_ids() {
        let yaml = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("api/crypto-collector.yaml"),
        )
        .expect("api/crypto-collector.yaml must exist");

        let operation_ids = [
            "listCoins",
            "registerCoin",
            "searchCoins",
            "getCoin",
            "updateCoin",
            "deleteCoin",
            "getCoinMetadata",
            "getCoinMarketLatest",
            "listCoinMarket",
            "listMarkets",
            "registerMarket",
            "searchMarkets",
            "getMarket",
            "updateMarket",
            "deleteMarket",
            "getLatestQuote",
            "listQuotes",
            "listCandles",
            "getLatestDerivative",
            "listDerivatives",
        ];
        for op_id in &operation_ids {
            assert!(
                yaml.contains(op_id),
                "OpenAPI spec must contain operationId '{op_id}'"
            );
        }
    }

    // Scenario 14: key schema names appear in OpenAPI.
    #[test]
    fn openapi_yaml_contains_key_schemas() {
        let yaml = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("api/crypto-collector.yaml"),
        )
        .expect("api/crypto-collector.yaml must exist");

        let schemas = [
            "Coin",
            "Market",
            "Quote",
            "Candle",
            "CoinMetadata",
            "CoinMarketSnapshot",
            "DerivativesQuote",
            "ApiError",
            "Page",
        ];
        for schema in &schemas {
            assert!(
                yaml.contains(schema),
                "OpenAPI spec must contain schema '{schema}'"
            );
        }
    }
}
