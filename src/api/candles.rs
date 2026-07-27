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

use axum::{extract::State, response::IntoResponse, Json};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::{
    candles_agg::{aggregate_candles, bucket_start, select_source_interval, IntervalCoverage},
    cursor::{decode_keyset_cursor, encode_keyset_cursor, paginate, validate_limit, TsKey},
    dto::{CoinCandleDto, Page},
    ensure_coin_exists,
    extract::{ApiPath, ApiQuery},
    ApiError, ApiResult, AppState,
};
use crate::models::{quote::CoinCandle, ApiInterval};

// The former supported-intervals allow-list (OR-API-1) folded into `ApiInterval::is_api_facing()`
// (SPEC-REFACTOR-001 M6, F-54): the API-facing subset {1m, 5m, 15m, 1h, 4h, 1d, 1w} is now
// `ApiInterval::API_FACING`, and the public boundary validates via `validate_interval` below.

// ── Query parameter types ─────────────────────────────────────────────────────

/// Query parameters for `GET /v1/coins/{coin_id}/candles` (SPEC-API-002/003).
///
/// `Serialize` is derived for the F-59 parameter-parity test (src/api/mod.rs).
#[derive(Debug, Deserialize, Serialize)]
pub struct ListCandlesParams {
    /// Required: must be one of the API-facing intervals (`ApiInterval::is_api_facing`, REQ-API-041).
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
    ApiPath(coin_id): ApiPath<String>,
    ApiQuery(params): ApiQuery<ListCandlesParams>,
) -> ApiResult<impl IntoResponse> {
    // `interval` is required (REQ-API-041 / REQ-API-215).
    let interval = params
        .interval
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("'interval' query parameter is required".into()))?;
    // Validate against the API-facing subset; a storage-only interval (e.g. `3m`) or any
    // non-fixed string returns 400 without querying (REQ-API-041/215; behavior-preserving vs
    // the removed supported-intervals allow-list, SPEC-REFACTOR-001 REQ-REFACTOR-062).
    let requested_interval = validate_interval(interval)?;

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

    ensure_coin_exists(&state.pool, &coin_id).await?;

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

        let (items, next_cursor) = paginate(items, limit, |c| TsKey { ts: c.ts });
        return Ok(Json(Page {
            items: items.into_iter().map(CoinCandleDto::from).collect(),
            next_cursor,
        }));
    }

    // ── Aggregation fallback (REQ-API-201/202/205/212/213/219) ───────────────
    //
    // Discover the stored interval strings for this coin + currency, select the largest
    // divisor, fetch source candles, fold into target-interval buckets.

    // Per-interval coverage span (earliest/latest ts), so source selection can weigh history
    // depth and staleness — not just bucket divisibility. Two stored intervals may both divide
    // the target while spanning wildly different date ranges (e.g. a 9-year 5m backfill vs a
    // 1-month 4h series); coverage-aware selection avoids silently serving the shallow one.
    let coverage_rows = crate::db::interval_coverage(&state.pool, &coin_id, &vs_currency).await?;

    // `interval` was validated into `requested_interval` above; `ApiInterval::secs()` is total
    // (no Option, no .expect — the old interval_to_seconds `.expect` panic path is gone,
    // SPEC-REFACTOR-001 REQ-REFACTOR-061).
    let target_secs = requested_interval.secs();

    // Wall-clock `now` is captured before source selection (staleness weighting) and reused
    // for aggregation bucket classification, so the pure logic never reads the system clock.
    let now = Utc::now();

    let coverage: Vec<IntervalCoverage> = coverage_rows
        .iter()
        .map(|(iv, earliest, latest)| IntervalCoverage {
            interval: iv.as_str(),
            earliest: *earliest,
            latest: *latest,
        })
        .collect();

    let source_interval = match select_source_interval(&coverage, target_secs, params.start, now) {
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

    // source_interval was returned by select_source_interval, which only returns strings that
    // parse as ApiInterval → this parse never fails in practice.
    let source_secs = source_interval
        .parse::<ApiInterval>()
        .map(|i| i.secs())
        .expect(
            "source interval selected from ApiInterval vocabulary must have a known second count",
        );

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

    // End upper bound (F-31/REQ-API-405): when `end` is present, bound the source read to
    // `ts < end + one target-interval bucket` (one-bucket upper margin, symmetric with the
    // `source_start` lower margin) so the boundary bucket at/before `end` has its full source
    // set. Without this, a far-past `[start, end]` window fetches only the newest rows (ORDER BY
    // ts DESC LIMIT cap) and post-aggregation retain drops them all → an empty page
    // indistinguishable from "no data". With it, the far-past window is reachable.
    let source_end: Option<DateTime<Utc>> = params
        .end
        .and_then(|e| e.checked_add_signed(Duration::seconds(target_secs)));

    let source_rows: Vec<CoinCandle> = sqlx::query_as(
        "SELECT coin_id, vs_currency, interval, ts, open, high, low, close, volume, source \
         FROM coin_candles \
         WHERE coin_id = $1 \
           AND vs_currency = $2 \
           AND interval = $3 \
           AND ($4::TIMESTAMPTZ IS NULL OR ts < $4) \
           AND ($5::TIMESTAMPTZ IS NULL OR ts >= $5) \
           AND ($6::TIMESTAMPTZ IS NULL OR ts < $6) \
         ORDER BY ts DESC \
         LIMIT $7",
    )
    .bind(&coin_id)
    .bind(&vs_currency)
    .bind(source_interval)
    .bind(cursor_ts)
    .bind(source_start)
    .bind(source_end)
    .bind(row_cap)
    .fetch_all(&state.pool)
    .await?;

    let source_hit_cap = source_rows.len() as i64 >= row_cap;

    // Oldest fetched source row (ORDER BY ts DESC → last). Captured before the move into
    // aggregate_candles so the cap-hit-but-empty branch can derive a continuation cursor from
    // its bucket start (F-31/REQ-API-406) when aggregation emits no bucket.
    let oldest_source_ts = source_rows.last().map(|c| c.ts);

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
        // The source read returned the full cap → older source rows remain in the DB. Continue
        // pagination even when this page's buckets were gap-dropped: derive the cursor from the
        // oldest emitted bucket, or — when aggregation emitted nothing (agg empty, so agg.last()
        // is None) — from the oldest fetched source row's bucket start (F-31/REQ-API-406). Both
        // are strictly older than any prior cursor, so pagination makes forward progress.
        let next_cursor = agg
            .last()
            .map(|c| c.ts)
            .or_else(|| oldest_source_ts.map(|ts| bucket_start(ts, target_secs)))
            .map(|ts| encode_keyset_cursor(&TsKey { ts }));
        (agg, next_cursor)
    } else {
        paginate(agg, limit, |c| TsKey { ts: c.ts })
    };

    Ok(Json(Page {
        items: items.into_iter().map(CoinCandleDto::from).collect(),
        next_cursor,
    }))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Validate `interval` against the API-facing set and return the parsed [`ApiInterval`]
