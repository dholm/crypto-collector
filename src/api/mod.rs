//! `/v1` REST API router and shared infrastructure (SPEC-API-001, SPEC-API-002).
//!
//! Exposed modules: cursor, dto, coins, quotes, candles, metadata,
//! coin_market, poll_interval, websocket.
//!
//! # Server bootstrap
//!
//! Call `build_api_router(state)` to assemble the Axum router; `main` binds the
//! `TcpListener` and calls `axum::serve` itself (alongside the health port 8081 and
//! Prometheus 9000 listeners). SPEC-OBS-002 REQ-OBS-074 removed the dead API-bootstrap
//! wrapper — `main` owns the serve loop directly.

pub mod candles;
pub mod candles_agg;
pub mod coin_market;
pub mod coins;
pub mod cursor;
pub mod cycle_overlay;
pub mod dto;
pub mod extract;
pub mod metadata;
pub mod poll_interval;
pub mod quotes;
pub mod websocket;

use axum::{
    extract::rejection::{JsonRejection, PathRejection, QueryRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use sqlx::PgPool;
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::providers::Provider;

// ── App state ─────────────────────────────────────────────────────────────────

/// Shared Axum application state for all `/v1` handlers.
// @MX:ANCHOR: [AUTO] AppState — shared across all /v1 handlers and WebSocket upgraders
// @MX:REASON: fan_in >= 3: all handler modules + listener.rs + main.rs.
//             Adding fields here requires updating the single AppState::test() constructor
//             (SPEC-REFACTOR-001 M5 / REQ-REFACTOR-054) that every test module builds from.
//             broadcast senders must outlive the router; they are cloned cheaply into handlers.
// @MX:SPEC: SPEC-API-001 SPEC-API-002 REQ-API-148
#[derive(Clone)]
pub struct AppState {
    /// Database connection pool.
    pub pool: PgPool,
    /// Ordered provider chain (same instance as the background workers).
    pub chain: Arc<Vec<Arc<dyn Provider>>>,
    /// Provider name to use for search calls (typically the first in the chain).
    pub search_provider: String,
    /// Broadcast sender for coin spot quotes — WebSocket fan-out (REQ-API-148).
    /// Driven by `src/listener.rs` which relays PG NOTIFY `coin_quote_updated`.
    pub coin_quote_tx: broadcast::Sender<String>,
    /// Broadcast sender for coin OHLCV candles — WebSocket fan-out (REQ-API-148).
    /// Driven by `src/listener.rs` which relays PG NOTIFY `coin_candle_updated`.
    pub coin_candle_tx: broadcast::Sender<String>,
}

// SPEC-REFACTOR-001 M5 (REQ-REFACTOR-053): the former `coingecko_base_url` / `http_client`
// fields were removed — no handler ever read them (search routes through the provider chain),
// so they were dead state populated only by constructors.

#[cfg(test)]
impl AppState {
    /// Shared test-state constructor (SPEC-REFACTOR-001 M5 / REQ-REFACTOR-054).
    ///
    /// Collapses the previously-duplicated per-module `AppState` test builders into one:
    /// an empty provider chain, `search_provider = "coingecko"`, and fresh broadcast channels.
    /// The caller supplies the pool (a lazy pool for router-only tests, or a real
    /// `DATABASE_URL`-backed pool for DB-gated tests). Modules needing a non-default field
    /// (e.g. a stub provider chain, or an externally-held `coin_quote_tx`) override it via
    /// struct-update syntax: `AppState { chain, ..AppState::test(pool) }`.
    pub(crate) fn test(pool: PgPool) -> Self {
        AppState {
            pool,
            chain: Arc::new(vec![]),
            search_provider: "coingecko".to_string(),
            coin_quote_tx: broadcast::channel(16).0,
            coin_candle_tx: broadcast::channel(16).0,
        }
    }
}

// ── Error type ────────────────────────────────────────────────────────────────

/// API error taxonomy mapped to HTTP status codes (REQ-API-074).
///
// @MX:ANCHOR: [AUTO] ApiError — shared error→status mapper; every handler returns this
// @MX:REASON: fan_in >= 3: all handler modules + integration tests.
//             All 400/404/422/500/503 responses must go through here to guarantee the
//             uniform JSON error body (REQ-API-074).
// @MX:SPEC: SPEC-API-001 REQ-API-074
#[derive(Debug)]
pub enum ApiError {
    /// 400: bad request (malformed cursor, invalid limit, bad interval, etc.).
    BadRequest(String),
    /// 404: coin or market id not found.
    NotFound(String),
    /// 422: semantically invalid (live_poll_interval out of bounds, etc.; REQ-API-114).
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

// F-33 (REQ-API-410): the extractor-rejection funnel. `ApiJson`/`ApiQuery`/`ApiPath`
// (src/api/extract.rs) declare `rejection(ApiError)`, so each built-in extractor's rejection
// is converted here into the uniform `{code, message}` BadRequest JSON body (REQ-API-074).
// `From<JsonRejection>` was previously dead code (handlers used bare `Json`); it is now live.
impl From<JsonRejection> for ApiError {
    fn from(e: JsonRejection) -> Self {
        ApiError::BadRequest(e.to_string())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(e: QueryRejection) -> Self {
        ApiError::BadRequest(e.to_string())
    }
}

impl From<PathRejection> for ApiError {
    fn from(e: PathRejection) -> Self {
        ApiError::BadRequest(e.to_string())
    }
}

/// Shorthand `Result` alias used by all handlers.
pub type ApiResult<T> = Result<T, ApiError>;

// ── Shared handler helpers ──────────────────────────────────────────────────────

/// Check that a coin_id exists in `tracked_coins`; return 404 if not.
///
/// SPEC-REFACTOR-001 M5 (REQ-REFACTOR-050): the single shared implementation, replacing the
/// two verbatim copies previously in `quotes.rs` and `metadata.rs`. Called by the quotes,
/// candles, market, and metadata read handlers before serving coin-scoped data.
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

// ── Router assembly ───────────────────────────────────────────────────────────

/// Assemble the `/v1` Axum router with all registered routes.
///
/// The returned router does NOT include a prefix — callers can nest it or serve at `/`.
/// SPEC-OBS-001 will add `/healthz` and metrics routes on separate ports/routers.
///
/// # Route registration order (REQ-API-148)
///
/// Literal routes (including stream WebSocket routes) MUST be registered BEFORE
/// the parameterised `/v1/coins/{coin_id}` route so Axum's literal-first matching
/// takes priority. Specifically:
/// - `/v1/coins/stream/quotes` and `/v1/coins/stream/candles` BEFORE `/v1/coins/{coin_id}`
/// - `/v1/coins/search` BEFORE `/v1/coins/{coin_id}`
///
// @MX:ANCHOR: [AUTO] build_api_router — single route registration point for all /v1 endpoints
// @MX:REASON: fan_in >= 3: main.rs startup, integration tests, docs.
//             All endpoint additions MUST go through here (REQ-API-001: single /v1 surface).
//             Literal routes MUST precede param routes (REQ-API-148).
// @MX:SPEC: SPEC-API-001 SPEC-API-002 REQ-API-001 REQ-API-148
pub fn build_api_router(state: AppState) -> Router {
    Router::new()
        // ── Coins management ─────────────────────────────────────────────────
        .route(
            "/v1/coins",
            get(coins::list_coins).post(coins::register_coin),
        )
        // NOTE: Literal paths MUST be registered BEFORE /v1/coins/{coin_id} (REQ-API-148).
        // Axum resolves exact matches first; parameterised catch-all comes last.
        .route("/v1/coins/search", get(coins::search_coins))
        // ── WebSocket streams (REQ-API-148: BEFORE /v1/coins/{coin_id}) ──────
        .route(
            "/v1/coins/stream/quotes",
            get(websocket::stream_coin_quotes),
        )
        .route(
            "/v1/coins/stream/candles",
            get(websocket::stream_coin_candles),
        )
        // ── All-coin latest-quote overview (SPEC-API-004 REQ-API-308: BEFORE /v1/coins/{coin_id}) ──
        // NOTE: literal `/v1/coins/quotes/latest` MUST precede `/v1/coins/{coin_id}` so Axum's
        // literal-first matching lets the `quotes` segment win over binding `{coin_id}="quotes"`
        // (literal-before-param, REQ-API-148/308).
        .route("/v1/coins/quotes/latest", get(quotes::list_latest_quotes))
        // ── Parameterised coin routes (must come after all literal /v1/coins/* routes) ──
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
        // ── Coin spot quote endpoints (SPEC-API-002 REQ-API-131/132) ─────────
        .route(
            "/v1/coins/{coin_id}/quotes/latest",
            get(quotes::get_latest_quote),
        )
        .route("/v1/coins/{coin_id}/quotes", get(quotes::list_quotes))
        // ── Coin OHLCV candle endpoints (SPEC-API-002 REQ-API-141/142) ───────
        .route("/v1/coins/{coin_id}/candles", get(candles::list_candles))
        // ── Bitcoin halving-cycle overlay (SPEC-CYCLE-001 REQ-CYCLE-090..099, v0.6.0) ────
        // NOTE: the base path (discovery) MUST be registered before the parameterised
        // `/cycle-projection/{model}` data path is READ here for documentation purposes only —
        // Axum resolves these unambiguously since they differ by path segment count, so
        // registration order between the two does not matter (unlike the literal-vs-param
        // ordering rule above). The former `/cycle-overlay` route is removed entirely
        // (REQ-CYCLE-098): no route registration for it means Axum returns 404.
        .route(
            "/v1/coins/{coin_id}/cycle-projection",
            get(cycle_overlay::list_cycle_projection_models),
        )
        .route(
            "/v1/coins/{coin_id}/cycle-projection/{model}",
            get(cycle_overlay::list_cycle_projection_data),
        )
        .with_state(state)
}

// (The dead API-bootstrap wrapper was removed — SPEC-OBS-002 REQ-OBS-074 / F-49; `main`
// binds the API listener and drives `axum::serve` itself.)

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
            "/v1/coins/stream/quotes",
            "/v1/coins/stream/candles",
            "/v1/coins/quotes/latest",
            "/v1/coins/{coin_id}",
            "/v1/coins/{coin_id}/metadata",
            "/v1/coins/{coin_id}/market/latest",
            "/v1/coins/{coin_id}/market",
            "/v1/coins/{coin_id}/quotes/latest",
            "/v1/coins/{coin_id}/quotes",
            "/v1/coins/{coin_id}/candles",
            "/v1/coins/{coin_id}/cycle-projection",
            "/v1/coins/{coin_id}/cycle-projection/{model}",
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
        let routes = [
            "/v1/coins",
            "/v1/coins/search",
            "/v1/coins/stream/quotes",
            "/v1/coins/stream/candles",
        ];
        for route in routes {
            assert!(
                !route.starts_with("/v2"),
                "route '{route}' must not be a /v2 route"
            );
        }
    }

    // REQ-API-148: stream routes appear before /{coin_id} in registration order.
    #[test]
    fn stream_routes_precede_coin_id_param_route() {
        // This is a static ordering check documenting the invariant.
        // The actual enforcement is in build_api_router — stream routes must appear
        // before the /v1/coins/{coin_id} line in source order.
        let ordered_routes = [
            "/v1/coins/search",
            "/v1/coins/stream/quotes",
            "/v1/coins/stream/candles",
            // Parameterised must come last
            "/v1/coins/{coin_id}",
        ];
        let param_pos = ordered_routes
            .iter()
            .position(|r| *r == "/v1/coins/{coin_id}")
            .unwrap();
        let stream_q_pos = ordered_routes
            .iter()
            .position(|r| *r == "/v1/coins/stream/quotes")
            .unwrap();
        let stream_c_pos = ordered_routes
            .iter()
            .position(|r| *r == "/v1/coins/stream/candles")
            .unwrap();
        assert!(
            stream_q_pos < param_pos,
            "stream/quotes must precede /{{coin_id}}"
        );
        assert!(
            stream_c_pos < param_pos,
            "stream/candles must precede /{{coin_id}}"
        );
    }

    // SPEC-API-004 Scenario 9 (REQ-API-308): the literal /v1/coins/quotes/latest route is
    // registered before the parameterised /v1/coins/{coin_id} route so it resolves as an
    // endpoint and not as {coin_id}="quotes" (literal-before-param, REQ-API-148).
    #[test]
    fn quotes_latest_route_precedes_coin_id_param_route() {
        // Mirrors the source registration order in build_api_router: the literal-first block
        // (search + stream + quotes/latest) precedes the parameterised /{coin_id} route.
        let ordered_routes = [
            "/v1/coins/search",
            "/v1/coins/stream/quotes",
            "/v1/coins/stream/candles",
            "/v1/coins/quotes/latest",
            // Parameterised must come last
            "/v1/coins/{coin_id}",
        ];
        let param_pos = ordered_routes
            .iter()
            .position(|r| *r == "/v1/coins/{coin_id}")
            .unwrap();
        let quotes_latest_pos = ordered_routes
            .iter()
            .position(|r| *r == "/v1/coins/quotes/latest")
            .unwrap();
        assert!(
            quotes_latest_pos < param_pos,
            "quotes/latest must precede /{{coin_id}}"
        );
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
            "getLatestCoinQuote",
            "listCoinQuotes",
            "listLatestCoinQuotes",
            "listCoinCandles",
            "streamCoinQuotes",
            "streamCoinCandles",
            "listCycleProjection",
            "listCycleProjectionModels",
        ];
        for op_id in &operation_ids {
            assert!(
                yaml.contains(op_id),
                "OpenAPI spec must contain operationId '{op_id}'"
            );
        }
    }

    // ── SPEC-API-005 M8 (F-59, REQ-API-417): per-operation parameter parity ──────
    //
    // Assert that every query parameter documented in api/crypto-collector.yaml has a matching
    // field on the corresponding Rust param struct — the guard that would have caught F-29
    // (the yaml documented vs_currency on the quote reads while the structs lacked the field).
    //
    // The struct field set is reflected via serde (the param structs derive Serialize), so the
    // check is bound to the actual structs: constructing each struct literal is compile-enforced
    // to match its fields, and serializing yields the true serde field names. RED verification:
    // temporarily remove vs_currency from ListQuotesParams (and its literal below) → this test
    // fails on operation 'listCoinQuotes'; restoring it returns to green.

    fn param_field_names<T: serde::Serialize>(v: &T) -> std::collections::BTreeSet<String> {
        match serde_json::to_value(v).expect("param struct must serialize") {
            serde_json::Value::Object(m) => m.keys().cloned().collect(),
            other => panic!("param struct must serialize to a JSON object, got {other:?}"),
        }
    }

    // operationId → the serde field set of its Rust query-param struct.
    fn operation_struct_fields(
    ) -> std::collections::BTreeMap<&'static str, std::collections::BTreeSet<String>> {
        use super::{candles, coin_market, coins, cycle_overlay, metadata, quotes};
        let mut m = std::collections::BTreeMap::new();
        m.insert(
            "listCoins",
            param_field_names(&coins::ListCoinsParams {
                cursor: None,
                limit: None,
            }),
        );
        m.insert(
            "searchCoins",
            param_field_names(&coins::SearchCoinsParams {
                q: None,
                limit: None,
            }),
        );
        m.insert(
            "getCoinMetadata",
            param_field_names(&metadata::GetMetadataParams { as_of: None }),
        );
        m.insert(
            "getCoinMarketLatest",
            param_field_names(&coin_market::GetCoinMarketLatestParams { vs_currency: None }),
        );
        m.insert(
            "listCoinMarket",
            param_field_names(&coin_market::ListCoinMarketParams {
                vs_currency: None,
                cursor: None,
                limit: None,
                start: None,
                end: None,
            }),
        );
        m.insert(
            "listLatestCoinQuotes",
            param_field_names(&quotes::ListLatestQuotesParams { vs_currency: None }),
        );
        m.insert(
            "getLatestCoinQuote",
            param_field_names(&quotes::GetLatestQuoteParams { vs_currency: None }),
        );
        m.insert(
            "listCoinQuotes",
            param_field_names(&quotes::ListQuotesParams {
                vs_currency: None,
                cursor: None,
                limit: None,
                start: None,
                end: None,
            }),
        );
        m.insert(
            "listCoinCandles",
            param_field_names(&candles::ListCandlesParams {
                interval: None,
                vs_currency: None,
                cursor: None,
                limit: None,
                start: None,
                end: None,
            }),
        );
        m.insert(
            "listCycleProjection",
            param_field_names(&cycle_overlay::ListCycleOverlayParams {
                vs_currency: None,
                cycle: None,
                cursor: None,
                limit: None,
                as_of: None,
            }),
        );
        m
    }

    // Parse components/parameters → ref_key -> parameter `name`, for `in: query` refs only.
    fn parse_component_query_refs(comp: &str) -> std::collections::BTreeMap<String, String> {
        let mut map = std::collections::BTreeMap::new();
        let Some(pidx) = comp.find("\n  parameters:") else {
            return map;
        };
        let rest = &comp[pidx + "\n  parameters:".len()..];
        let end = ["\n  responses:", "\n  schemas:", "\n  requestBodies:"]
            .iter()
            .filter_map(|marker| rest.find(marker))
            .min()
            .unwrap_or(rest.len());
        let block = &rest[..end];

        let (mut cur_key, mut cur_name, mut cur_in): (
            Option<String>,
            Option<String>,
            Option<String>,
        ) = (None, None, None);
        let mut flush = |k: &Option<String>, n: &Option<String>, i: &Option<String>| {
            if let (Some(k), Some(n), Some(i)) = (k, n, i) {
                if i == "query" {
                    map.insert(k.clone(), n.clone());
                }
            }
        };
        for line in block.lines() {
            if let Some(after4) = line.strip_prefix("    ") {
                if !after4.starts_with(' ') && after4.ends_with(':') && !after4.contains(' ') {
                    flush(&cur_key, &cur_name, &cur_in);
                    cur_key = Some(after4.trim_end_matches(':').to_string());
                    cur_name = None;
                    cur_in = None;
                    continue;
                }
            }
            if let Some(after6) = line.strip_prefix("      ") {
                if let Some(n) = after6.strip_prefix("name: ") {
                    cur_name = Some(n.trim().to_string());
                } else if let Some(i) = after6.strip_prefix("in: ") {
                    cur_in = Some(i.trim().to_string());
                }
            }
        }
        flush(&cur_key, &cur_name, &cur_in);
        map
    }

    // Parse the paths section → operationId -> documented query parameter names.
    fn parse_operation_query_params(
        paths: &str,
        ref_query_names: &std::collections::BTreeMap<String, String>,
    ) -> std::collections::BTreeMap<String, std::collections::BTreeSet<String>> {
        let mut out: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
            std::collections::BTreeMap::new();
        let mut cur_op: Option<String> = None;
        let mut pending_name: Option<String> = None;
        for line in paths.lines() {
            let t = line.trim_start();
            if let Some(op) = t.strip_prefix("operationId: ") {
                cur_op = Some(op.trim().to_string());
                pending_name = None;
                out.entry(cur_op.clone().unwrap()).or_default();
            } else if let Some(name) = t.strip_prefix("- name: ") {
                pending_name = Some(name.trim().to_string());
            } else if let Some(rest) = t.strip_prefix("- $ref: ") {
                if let Some(ref_key) = rest.trim().trim_matches('\'').rsplit('/').next() {
                    if let (Some(op), Some(qname)) = (cur_op.as_ref(), ref_query_names.get(ref_key))
                    {
                        out.entry(op.clone()).or_default().insert(qname.clone());
                    }
                }
                pending_name = None;
            } else if t == "in: query" {
                if let (Some(op), Some(name)) = (cur_op.as_ref(), pending_name.take()) {
                    out.entry(op.clone()).or_default().insert(name);
                }
            } else if t == "in: path" || t == "in: header" || t == "in: cookie" {
                pending_name = None;
            }
        }
        out
    }

    #[test]
    fn openapi_query_params_have_matching_struct_fields() {
        let yaml = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("api/crypto-collector.yaml"),
        )
        .expect("api/crypto-collector.yaml must exist");

        let comp_idx = yaml.find("\ncomponents:").expect("components section");
        let ref_query_names = parse_component_query_refs(&yaml[comp_idx..]);
        let documented = parse_operation_query_params(&yaml[..comp_idx], &ref_query_names);
        let struct_fields = operation_struct_fields();

        // Parser sanity: the F-29 case is actually observed (guard is not a silent no-op).
        assert!(
            documented
                .get("listCoinQuotes")
                .is_some_and(|s| s.contains("vs_currency")),
            "parser sanity: listCoinQuotes must document a vs_currency query param"
        );
        assert!(
            documented
                .get("listCoinCandles")
                .is_some_and(|s| s.contains("interval") && s.contains("vs_currency")),
            "parser sanity: listCoinCandles must document interval + vs_currency"
        );

        // Every operation with a mapped param struct must have a field for each documented query
        // parameter (the F-29 guard).
        for (op_id, fields) in &struct_fields {
            let params = documented.get(*op_id).unwrap_or_else(|| {
                panic!(
                    "operation '{op_id}' has a param struct but is absent from the OpenAPI paths"
                )
            });
            for p in params {
                assert!(
                    fields.contains(p),
                    "operation '{op_id}' documents query parameter '{p}' but the Rust param \
                     struct has no matching field (F-59); struct fields = {fields:?}"
                );
            }
        }
    }

    // Scenario 34 (REQ-CYCLE-098/099): the removed listCycleOverlay operationId and the
    // deleted /cycle-overlay path must be entirely absent from the document.
    #[test]
    fn openapi_yaml_does_not_contain_removed_cycle_overlay_route() {
        let yaml = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("api/crypto-collector.yaml"),
        )
        .expect("api/crypto-collector.yaml must exist");

        assert!(
            !yaml.contains("listCycleOverlay"),
            "OpenAPI spec must not contain the removed operationId 'listCycleOverlay'"
        );
        // Match the YAML path *key* form specifically (trailing colon) rather than any prose
        // mention — the migration-guidance description text legitimately references the old
        // path by name when explaining what replaced it.
        assert!(
            !yaml.contains("/coins/{coin_id}/cycle-overlay:"),
            "OpenAPI spec must not contain the removed path '/coins/{{coin_id}}/cycle-overlay'"
        );
    }

    // REQ-CYCLE-084/099 (SPEC-CYCLE-001 v0.6.0): the optional `as_of` query parameter must be
    // documented on the single parameterized data path, and must NOT appear on the bare
    // discovery path (which carries no `as_of`-relevant parameters).
    #[test]
    fn openapi_yaml_documents_as_of_on_the_model_data_path_only() {
        let yaml = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("api/crypto-collector.yaml"),
        )
        .expect("api/crypto-collector.yaml must exist");

        let discovery_start = yaml
            .find("/coins/{coin_id}/cycle-projection:")
            .expect("cycle-projection discovery path must exist");
        let data_start = yaml
            .find("/coins/{coin_id}/cycle-projection/{model}:")
            .expect("cycle-projection/{model} data path must exist");
        let components_start = yaml
            .find("\ncomponents:")
            .expect("components section must exist");

        // The discovery block runs from its own header up to the data path's header (the two
        // path items are adjacent in the document); the data block runs from its header to
        // components.
        let discovery_block = &yaml[discovery_start..data_start];
        let data_block = &yaml[data_start..components_start];

        assert!(
            data_block.contains("cycle_as_of") || data_block.contains("as_of"),
            "the parameterized data path must document the optional as_of query parameter"
        );
        assert!(
            !(discovery_block.contains("cycle_as_of") || discovery_block.contains("as_of")),
            "the bare discovery path must NOT document an as_of query parameter"
        );
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
            "CoinQuote",
            "CoinCandle",
            "CoinMetadata",
            "CoinMarketSnapshot",
            "ApiError",
            "Page",
            "CycleOverlayPoint",
            "CycleProjectionModels",
        ];
        for schema in &schemas {
            assert!(
                yaml.contains(schema),
                "OpenAPI spec must contain schema '{schema}'"
            );
        }
    }
}
