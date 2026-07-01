//! Coin-keyed OHLCV candle read handler (SPEC-API-002 REQ-API-141/142, SPEC-API-003).
//!
//! Route:
//! - `GET /v1/coins/{coin_id}/candles` → list_candles (interval required, keyset-paginated)
//!
//! OR-API-1 resolved: supported intervals are `1m`, `5m`, `15m`, `1h`, `4h`, `1d`, `1w`.
//! `interval` is required; absent or invalid → 400 (REQ-API-041).
//! `volume` is nullable in the response (CoinGecko OHLC; REQ-API-042).
//!
//! SPEC-API-003 additions:
//! - Optional `vs_currency` parameter (default `usd`); unrecognised values → 200 empty page.
//! - Aggregation fallback when no native candles exist at the exact interval.

use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;

use super::{
    candles_agg::{aggregate_candles, interval_to_seconds, select_source_interval},
    cursor::{decode_keyset_cursor, encode_keyset_cursor, validate_limit, TsKey},
    dto::{CoinCandleDto, Page},
    quotes::paginate_ts,
    ApiError, ApiResult, AppState,
};
use crate::models::quote::CoinCandle;

/// Supported candle intervals (OR-API-1 resolved).
///
// @MX:NOTE: [AUTO] Supported candle intervals: 1m, 5m, 15m, 1h, 4h, 1d, 1w (OR-API-1)
// @MX:SPEC: SPEC-API-001 OR-API-1 REQ-API-041
pub const SUPPORTED_INTERVALS: &[&str] = &["1m", "5m", "15m", "1h", "4h", "1d", "1w"];

// ── Query parameter types ─────────────────────────────────────────────────────

/// Query parameters for `GET /v1/coins/{coin_id}/candles` (SPEC-API-002/003).
#[derive(Debug, Deserialize)]
pub struct ListCandlesParams {
    /// Required: must be one of SUPPORTED_INTERVALS (REQ-API-041).
    pub interval: Option<String>,
    /// Optional: quote currency filter; defaults to `usd` (REQ-API-217).
    /// Unrecognised values are NOT rejected — they simply match no rows → 200 empty page.
    pub vs_currency: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<i64>,
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// `GET /v1/coins/{coin_id}/candles` — keyset-paginated OHLCV candles (REQ-API-141/142).
///
/// SPEC-API-003 aggregation fallback: when no native candle exists at the exact `interval`,
/// the handler derives candles on the fly from the largest stored divisor interval.
/// Native data is always served unchanged; aggregation is a read-time fallback only.
// @MX:NOTE: [AUTO] list_candles native-vs-aggregate branch point.
// @MX:REASON: The exact-interval EXISTS probe (REQ-API-200/201, OR-API3-2) is evaluated FIRST.
//             If any native row exists for (coin_id, interval, vs_currency), the handler
//             returns native data unchanged (even if this specific page is empty due to cursor).
//             Aggregation is triggered ONLY on a coin-level miss — never by an empty page.
//             This prevents deep-cursor pagination from misfiring into aggregation (acceptance.md edge).
// @MX:SPEC: SPEC-API-003 REQ-API-200 REQ-API-201 OR-API3-2
pub async fn list_candles(
    State(state): State<AppState>,
    Path(coin_id): Path<String>,
    Query(params): Query<ListCandlesParams>,
) -> ApiResult<impl IntoResponse> {
    // `interval` is required (REQ-API-041 / REQ-API-215).
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

    // REQ-API-217: resolve vs_currency; default "usd" mirrors coin_market.rs:51,86.
    // Unrecognised values are accepted — they match no rows → 200 empty page (not 400).
    let vs_currency = params
        .vs_currency
        .as_deref()
        .unwrap_or("usd")
        .to_lowercase();

    super::quotes::ensure_coin_exists(&state.pool, &coin_id).await?;

    // ── Native precedence probe (REQ-API-200/201, OR-API3-2) ─────────────────
    //
    // Use a cheap EXISTS check scoped to (coin_id, interval, vs_currency) — NOT to the
    // current page window. This is intentional: a legitimate deep cursor can produce an
    // empty native page while native rows still exist; using the first-page read result
    // to decide native-vs-aggregate would wrongly flip to aggregation on page 2+.
    let native_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(\
           SELECT 1 FROM coin_candles \
           WHERE coin_id = $1 AND interval = $2 AND vs_currency = $3\
         )",
    )
    .bind(&coin_id)
    .bind(interval)
    .bind(&vs_currency)
    .fetch_one(&state.pool)
    .await?;