/// (REQ-API-041 / REQ-API-215; SPEC-REFACTOR-001 REQ-REFACTOR-062).
///
/// Only the API-facing subset `{1m, 5m, 15m, 1h, 4h, 1d, 1w}` is admitted. A storage-only
/// interval (e.g. `3m`) or any non-fixed / unknown string returns 400 — identical to the prior
/// supported-intervals `.contains` allow-list, now expressed as `ApiInterval::from_api_str`.
pub fn validate_interval(interval: &str) -> ApiResult<ApiInterval> {
    ApiInterval::from_api_str(interval).ok_or_else(|| {
        let allowed: Vec<&str> = ApiInterval::API_FACING.iter().map(|i| i.as_str()).collect();
        ApiError::BadRequest(format!(
            "unsupported interval '{interval}': must be one of {allowed:?}"
        ))
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum_test::TestServer;

    fn test_server() -> TestServer {
        use crate::api::{build_api_router, AppState};

        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/crypto_collector_test")
            .expect("lazy pool");

        TestServer::new(build_api_router(AppState::test(pool)))
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

    // Scenario 6: all API-facing intervals are accepted (validate_interval).
    #[test]
    fn all_api_facing_intervals_are_valid() {
        for iv in ApiInterval::API_FACING {
            let s = iv.as_str();
            let parsed = validate_interval(s).expect("api-facing interval must be valid");
            assert_eq!(parsed, iv, "interval '{s}' must validate to {iv:?}");
        }
    }

    // Scenario 6 (REQ-REFACTOR-062): invalid AND storage-only intervals are rejected.
    #[test]
    fn invalid_interval_is_rejected() {
        // Non-fixed / unknown strings.
        assert!(validate_interval("3h").is_err());
        assert!(validate_interval("").is_err());
        assert!(validate_interval("1hour").is_err());
        assert!(validate_interval("1M").is_err());
        // Storage-only intervals (valid ApiInterval variants, but !is_api_facing) are rejected
        // at the public boundary, exactly as the removed allow-list rejected them.
        for storage_only in ["3m", "30m", "2h", "6h", "8h", "12h", "3d", "4d"] {
            assert!(
                validate_interval(storage_only).is_err(),
                "storage-only interval '{storage_only}' must be rejected at the API boundary"
            );
        }
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

    // Scenario 13 (REQ-API-215): 2h (a storage-only interval) → 400 without querying.
    #[tokio::test]
    async fn list_candles_unsupported_interval_2h_returns_400() {
        let server = test_server();
        let resp = server
            .get("/v1/coins/bitcoin/candles")
            .add_query_param("interval", "2h")
            .await;
        assert_eq!(resp.status_code(), 400, "2h is not API-facing");
    }

    // AC-REFACTOR-062b (characterization, behavior-preserving): after folding the supported-intervals
    // allow-list into ApiInterval::is_api_facing(), a public request for a STORAGE-ONLY interval (`3m`) STILL
    // returns 400 WITHOUT issuing a query — identical to the prior allow-list rejection. This is
    // NOT an intended behavior change. Runs without a live DB precisely because the 400 is emitted
    // at the interval-validation boundary, before ensure_coin_exists / any SQL.
    #[tokio::test]
    async fn list_candles_storage_only_interval_3m_returns_400() {
        let server = test_server();
        let resp = server
            .get("/v1/coins/bitcoin/candles")
            .add_query_param("interval", "3m")
            .await;
        assert_eq!(
            resp.status_code(),
            400,
            "storage-only interval 3m must return 400 at the public boundary (behavior-preserving)"
        );
        let body: serde_json::Value = resp.json();
        assert_eq!(body["code"], "BAD_REQUEST");
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
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for DB tests");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy(&url)
            .expect("lazy pool from DATABASE_URL");
        let state = crate::api::AppState::test(pool);
        let server = TestServer::new(crate::api::build_api_router(state.clone()));
        (server, state)
    }

    // Existing DB-gated test (characterization — REQ-API-200 baseline).
    #[tokio::test]
    #[ignore]
    async fn db_list_candles_unknown_coin_returns_404() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::db::connect(&url).await.expect("db connect");
        let server = TestServer::new(crate::api::build_api_router(crate::api::AppState::test(
            pool,
        )));
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

    // Reproduction test 3 (SPEC-CANDLE-001): when materialized rollup 1d rows exist for a
    // coin, the endpoint must serve them natively — every returned source starts with
    // `rollup:`, never `aggregated:` — because the coin-scoped native EXISTS probe
    // short-circuits before read-time aggregation is ever invoked (Load-Bearing Premise).
    //
    // Requires: bitcoin registered in tracked_coins with at least one materialized
    // `interval='1d'` row whose `source` starts with `rollup:` (produced by a prior
    // `("coin","rollup")` dispatch run).
    #[tokio::test]
    #[ignore]
    async fn db_candle_001_rollup_native_precedence_no_aggregated_source() {
        let (server, _) = db_test_server();
        let resp = server
            .get("/v1/coins/bitcoin/candles")
            .add_query_param("interval", "1d")
            .add_query_param("vs_currency", "usd")
            .await;
        assert_eq!(
            resp.status_code(),
            200,
            "SPEC-CANDLE-001: native 1d rollup read must be 200"
        );
        let body: serde_json::Value = resp.json();
        let items = body["items"].as_array().expect("items array");
        assert!(
            !items.is_empty(),
            "SPEC-CANDLE-001: materialized 1d rollup rows must exist for this fixture"
        );
        for item in items {
            let src = item["source"].as_str().unwrap_or("");
            assert!(
                src.starts_with("rollup:"),
                "SPEC-CANDLE-001 (Load-Bearing Premise): materialized rows must be served \
                 natively with a rollup: source, never aggregated: — got {src}"
            );
            assert!(
                !src.starts_with("aggregated:"),
                "SPEC-CANDLE-001: read-time aggregation must not be invoked when native \
                 rollup rows exist — got {src}"
            );
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
    // dogecoin stores 30m (a storage-only interval); target 1h → source 30m.
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

    // ── SPEC-API-005 M4 (F-31): aggregation reachability & cap-cursor ────────────

    // Self-contained DB fixtures: seed the parent tracked_coins row first (FK) and a set of 1h
    // coin_candles rows; teardown children before the parent.
    #[cfg(test)]
    async fn m4_seed_coin(pool: &sqlx::PgPool, coin_id: &str) {
        sqlx::query("DELETE FROM coin_candles WHERE coin_id = $1")
            .bind(coin_id)
            .execute(pool)
            .await
            .ok();
        sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
            .bind(coin_id)
            .execute(pool)
            .await
            .ok();
        sqlx::query(
            "INSERT INTO tracked_coins (coin_id, symbol, name, status) \
             VALUES ($1, 'T', 'Test', 'active')",
        )
        .bind(coin_id)
        .execute(pool)
        .await
        .expect("seed tracked_coins (FK parent)");
    }

    #[cfg(test)]
    async fn m4_seed_1h_candle(pool: &sqlx::PgPool, coin_id: &str, ts: DateTime<Utc>) {
        sqlx::query(
            "INSERT INTO coin_candles \
             (coin_id, vs_currency, interval, ts, open, high, low, close, volume, source) \
             VALUES ($1, 'usd', '1h', $2, 100, 110, 90, 105, 1, 'binance') \
             ON CONFLICT DO NOTHING",
        )
        .bind(coin_id)
        .bind(ts)
        .execute(pool)
        .await
        .expect("seed 1h coin_candle");
    }

    #[cfg(test)]
    async fn m4_teardown(pool: &sqlx::PgPool, coin_id: &str) {
        sqlx::query("DELETE FROM coin_candles WHERE coin_id = $1")
            .bind(coin_id)
            .execute(pool)
            .await
            .ok();
        sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
            .bind(coin_id)
            .execute(pool)
            .await
            .ok();
    }

    // AC-API-405 [DB-backed]: a far-past [start, end] 4h window aggregated from 1h source is
    // reachable (non-empty) even when many newer 1h rows would otherwise fill the row cap. The
    // `ts < end + one bucket` source upper bound is what makes the far-past window fetchable.
    #[tokio::test]
    #[ignore]
    async fn db_aggregation_far_past_window_is_reachable() {
        use chrono::{Duration, DurationRound};

        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::db::connect(&url).await.expect("db connect");
        let sfx = chrono::Utc::now().timestamp_nanos_opt().unwrap();
        let coin = format!("api5-farpast-{sfx}");
        m4_seed_coin(&pool, &coin).await;

        let now = chrono::Utc::now();
        // Far-past target: two complete 4h buckets ~6 months ago (8 consecutive 1h candles),
        // aligned to a 4h boundary so each bucket holds its full N=4 source candles.
        let base = (now - Duration::days(180))
            .duration_trunc(Duration::hours(4))
            .expect("trunc");
        for h in 0..8i64 {
            m4_seed_1h_candle(&pool, &coin, base + Duration::hours(h)).await;
        }
        // Many recent 1h candles (newer than `end`) that would fill a small row cap first.
        for h in 0..16i64 {
            m4_seed_1h_candle(&pool, &coin, now - Duration::hours(h + 1)).await;
        }

        let state = crate::api::AppState::test(pool.clone());
        let server = TestServer::new(crate::api::build_api_router(state));

        // Small limit → small row cap; without the end-bound the recent rows would fill it and
        // the far-past window would post-filter to nothing.
        let start = base.to_rfc3339();
        let end = (base + Duration::hours(8)).to_rfc3339();
        let resp = server
            .get(&format!("/v1/coins/{coin}/candles"))
            .add_query_param("interval", "4h")
            .add_query_param("vs_currency", "usd")
            .add_query_param("limit", "2")
            .add_query_param("start", &start)
            .add_query_param("end", &end)
            .await;
        assert_eq!(resp.status_code(), 200);
        let body: serde_json::Value = resp.json();
        let items = body["items"].as_array().expect("items");
        assert!(
            !items.is_empty(),
            "far-past [start,end] 4h window must return aggregated buckets, not an empty page \
             (REQ-API-405); got {body}"
        );
        for item in items {
            let ts: DateTime<Utc> = item["ts"].as_str().unwrap().parse().unwrap();
            assert!(
                ts >= base && ts <= base + Duration::hours(8),
                "aggregated bucket ts must fall inside the requested window"
            );
        }

        m4_teardown(&pool, &coin).await;
    }

    // AC-API-406 [DB-backed]: when the source read hits the row cap but every fetched bucket is
    // gap-dropped (agg empty), pagination continues via a cursor derived from the oldest fetched
    // source row's bucket start — next_cursor is non-null rather than terminating.
    #[tokio::test]
    #[ignore]
    async fn db_cap_hit_gap_dropped_page_continues() {
        use chrono::{Duration, DurationRound};

        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::db::connect(&url).await.expect("db connect");
        let sfx = chrono::Utc::now().timestamp_nanos_opt().unwrap();
        let coin = format!("api5-capcursor-{sfx}");
        m4_seed_coin(&pool, &coin).await;

        let now = chrono::Utc::now();
        // Sparse 1h candles: one per distinct 4h bucket, all in the past (closed buckets). Each
        // closed 4h bucket has 1 of N=4 source candles → gap-dropped → aggregation emits nothing.
        // With limit=1 the row cap is (1+1)*4 = 8; seed 10 sparse candles so the cap is hit.
        let base = (now - Duration::days(3))
            .duration_trunc(Duration::hours(4))
            .expect("trunc");
        for b in 0..10i64 {
            m4_seed_1h_candle(&pool, &coin, base - Duration::hours(4 * b)).await;
        }

        let state = crate::api::AppState::test(pool.clone());
        let server = TestServer::new(crate::api::build_api_router(state));

        let resp = server
            .get(&format!("/v1/coins/{coin}/candles"))
            .add_query_param("interval", "4h")
            .add_query_param("vs_currency", "usd")
            .add_query_param("limit", "1")
            .await;
        assert_eq!(resp.status_code(), 200);
        let body: serde_json::Value = resp.json();
        assert!(
            body["items"].as_array().unwrap().is_empty(),
            "every fetched bucket is gap-dropped → this page is empty"
        );
        assert!(
            body["next_cursor"].is_string(),
            "a cap-hit gap-dropped page must continue via a source-bucket cursor, not terminate \
             with next_cursor: null (REQ-API-406); got {body}"
        );

        m4_teardown(&pool, &coin).await;
    }
}