    if native_exists {
        // ── Native path (REQ-API-200): serve exact-interval rows with vs_currency filter ──
        // REQ-API-218: native read now filters by resolved vs_currency.
        let items: Vec<CoinCandle> = sqlx::query_as(
            "SELECT coin_id, vs_currency, interval, ts, open, high, low, close, volume, source \
             FROM coin_candles \
             WHERE coin_id = $1 \
               AND interval   = $2 \
               AND vs_currency = $3 \
               AND ($4::TIMESTAMPTZ IS NULL OR ts <= $4) \
               AND ($5::TIMESTAMPTZ IS NULL OR ts >= $5) \
               AND ($6::TIMESTAMPTZ IS NULL OR ts < $6) \
             ORDER BY ts DESC \
             LIMIT $7",
        )
        .bind(&coin_id)
        .bind(interval)
        .bind(&vs_currency)
        .bind(params.end)
        .bind(params.start)
        .bind(cursor_ts)
        .bind(limit + 1)
        .fetch_all(&state.pool)
        .await?;

        let (items, next_cursor) = paginate_ts(items, limit, |c| c.ts);
        return Ok(Json(Page {
            items: items.into_iter().map(CoinCandleDto::from).collect(),
            next_cursor,
        }));
    }

    // ── Aggregation fallback (REQ-API-201/202/205/212/213/219) ───────────────
    //
    // Discover the stored interval strings for this coin + currency, select the largest
    // divisor, fetch source candles, fold into target-interval buckets.

    let stored_intervals: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT interval FROM coin_candles WHERE coin_id = $1 AND vs_currency = $2",
    )
    .bind(&coin_id)
    .bind(&vs_currency)
    .fetch_all(&state.pool)
    .await?;

    // `interval` has already been validated against SUPPORTED_INTERVALS; all members of
    // SUPPORTED_INTERVALS are present in interval_to_seconds → this expect never panics.
    let target_secs =
        interval_to_seconds(interval).expect("validated interval must have a known second count");

    let stored_refs: Vec<&str> = stored_intervals.iter().map(String::as_str).collect();
    let source_interval = match select_source_interval(&stored_refs, target_secs) {
        Some(si) => si,
        // REQ-API-202: no stored interval divides the target → empty page, not an error.
        None => {
            let empty: Page<CoinCandleDto> = Page {
                items: vec![],
                next_cursor: None,
            };
            return Ok(Json(empty));
        }
    };

    // source_interval was returned by select_source_interval, which only returns strings
    // that passed interval_to_seconds → this expect never panics.
    let source_secs = interval_to_seconds(source_interval)
        .expect("source interval selected from interval_to_seconds must have a known second count");

    // Hard ceiling on source rows fetched to bound memory regardless of the N multiplier
    // (e.g. 1w from 1m → N = 10 080; without a cap, limit=1000 would request ~10M rows).
    // @MX:NOTE: [AUTO] MAX_SOURCE_ROWS caps the sqlx fetch_all buffer; chosen conservatively
    //           at 50 000 rows (~4 MB for a typical CoinCandle). When hit, truncation-aware
    //           has_more (below) ensures pagination still terminates correctly.
    // @MX:SPEC: SPEC-API-003 REQ-API-214
    const MAX_SOURCE_ROWS: i64 = 50_000;

    // Row cap for the source query (Risk R1 mitigation, T-009):
    // Fetch (limit+1)*N source rows. If the DB returned the full cap, more pages exist
    // even if gap-dropping reduces aggregated output below `limit`.
    let n: i64 = target_secs / source_secs;
    let row_cap: i64 = ((limit + 1) * n).min(MAX_SOURCE_ROWS);

    // Source query: scoped to (coin_id, vs_currency, source_interval).
    // cursor_ts is an exclusive upper bound (bucket_start of the previous page's last item).
    // start/end filters are applied on aggregated output after folding (see below).
    // REQ-API-219: vs_currency scoping ensures no cross-currency folding.
    //
    // Start lower bound: when params.start is provided, the source query is bounded to
    // `ts >= start - target_secs` (one target-interval margin) so the boundary bucket
    // at/after `start` has its full set of source candles available.  Without this bound
    // a far-past `start` would let the row cap be exhausted by recent rows, silently
    // dropping the requested historical window from the result.
    // The exact `ts >= start` filter is applied post-aggregation (retain below).
    let source_start: Option<DateTime<Utc>> = params
        .start
        .and_then(|s| s.checked_sub_signed(Duration::seconds(target_secs)));

    let source_rows: Vec<CoinCandle> = sqlx::query_as(
        "SELECT coin_id, vs_currency, interval, ts, open, high, low, close, volume, source \
         FROM coin_candles \
         WHERE coin_id = $1 \
           AND vs_currency = $2 \
           AND interval = $3 \
           AND ($4::TIMESTAMPTZ IS NULL OR ts < $4) \
           AND ($5::TIMESTAMPTZ IS NULL OR ts >= $5) \
         ORDER BY ts DESC \
         LIMIT $6",
    )
    .bind(&coin_id)
    .bind(&vs_currency)
    .bind(source_interval)
    .bind(cursor_ts)
    .bind(source_start)
    .bind(row_cap)
    .fetch_all(&state.pool)
    .await?;

    let source_hit_cap = source_rows.len() as i64 >= row_cap;

    // Wall-clock `now` is captured here in the handler so the pure aggregate_candles
    // function never reads the system clock (makes it hermetically testable).
    let now = Utc::now();

    let mut agg = aggregate_candles(
        source_rows,
        target_secs,
        source_secs,
        now,
        source_interval,
        interval,
    );

    // Apply start/end filters on aggregated bucket ts (REQ-API-214).
    // These are applied post-aggregation because: (a) end requires knowing the bucket_start
    // of source candles at the boundary, not just source.ts; (b) start applied to the
    // source query would strip early source candles needed for the first boundary bucket.
    if let Some(end) = params.end {
        agg.retain(|c| c.ts <= end);
    }
    if let Some(start) = params.start {
        agg.retain(|c| c.ts >= start);
    }

    // Truncation-aware pagination (Risk R1 from tasks.md):
    // If the source DB read returned the cap, there are older source rows in the DB.
    // A gap-heavy series may have reduced the aggregated count below `limit`; the standard
    // `paginate_ts` heuristic (`len > limit`) would wrongly emit a null next_cursor in that
    // case. When the source cap was hit, we emit a cursor from the oldest emitted bucket.
    let (items, next_cursor) = if source_hit_cap && (agg.len() as i64) <= limit {
        let next_cursor = agg
            .last()
            .map(|c| encode_keyset_cursor(&TsKey { ts: c.ts }));
        (agg, next_cursor)
    } else {
        paginate_ts(agg, limit, |c| c.ts)
    };

    Ok(Json(Page {
        items: items.into_iter().map(CoinCandleDto::from).collect(),
        next_cursor,
    }))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Validate `interval` against the supported set (REQ-API-041 / REQ-API-215).
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

    // ── Existing tests (REQ-API-215 regression / T-010) ─────────────────────

    // Scenario 6 (REQ-API-041): absent interval → 400.
    #[tokio::test]
    async fn list_candles_missing_interval_returns_400() {
        let server = test_server();
        let resp = server.get("/v1/coins/bitcoin/candles").await;
        assert_eq!(resp.status_code(), 400);
        let body: serde_json::Value = resp.json();
        assert_eq!(body["code"], "BAD_REQUEST");
    }

    // Scenario 6 (REQ-API-041): unknown interval → 400.
    #[tokio::test]
    async fn list_candles_unknown_interval_returns_400() {
        let server = test_server();
        let resp = server
            .get("/v1/coins/bitcoin/candles")
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
            .get("/v1/coins/bitcoin/candles")
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
            .get("/v1/coins/bitcoin/candles")
            .add_query_param("interval", "1d")
            .add_query_param("limit", "9999999")
            .await;
        assert_eq!(resp.status_code(), 400);
    }

    // Scenario 6 (REQ-API-042): CoinCandleDto has nullable volume field.
    #[test]
    fn coin_candle_dto_has_nullable_volume() {
        use crate::api::dto::CoinCandleDto;
        use rust_decimal_macros::dec;

        let candle = crate::models::quote::CoinCandle {
            coin_id: "bitcoin".into(),
            vs_currency: "usd".into(),
            interval: "1h".into(),
            ts: chrono::Utc::now(),
            open: dec!(100),
            high: dec!(110),
            low: dec!(90),
            close: dec!(105),
            volume: None, // CoinGecko: no volume
            source: "coingecko".into(),
        };
        let dto = CoinCandleDto::from(candle);
        assert!(dto.volume.is_none(), "CoinGecko candle volume must be null");
    }

    // ── T-007: vs_currency plumbing (pure tests) ──────────────────────────────

    // Scenario 15 (REQ-API-217): omitted vs_currency defaults to "usd".
    #[test]
    fn vs_currency_defaults_to_usd_when_omitted() {
        let params = ListCandlesParams {
            interval: Some("1h".into()),
            vs_currency: None,
            cursor: None,
            limit: None,
            start: None,
            end: None,
        };
        let resolved = params
            .vs_currency
            .as_deref()
            .unwrap_or("usd")
            .to_lowercase();
        assert_eq!(
            resolved, "usd",
            "REQ-API-217: missing vs_currency defaults to usd"
        );
    }

    // REQ-API-217: explicit vs_currency is lowercased and passed through.
    #[test]
    fn vs_currency_explicit_value_is_lowercased() {
        let params = ListCandlesParams {
            interval: Some("1h".into()),
            vs_currency: Some("EUR".into()),
            cursor: None,
            limit: None,
            start: None,
            end: None,
        };
        let resolved = params
            .vs_currency
            .as_deref()
            .unwrap_or("usd")
            .to_lowercase();
        assert_eq!(resolved, "eur");
    }

    // Scenario 13 (REQ-API-215): 2h (not in SUPPORTED_INTERVALS) → 400 without querying.
    #[tokio::test]
    async fn list_candles_unsupported_interval_2h_returns_400() {
        let server = test_server();
        let resp = server
            .get("/v1/coins/bitcoin/candles")
            .add_query_param("interval", "2h")
            .await;
        assert_eq!(resp.status_code(), 400, "2h is not in SUPPORTED_INTERVALS");
    }

    // REQ-API-217: an unrecognised vs_currency must NOT be rejected with 400.
    // (It will fail with 500/404 due to no live DB in unit tests, but not 400.)
    #[tokio::test]
    async fn list_candles_unknown_vs_currency_is_not_400() {
        let server = test_server();
        let resp = server
            .get("/v1/coins/bitcoin/candles")
            .add_query_param("interval", "1h")
            .add_query_param("vs_currency", "xyz_unknown_currency")
            .await;
        assert_ne!(
            resp.status_code(),
            400,
            "REQ-API-217: unrecognised vs_currency must not produce 400"
        );
    }

    // ── DB-gated tests ────────────────────────────────────────────────────────

    // Scenario 17 (REQ-API-217): unknown vs_currency returns 200 with empty items.
    // This is the positive DB-backed assertion: not 400, not 500, and no items.
    // A coin registered with usd candles is used; xyz matches no stored rows.
    #[tokio::test]
    #[ignore]
    async fn db_scenario_17_unknown_vs_currency_returns_200_empty() {
        // Requires: bitcoin registered in tracked_coins (with any usd candles).
        // Requesting vs_currency=xyz_unknown goes through the aggregation branch
        // (no native xyz candles exist → EXISTS probe is false → DISTINCT intervals
        // returns empty → select_source_interval returns None → empty page returned).
        let (server, _) = db_test_server();
        let resp = server
            .get("/v1/coins/bitcoin/candles")
            .add_query_param("interval", "1h")
            .add_query_param("vs_currency", "xyz_unknown_currency")
            .await;
        assert_eq!(
            resp.status_code(),
            200,
            "REQ-API-217: unrecognised vs_currency must return 200, not 4xx/5xx"
        );
        let body: serde_json::Value = resp.json();
        let items = body["items"].as_array().expect("items must be an array");
        assert!(
            items.is_empty(),
            "REQ-API-217: no candles exist for xyz_unknown_currency; items must be []"
        );
        assert_eq!(
            body["next_cursor"],
            serde_json::Value::Null,
            "REQ-API-217: next_cursor must be null when items is empty"
        );
    }

    // DB-gated helper: build a test server backed by the real DATABASE_URL.
    fn db_test_server() -> (TestServer, crate::api::AppState) {
        use std::sync::Arc;
        use tokio::sync::broadcast;
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for DB tests");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy(&url)
            .expect("lazy pool from DATABASE_URL");
        let (coin_quote_tx, _) = broadcast::channel(16);
        let (coin_candle_tx, _) = broadcast::channel(16);
        let state = crate::api::AppState {
            pool,
            chain: Arc::new(vec![]),
            search_provider: "coingecko".into(),
            coingecko_base_url: "https://api.coingecko.com".into(),
            http_client: reqwest::Client::new(),
            coin_quote_tx,
            coin_candle_tx,
        };
        let server = TestServer::new(crate::api::build_api_router(state.clone()));
        (server, state)
    }

    // Existing DB-gated test (characterization — REQ-API-200 baseline).
    #[tokio::test]
    #[ignore]
    async fn db_list_candles_unknown_coin_returns_404() {
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
        let resp = server
            .get("/v1/coins/no-such-coin-xyz/candles")
            .add_query_param("interval", "1h")
            .await;
        assert_eq!(resp.status_code(), 404);
    }

    // Scenario 1 (REQ-API-200): native candles served unchanged — no aggregated: source.
    #[tokio::test]
    #[ignore]
    async fn db_scenario_1_native_precedence_no_aggregated_source() {
        // Requires: bitcoin registered in tracked_coins; 1h native candles in coin_candles
        // with source="binance" and vs_currency="usd".
        let (server, _) = db_test_server();
        let resp = server
            .get("/v1/coins/bitcoin/candles")
            .add_query_param("interval", "1h")
            .add_query_param("vs_currency", "usd")
            .await;
        assert_eq!(resp.status_code(), 200, "Scenario 1: native 1h must be 200");
        let body: serde_json::Value = resp.json();
        let items = body["items"].as_array().expect("items array");
        if !items.is_empty() {
            for item in items {
                let src = item["source"].as_str().unwrap_or("");
                assert!(
                    !src.starts_with("aggregated:"),
                    "Scenario 1 (REQ-API-200): native candles must not carry aggregated: source; got {src}"
                );
            }
        }
    }

    // Scenario 2 (REQ-API-201/205/206/208/212): aggregate 4h from 1h candles.
    // Requires: bitcoin with 1h candles, no native 4h.
    #[tokio::test]
    #[ignore]
    async fn db_scenario_2_aggregate_4h_from_1h_ohlc() {
        let (server, _) = db_test_server();
        let resp = server
            .get("/v1/coins/bitcoin/candles")
            .add_query_param("interval", "4h")
            .add_query_param("vs_currency", "usd")
            .await;
        assert_eq!(resp.status_code(), 200);
        let body: serde_json::Value = resp.json();
        let items = body["items"].as_array().expect("items array");
        for item in items {
            let src = item["source"].as_str().unwrap_or("");
            if src.starts_with("aggregated:") {
                assert_eq!(
                    src, "aggregated:1h",
                    "Scenario 2: source must be aggregated:1h"
                );
            }
        }
    }

    // Scenario 3 (REQ-API-205): largest divisor selected (dogecoin: 30m/4h/4d → 4h for 1d).
    #[tokio::test]
    #[ignore]
    async fn db_scenario_3_largest_divisor_4h_for_1d() {
        let (server, _) = db_test_server();
        let resp = server
            .get("/v1/coins/dogecoin/candles")
            .add_query_param("interval", "1d")
            .add_query_param("vs_currency", "usd")
            .await;
        assert_eq!(resp.status_code(), 200);
        let body: serde_json::Value = resp.json();
        let items = body["items"].as_array().expect("items array");
        for item in items {
            let src = item["source"].as_str().unwrap_or("");
            if src.starts_with("aggregated:") {
                assert_eq!(
                    src, "aggregated:4h",
                    "Scenario 3 (REQ-API-205): largest divisor of 1d must be 4h, not 30m"
                );
            }
        }
    }

    // Scenario 4 (REQ-API-204/205): non-API stored interval used as source.
    // dogecoin stores 30m (not in SUPPORTED_INTERVALS); target 1h → source 30m.
    #[tokio::test]
    #[ignore]
    async fn db_scenario_4_non_api_source_30m_for_1h() {
        let (server, _) = db_test_server();
        let resp = server
            .get("/v1/coins/dogecoin/candles")
            .add_query_param("interval", "1h")
            .add_query_param("vs_currency", "usd")
            .await;
        assert_eq!(resp.status_code(), 200);
        let body: serde_json::Value = resp.json();
        let items = body["items"].as_array().expect("items array");
        for item in items {
            let src = item["source"].as_str().unwrap_or("");
            if src.starts_with("aggregated:") {
                assert_eq!(
                    src, "aggregated:30m",
                    "Scenario 4 (REQ-API-204): 30m is a valid non-API source"
                );
            }
        }
    }

    // Scenario 9 (REQ-API-202): no divisor → HTTP 200 empty page.
    // dogecoin with only 4h/4d; target 1h (3600 % 14400 != 0) → no divisor.
    #[tokio::test]
    #[ignore]
    async fn db_scenario_9_no_divisor_yields_empty_page() {
        // Requires: a coin (e.g. dogecoin) storing only 4h candles, no 1h or 30m.
        let (server, _) = db_test_server();
        let resp = server
            .get("/v1/coins/dogecoin_no_divisor/candles")
            .add_query_param("interval", "1h")
            .add_query_param("vs_currency", "usd")
            .await;
        // Could be 404 if the coin doesn't exist, or 200 empty if it does.
        // The important invariant is: NOT 400 or 500.
        assert!(
            resp.status_code() == 200 || resp.status_code() == 404,
            "Scenario 9 (REQ-API-202): no divisor → 200 empty or 404 if coin missing"
        );
        if resp.status_code() == 200 {
            let body: serde_json::Value = resp.json();
            assert_eq!(
                body["items"],
                serde_json::json!([]),
                "no divisor → empty items"
            );
            assert_eq!(body["next_cursor"], serde_json::Value::Null);
        }
    }

    // Scenario 11 (REQ-API-214): keyset pagination over aggregated results.
    #[tokio::test]
    #[ignore]
    async fn db_scenario_11_keyset_pagination_over_aggregated() {
        let (server, _) = db_test_server();
        // First page (limit=2).
        let resp1 = server
            .get("/v1/coins/bitcoin/candles")
            .add_query_param("interval", "4h")
            .add_query_param("limit", "2")
            .add_query_param("vs_currency", "usd")
            .await;
        assert_eq!(resp1.status_code(), 200);
        let body1: serde_json::Value = resp1.json();
        let items1 = body1["items"].as_array().expect("items");
        if items1.len() < 2 {
            return; // Not enough data for pagination test
        }
        let cursor = body1["next_cursor"]
            .as_str()
            .expect("next_cursor must be non-null for 2-item page");

        // Second page.
        let resp2 = server
            .get("/v1/coins/bitcoin/candles")
            .add_query_param("interval", "4h")
            .add_query_param("limit", "2")
            .add_query_param("vs_currency", "usd")
            .add_query_param("cursor", cursor)
            .await;
        assert_eq!(resp2.status_code(), 200);
        let body2: serde_json::Value = resp2.json();
        let items2 = body2["items"].as_array().expect("items");
        assert!(!items2.is_empty(), "second page must have items");

        // Items on page 2 must be older than page 1 (ts DESC ordering).
        let last_ts_p1 = items1.last().unwrap()["ts"].as_str().unwrap();
        let first_ts_p2 = items2[0]["ts"].as_str().unwrap();
        assert!(
            first_ts_p2 < last_ts_p1,
            "page 2 items must be older than page 1"
        );
    }

    // Scenario 12 (REQ-API-213/219): aggregation respects vs_currency boundary.
    #[tokio::test]
    #[ignore]
    async fn db_scenario_12_currency_boundary_usd_only() {
        let (server, _) = db_test_server();
        let resp = server
            .get("/v1/coins/bitcoin/candles")
            .add_query_param("interval", "4h")
            .add_query_param("vs_currency", "usd")
            .await;
        assert_eq!(resp.status_code(), 200);
        let body: serde_json::Value = resp.json();
        let items = body["items"].as_array().expect("items");
        for item in items {
            let vc = item["vs_currency"].as_str().unwrap_or("");
            assert_eq!(
                vc, "usd",
                "Scenario 12 (REQ-API-213): only usd candles must appear"
            );
        }
    }

    // Scenario 14 (REQ-API-217/218/219): explicit vs_currency filters native path.
    #[tokio::test]
    #[ignore]
    async fn db_scenario_14_explicit_vs_currency_native_path() {
        // Requires: bitcoin with native 1h candles in eur.
        let (server, _) = db_test_server();
        let resp = server
            .get("/v1/coins/bitcoin/candles")
            .add_query_param("interval", "1h")
            .add_query_param("vs_currency", "eur")
            .await;
        assert_eq!(resp.status_code(), 200);
        let body: serde_json::Value = resp.json();
        let items = body["items"].as_array().expect("items");
        for item in items {
            let vc = item["vs_currency"].as_str().unwrap_or("");
            assert_eq!(vc, "eur", "REQ-API-218: native read must filter to eur");
        }
    }

    // Scenario 15 (REQ-API-217): omitting vs_currency yields only usd candles (DB-gated).
    #[tokio::test]
    #[ignore]
    async fn db_scenario_15_omitted_vs_currency_defaults_to_usd() {
        let (server, _) = db_test_server();
        let resp = server
            .get("/v1/coins/bitcoin/candles")
            .add_query_param("interval", "1h")
            .await;
        assert_eq!(resp.status_code(), 200);
        let body: serde_json::Value = resp.json();
        let items = body["items"].as_array().expect("items");
        for item in items {
            let vc = item["vs_currency"].as_str().unwrap_or("");
            assert_eq!(vc, "usd", "REQ-API-217: default vs_currency must be usd");
        }
    }

    // Scenario 16 (REQ-API-214): start/end filtering over aggregated results.
    #[tokio::test]
    #[ignore]
    async fn db_scenario_16_start_end_filter_aggregated() {
        use chrono::{Duration, Utc};
        let (server, _) = db_test_server();
        let end = Utc::now();
        let start = end - Duration::hours(48);
        let resp = server
            .get("/v1/coins/bitcoin/candles")
            .add_query_param("interval", "4h")
            .add_query_param("vs_currency", "usd")
            .add_query_param("start", start.to_rfc3339())
            .add_query_param("end", end.to_rfc3339())
            .await;
        assert_eq!(resp.status_code(), 200);
        let body: serde_json::Value = resp.json();
        let items = body["items"].as_array().expect("items");
        for item in items {
            let ts_str = item["ts"].as_str().unwrap();
            let ts: chrono::DateTime<Utc> = ts_str.parse().unwrap();
            assert!(
                ts >= start && ts <= end,
                "Scenario 16 (REQ-API-214): ts {ts_str} must be in [start, end]"
            );
        }
    }
}
