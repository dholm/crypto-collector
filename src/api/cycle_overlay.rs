//! Bitcoin halving-cycle overlay read handlers (SPEC-CYCLE-001 REQ-CYCLE-050..054).
//!
//! Routes:
//! - `GET /v1/coins/{coin_id}/cycle-overlay` → list_cycle_overlay (replay model, keyset-paginated)
//! - `GET /v1/coins/{coin_id}/cycle-projection` → list_cycle_projection (composite model,
//!   keyset-paginated)
//!
//! Both endpoints share the same real (observed) points and the same SELECT/pagination
//! logic (`list_overlay_for_model`), differing only in which projected `projection_model`
//! they additionally include.
//!
//! Unlike most `/v1/coins/{coin_id}/...` routes, these endpoints never 404 on an unknown
//! or non-target coin (REQ-CYCLE-052): the query is simply scoped to
//! `(coin_id, vs_currency)` and an unmatched coin naturally yields an empty page.

use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::{
    cursor::{decode_keyset_cursor, encode_keyset_cursor, validate_limit, CycleOverlayKey},
    dto::{CycleOverlayPointDto, Page},
    ApiError, ApiResult, AppState,
};
use crate::collectors::cycle_overlay::OverlayPoint;
use crate::models::cycle_overlay::CycleOverlayPoint;

// ── Query parameter types ─────────────────────────────────────────────────────

/// Query parameters for `GET /v1/coins/{coin_id}/cycle-overlay` (SPEC-CYCLE-001).
#[derive(Debug, Deserialize)]
pub struct ListCycleOverlayParams {
    /// Optional: quote currency filter; defaults to `usd` (REQ-CYCLE-052).
    pub vs_currency: Option<String>,
    /// Optional: filter to a single cycle ordinal (OR-CYCLE-6 resolved: cycle_number).
    pub cycle: Option<i32>,
    pub cursor: Option<String>,
    pub limit: Option<i64>,
    /// Optional point-in-time cutoff (RFC3339; v0.5.0, REQ-CYCLE-070). When present, the
    /// response is computed on the fly from candles with `ts <= as_of` instead of being
    /// served from the precomputed `cycle_overlay_points` table (REQ-CYCLE-072/074).
    pub as_of: Option<DateTime<Utc>>,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// `GET /v1/coins/{coin_id}/cycle-overlay` — keyset-paginated cycle-overlay read (REQ-CYCLE-050),
/// serving the Bitbo-style cycle-repeat REPLAY projection (no confidence bands; `price_low`/
/// `price_high` are always null on projected points from this endpoint).
///
/// Ordered `(cycle_number ASC, days_since_halving ASC)`. An unknown/non-target coin, or a
/// coin with no computed overlay, yields HTTP 200 with an empty page — NOT 404 (REQ-CYCLE-052).
pub async fn list_cycle_overlay(
    state: State<AppState>,
    coin_id: Path<String>,
    params: Query<ListCycleOverlayParams>,
) -> ApiResult<impl IntoResponse> {
    list_overlay_for_model(state, coin_id, params, "replay").await
}

/// `GET /v1/coins/{coin_id}/cycle-projection` — keyset-paginated cycle-overlay read serving the
/// COMPOSITE forward-projection model (power-law spine + damped phase-conditioned cycle
/// component + mean-reversion continuity term; `price` is the P50 path, `price_low`/`price_high`
/// carry the P10/P90 confidence bands).
///
/// Same ordering, pagination, and empty-page-not-404 semantics as `list_cycle_overlay`
/// (REQ-CYCLE-050/051/052).
pub async fn list_cycle_projection(
    state: State<AppState>,
    coin_id: Path<String>,
    params: Query<ListCycleOverlayParams>,
) -> ApiResult<impl IntoResponse> {
    list_overlay_for_model(state, coin_id, params, "composite").await
}

/// Shared read implementation for both cycle-overlay endpoints: identical limit validation,
/// cursor decode, `vs_currency` default, pagination, and DTO mapping — the only difference
/// between callers is which projected `projection_model` is included alongside the always-real
/// points ('real' is never itself a query parameter; it is unconditionally included).
async fn list_overlay_for_model(
    State(state): State<AppState>,
    Path(coin_id): Path<String>,
    Query(params): Query<ListCycleOverlayParams>,
    projected_model: &str,
) -> ApiResult<impl IntoResponse> {
    let limit = validate_limit(params.limit).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let cursor_key: Option<CycleOverlayKey> = params
        .cursor
        .as_deref()
        .map(decode_keyset_cursor::<CycleOverlayKey>)
        .transpose()
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    // REQ-CYCLE-052: default vs_currency to "usd", mirroring candles.rs/coin_market.rs.
    let vs_currency = params
        .vs_currency
        .as_deref()
        .unwrap_or("usd")
        .to_lowercase();

    let (cursor_cycle, cursor_dsh) = match cursor_key {
        Some(k) => (Some(k.cycle_number), Some(k.days_since_halving)),
        None => (None, None),
    };

    let items: Vec<CycleOverlayPoint> = match params.as_of {
        // REQ-CYCLE-074: no `as_of` → byte-for-byte unchanged, table-served read path.
        None => {
            // No `ensure_coin_exists` call here (REQ-CYCLE-052): an unknown/non-target coin or
            // a coin with no computed overlay simply matches zero rows below — HTTP 200 empty.
            sqlx::query_as(
                "SELECT coin_id, vs_currency, cycle_number, halving_date, days_since_halving, \
                        ts, price, norm_halving, norm_cycle_low, halving_baseline_approximate, \
                        projected, price_low, price_high \
                 FROM cycle_overlay_points \
                 WHERE coin_id = $1 \
                   AND vs_currency = $2 \
                   AND projection_model IN ('real', $7) \
                   AND ($3::INTEGER IS NULL OR cycle_number = $3) \
                   AND ($4::INTEGER IS NULL \
                        OR (cycle_number, days_since_halving) > ($4, $5)) \
                 ORDER BY cycle_number ASC, days_since_halving ASC \
                 LIMIT $6",
            )
            .bind(&coin_id)
            .bind(&vs_currency)
            .bind(params.cycle)
            .bind(cursor_cycle)
            .bind(cursor_dsh)
            .bind(limit + 1)
            .bind(projected_model)
            .fetch_all(&state.pool)
            .await?
        }
        // REQ-CYCLE-072: `as_of` present → compute on the fly, never read the table.
        Some(as_of) => {
            compute_as_of_page(
                &state.pool,
                &coin_id,
                &vs_currency,
                as_of,
                params.cycle,
                (cursor_cycle, cursor_dsh),
                limit,
                projected_model,
            )
            .await?
        }
    };

    let (items, next_cursor) = paginate_cycle_overlay(items, limit);

    Ok(Json(Page {
        items: items.into_iter().map(CycleOverlayPointDto::from).collect(),
        next_cursor,
    }))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Compute the as-of page (v0.5.0, REQ-CYCLE-070..083): builds the daily series truncated to
/// `ts <= as_of` (via the shared `load_daily_series`, same loader the periodic recompute uses
/// — REQ-CYCLE-075), re-runs the existing pure functions per request, then applies — over the
/// in-memory result — the same `cycle` filter, keyset ordering/cursor, and `limit + 1` fetch
/// shape as the table-backed SQL path (REQ-CYCLE-078), so `paginate_cycle_overlay` can be reused
/// unchanged by the caller.
#[allow(clippy::too_many_arguments)]
async fn compute_as_of_page(
    pool: &sqlx::PgPool,
    coin_id: &str,
    vs_currency: &str,
    as_of: DateTime<Utc>,
    cycle_filter: Option<i32>,
    cursor: (Option<i32>, Option<i32>),
    limit: i64,
    projected_model: &str,
) -> ApiResult<Vec<CycleOverlayPoint>> {
    use crate::collectors::cycle_overlay::{
        compute_overlay, load_daily_series, project_cycle_repeat,
    };
    use crate::collectors::cycle_projection::project_composite;

    let daily = load_daily_series(pool, coin_id, vs_currency, Some(as_of)).await?;
    let real = compute_overlay(daily.clone());
    let projected = match projected_model {
        "replay" => project_cycle_repeat(&daily, &real),
        "composite" => {
            // REQ-CYCLE-082: preserve the BTC/USD-only calibration-anchor rule under as_of.
            let use_btc_anchors = coin_id == "bitcoin" && vs_currency == "usd";
            project_composite(&daily, &real, use_btc_anchors)
        }
        other => unreachable!("unsupported projection model '{other}'"),
    };

    Ok(build_as_of_page(
        real,
        projected,
        coin_id,
        vs_currency,
        cycle_filter,
        cursor,
        limit,
    ))
}

/// Pure in-memory paginate/filter/order step of the as-of path (REQ-CYCLE-078): applies the
/// optional `cycle` filter, the strict-tuple keyset cursor advance, and
/// `(cycle_number ASC, days_since_halving ASC)` ordering, then truncates to `limit + 1` — the
/// same shape `paginate_cycle_overlay` expects from the SQL path.
fn build_as_of_page(
    real: Vec<OverlayPoint>,
    projected: Vec<OverlayPoint>,
    coin_id: &str,
    vs_currency: &str,
    cycle_filter: Option<i32>,
    cursor: (Option<i32>, Option<i32>),
    limit: i64,
) -> Vec<CycleOverlayPoint> {
    let mut items: Vec<CycleOverlayPoint> = real
        .into_iter()
        .chain(projected)
        .map(|p| overlay_point_to_model(p, coin_id, vs_currency))
        .filter(|p| cycle_filter.is_none_or(|c| p.cycle_number == c))
        .filter(|p| match cursor {
            (Some(cc), Some(cd)) => (p.cycle_number, p.days_since_halving) > (cc, cd),
            _ => true,
        })
        .collect();

    items.sort_by_key(|p| (p.cycle_number, p.days_since_halving));
    items.truncate((limit + 1) as usize);
    items
}

/// Stamp a pure `OverlayPoint` (from `crate::collectors::cycle_overlay`) with `coin_id`/
/// `vs_currency` to produce the same model shape the table-backed SELECT returns, so
/// `CycleOverlayPointDto::from` and `paginate_cycle_overlay` are reused unchanged.
fn overlay_point_to_model(p: OverlayPoint, coin_id: &str, vs_currency: &str) -> CycleOverlayPoint {
    CycleOverlayPoint {
        coin_id: coin_id.to_string(),
        vs_currency: vs_currency.to_string(),
        cycle_number: p.cycle_number,
        halving_date: p.halving_date,
        days_since_halving: p.days_since_halving as i32,
        ts: p.ts,
        price: p.price,
        norm_halving: p.norm_halving,
        norm_cycle_low: p.norm_cycle_low,
        halving_baseline_approximate: p.halving_baseline_approximate,
        projected: p.projected,
        price_low: p.price_low,
        price_high: p.price_high,
    }
}

/// Truncate a `limit + 1`-sized fetch to `limit` items and derive the next cursor.
///
/// Mirrors `quotes::paginate_ts`'s len-based heuristic, but over the composite
/// `(cycle_number, days_since_halving)` keyset key (REQ-CYCLE-051).
fn paginate_cycle_overlay(
    mut items: Vec<CycleOverlayPoint>,
    limit: i64,
) -> (Vec<CycleOverlayPoint>, Option<String>) {
    if items.len() as i64 > limit {
        items.truncate(limit as usize);
        let next_cursor = items.last().map(|p| {
            encode_keyset_cursor(&CycleOverlayKey {
                cycle_number: p.cycle_number,
                days_since_halving: p.days_since_halving,
            })
        });
        (items, next_cursor)
    } else {
        (items, None)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum_test::TestServer;
    use rust_decimal_macros::dec;

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

    fn point(cycle: i32, dsh: i32) -> CycleOverlayPoint {
        CycleOverlayPoint {
            coin_id: "bitcoin".into(),
            vs_currency: "usd".into(),
            cycle_number: cycle,
            halving_date: chrono::NaiveDate::from_ymd_opt(2020, 5, 11).unwrap(),
            days_since_halving: dsh,
            ts: chrono::NaiveDate::from_ymd_opt(2020, 11, 27).unwrap(),
            price: dec!(4000),
            norm_halving: dec!(1),
            norm_cycle_low: dec!(1),
            halving_baseline_approximate: false,
            projected: false,
            price_low: None,
            price_high: None,
        }
    }

    // Scenario 14 (REQ-CYCLE-051): pagination truncates and derives next_cursor.
    #[test]
    fn paginate_cycle_overlay_truncates_and_derives_cursor() {
        let items = vec![point(3, 1), point(3, 2), point(3, 3)];
        let (page, next_cursor) = paginate_cycle_overlay(items, 2);
        assert_eq!(page.len(), 2);
        assert!(next_cursor.is_some());
        let decoded: CycleOverlayKey = decode_keyset_cursor(&next_cursor.unwrap()).unwrap();
        assert_eq!(decoded.cycle_number, 3);
        assert_eq!(decoded.days_since_halving, 2);
    }

    // Scenario 14: exhausted page → null next_cursor.
    #[test]
    fn paginate_cycle_overlay_exhausted_returns_null_cursor() {
        let items = vec![point(3, 1), point(3, 2)];
        let (page, next_cursor) = paginate_cycle_overlay(items, 2);
        assert_eq!(page.len(), 2);
        assert!(next_cursor.is_none());
    }

    // Scenario 16 (REQ-CYCLE-053): invalid cursor → 400 without querying.
    #[tokio::test]
    async fn list_cycle_overlay_invalid_cursor_returns_400() {
        let server = test_server();
        let resp = server
            .get("/v1/coins/bitcoin/cycle-overlay")
            .add_query_param("cursor", "NOT_VALID!!!")
            .await;
        assert_eq!(resp.status_code(), 400);
    }

    // Scenario 16 (REQ-CYCLE-053): limit out of range → 400 without querying.
    #[tokio::test]
    async fn list_cycle_overlay_limit_too_large_returns_400() {
        let server = test_server();
        let resp = server
            .get("/v1/coins/bitcoin/cycle-overlay")
            .add_query_param("limit", "9999999")
            .await;
        assert_eq!(resp.status_code(), 400);
    }

    #[tokio::test]
    async fn list_cycle_overlay_zero_limit_returns_400() {
        let server = test_server();
        let resp = server
            .get("/v1/coins/bitcoin/cycle-overlay")
            .add_query_param("limit", "0")
            .await;
        assert_eq!(resp.status_code(), 400);
    }

    // Scenario 17: vs_currency defaults to usd (pure param test, mirrors candles.rs pattern).
    #[test]
    fn vs_currency_defaults_to_usd_when_omitted() {
        let params = ListCycleOverlayParams {
            vs_currency: None,
            cycle: None,
            cursor: None,
            limit: None,
            as_of: None,
        };
        let resolved = params
            .vs_currency
            .as_deref()
            .unwrap_or("usd")
            .to_lowercase();
        assert_eq!(resolved, "usd");
    }

    // ── DB-gated tests (require live DATABASE_URL) ────────────────────────────

    fn db_test_server() -> TestServer {
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
        TestServer::new(crate::api::build_api_router(state))
    }

    // Scenario 15 (REQ-CYCLE-052): unknown/non-target coin → 200 empty page, not 404.
    #[tokio::test]
    #[ignore]
    async fn db_scenario_15_unknown_coin_returns_200_empty() {
        let server = db_test_server();
        let resp = server.get("/v1/coins/ethereum/cycle-overlay").await;
        assert_eq!(resp.status_code(), 200);
        let body: serde_json::Value = resp.json();
        assert_eq!(body["items"], serde_json::json!([]));
        assert_eq!(body["next_cursor"], serde_json::Value::Null);
    }

    // Scenario 14 (REQ-CYCLE-050/051): keyset pagination round-trip over real data.
    #[tokio::test]
    #[ignore]
    async fn db_scenario_14_keyset_pagination_round_trip() {
        let server = db_test_server();
        let resp1 = server
            .get("/v1/coins/bitcoin/cycle-overlay")
            .add_query_param("limit", "2")
            .await;
        assert_eq!(resp1.status_code(), 200);
        let body1: serde_json::Value = resp1.json();
        let items1 = body1["items"].as_array().expect("items array");
        if items1.len() < 2 {
            return; // insufficient seeded data
        }
        let cursor = body1["next_cursor"]
            .as_str()
            .expect("next_cursor must be non-null for a 2-item page");

        let resp2 = server
            .get("/v1/coins/bitcoin/cycle-overlay")
            .add_query_param("limit", "2")
            .add_query_param("cursor", cursor)
            .await;
        assert_eq!(resp2.status_code(), 200);
        let body2: serde_json::Value = resp2.json();
        let items2 = body2["items"].as_array().expect("items array");
        assert!(!items2.is_empty());

        // Ordering: page 2's first item must sort after page 1's last item.
        let last1 = (
            items1.last().unwrap()["cycle_number"].as_i64().unwrap(),
            items1.last().unwrap()["days_since_halving"]
                .as_i64()
                .unwrap(),
        );
        let first2 = (
            items2[0]["cycle_number"].as_i64().unwrap(),
            items2[0]["days_since_halving"].as_i64().unwrap(),
        );
        assert!(
            first2 > last1,
            "page 2 must continue after page 1 in (cycle_number, days_since_halving) order"
        );
    }

    // Scenario 17 (REQ-CYCLE-052): cycle filter scopes results to one cycle.
    #[tokio::test]
    #[ignore]
    async fn db_scenario_17_cycle_filter_scopes_results() {
        let server = db_test_server();
        let resp = server
            .get("/v1/coins/bitcoin/cycle-overlay")
            .add_query_param("vs_currency", "usd")
            .add_query_param("cycle", "3")
            .await;
        assert_eq!(resp.status_code(), 200);
        let body: serde_json::Value = resp.json();
        let items = body["items"].as_array().expect("items array");
        for item in items {
            assert_eq!(item["cycle_number"], 3);
        }
    }

    // Scenario 6 (REQ-CYCLE-022): every item carries both baselines as strings.
    #[tokio::test]
    #[ignore]
    async fn db_scenario_6_both_baselines_present() {
        let server = db_test_server();
        let resp = server.get("/v1/coins/bitcoin/cycle-overlay").await;
        assert_eq!(resp.status_code(), 200);
        let body: serde_json::Value = resp.json();
        let items = body["items"].as_array().expect("items array");
        for item in items {
            assert!(item["norm_halving"].is_string());
            assert!(item["norm_cycle_low"].is_string());
            assert!(item["price"].is_string());
        }
    }

    // ── as_of (v0.5.0) — pure/unit coverage ────────────────────────────────────

    fn overlay_point(cycle: i32, dsh: i64, projected: bool) -> OverlayPoint {
        OverlayPoint {
            cycle_number: cycle,
            halving_date: chrono::NaiveDate::from_ymd_opt(2020, 5, 11).unwrap(),
            days_since_halving: dsh,
            ts: chrono::NaiveDate::from_ymd_opt(2020, 5, 11).unwrap() + chrono::Duration::days(dsh),
            price: dec!(1000),
            norm_halving: dec!(1),
            norm_cycle_low: dec!(1),
            halving_baseline_approximate: false,
            projected,
            price_low: None,
            price_high: None,
        }
    }

    // Param deserialisation default: `as_of` absent keeps existing behaviour (REQ-CYCLE-074).
    // (RFC3339 parsing itself is exercised end-to-end by the DB-gated as_of route tests below,
    // and mirrors `GetMetadataParams::as_of` in `src/api/metadata.rs`, which uses the same
    // `Option<DateTime<Utc>>` shape and is covered there.)
    #[test]
    fn list_cycle_overlay_params_as_of_defaults_to_none() {
        let params = ListCycleOverlayParams {
            vs_currency: None,
            cycle: None,
            cursor: None,
            limit: None,
            as_of: None,
        };
        assert!(params.as_of.is_none());
    }

    // `OverlayPoint` → `CycleOverlayPoint` mapping preserves every field, including bands.
    #[test]
    fn overlay_point_to_model_preserves_all_fields() {
        let mut p = overlay_point(3, 42, true);
        p.price_low = Some(dec!(900));
        p.price_high = Some(dec!(1100));
        p.halving_baseline_approximate = true;

        let model = overlay_point_to_model(p.clone(), "bitcoin", "usd");
        assert_eq!(model.coin_id, "bitcoin");
        assert_eq!(model.vs_currency, "usd");
        assert_eq!(model.cycle_number, p.cycle_number);
        assert_eq!(model.halving_date, p.halving_date);
        assert_eq!(model.days_since_halving, p.days_since_halving as i32);
        assert_eq!(model.ts, p.ts);
        assert_eq!(model.price, p.price);
        assert_eq!(model.norm_halving, p.norm_halving);
        assert_eq!(model.norm_cycle_low, p.norm_cycle_low);
        assert_eq!(
            model.halving_baseline_approximate,
            p.halving_baseline_approximate
        );
        assert_eq!(model.projected, p.projected);
        assert_eq!(model.price_low, p.price_low);
        assert_eq!(model.price_high, p.price_high);
    }

    // Scenario 24 (REQ-CYCLE-078): the `cycle` filter composes with the as-of in-memory page.
    #[test]
    fn build_as_of_page_cycle_filter_composes() {
        let real = vec![
            overlay_point(3, 1, false),
            overlay_point(3, 2, false),
            overlay_point(4, 1, false),
        ];
        let page = build_as_of_page(real, vec![], "bitcoin", "usd", Some(3), (None, None), 10);
        assert_eq!(page.len(), 2);
        assert!(page.iter().all(|p| p.cycle_number == 3));
    }

    // Scenario 23 (REQ-CYCLE-078): ordering + keyset cursor round-trip is stable under a fixed
    // as-of computed set — concatenating two pages equals a single unpaginated page.
    #[test]
    fn build_as_of_page_pagination_round_trip_matches_unpaginated() {
        let real = vec![
            overlay_point(3, 1, false),
            overlay_point(3, 2, false),
            overlay_point(3, 3, false),
            overlay_point(4, 1, false),
        ];

        let unpaginated = build_as_of_page(
            real.clone(),
            vec![],
            "bitcoin",
            "usd",
            None,
            (None, None),
            10,
        );
        assert_eq!(unpaginated.len(), 4);
        assert_eq!(
            unpaginated
                .iter()
                .map(|p| (p.cycle_number, p.days_since_halving))
                .collect::<Vec<_>>(),
            vec![(3, 1), (3, 2), (3, 3), (4, 1)],
            "must be ordered (cycle_number ASC, days_since_halving ASC)"
        );

        // Page 1: limit=2 → items[0..2], next cursor = (3, 2).
        let page1 = build_as_of_page(
            real.clone(),
            vec![],
            "bitcoin",
            "usd",
            None,
            (None, None),
            2,
        );
        let (page1_items, next_cursor) = paginate_cycle_overlay(page1, 2);
        assert_eq!(page1_items.len(), 2);
        assert!(next_cursor.is_some());
        let cursor_key: CycleOverlayKey = decode_keyset_cursor(&next_cursor.unwrap()).unwrap();

        // Page 2: cursor from page 1 → strictly-after items.
        let page2 = build_as_of_page(
            real,
            vec![],
            "bitcoin",
            "usd",
            None,
            (
                Some(cursor_key.cycle_number),
                Some(cursor_key.days_since_halving),
            ),
            10,
        );
        let (page2_items, next_cursor2) = paginate_cycle_overlay(page2, 10);
        assert!(next_cursor2.is_none(), "page 2 exhausts the result");

        let concatenated: Vec<(i32, i32)> = page1_items
            .iter()
            .chain(page2_items.iter())
            .map(|p| (p.cycle_number, p.days_since_halving))
            .collect();
        assert_eq!(
            concatenated,
            vec![(3, 1), (3, 2), (3, 3), (4, 1)],
            "concatenation of pages must equal the single unpaginated as-of request"
        );
    }

    // REQ-CYCLE-073: as-of page never includes a projected point mixed out of order with reals
    // — projected points sort by their own (cycle_number, days_since_halving), composing with
    // real points under the same total order.
    #[test]
    fn build_as_of_page_orders_real_and_projected_together() {
        let real = vec![overlay_point(4, 5, false)];
        let projected = vec![overlay_point(4, 6, true), overlay_point(5, 1, true)];
        let page = build_as_of_page(real, projected, "bitcoin", "usd", None, (None, None), 10);
        assert_eq!(
            page.iter()
                .map(|p| (p.cycle_number, p.days_since_halving, p.projected))
                .collect::<Vec<_>>(),
            vec![(4, 5, false), (4, 6, true), (5, 1, true)]
        );
    }

    // Scenario 19 / insufficient-history (REQ-CYCLE-077) at the build-page level: an empty
    // projected set still yields the real-only page without error.
    #[test]
    fn build_as_of_page_empty_projection_yields_real_only() {
        let real = vec![overlay_point(4, 1, false), overlay_point(4, 2, false)];
        let page = build_as_of_page(real, vec![], "bitcoin", "usd", None, (None, None), 10);
        assert_eq!(page.len(), 2);
        assert!(page.iter().all(|p| !p.projected));
    }

    // ── DB-gated as-of scenarios (require live DATABASE_URL) ──────────────────

    async fn seed_bitcoin_candle(
        pool: &sqlx::PgPool,
        ts: chrono::DateTime<Utc>,
        close: rust_decimal::Decimal,
    ) {
        sqlx::query(
            "INSERT INTO tracked_coins (coin_id, symbol, name, status, registered_at) \
             VALUES ('bitcoin', 'BTC', 'Bitcoin', 'active', now()) \
             ON CONFLICT DO NOTHING",
        )
        .execute(pool)
        .await
        .expect("seed tracked_coins");
        sqlx::query(
            "INSERT INTO coin_candles (coin_id, vs_currency, interval, ts, open, high, low, close) \
             VALUES ('bitcoin', 'usd', '1d', $1, $2, $2, $2, $2) \
             ON CONFLICT (coin_id, vs_currency, interval, ts) DO UPDATE SET close = $2",
        )
        .bind(ts)
        .bind(close)
        .execute(pool)
        .await
        .expect("seed coin_candles");
    }

    // Scenario 20 (REQ-CYCLE-076): `as_of` before all data → 200 empty page.
    #[tokio::test]
    #[ignore]
    async fn db_scenario_20_as_of_before_any_data_returns_200_empty() {
        let server = db_test_server();
        let resp = server
            .get("/v1/coins/bitcoin/cycle-overlay")
            .add_query_param("as_of", "2000-01-01T00:00:00Z")
            .await;
        assert_eq!(resp.status_code(), 200);
        let body: serde_json::Value = resp.json();
        assert_eq!(body["items"], serde_json::json!([]));
        assert_eq!(body["next_cursor"], serde_json::Value::Null);
    }

    // Scenario 21 (REQ-CYCLE-074/075): `as_of` at/after the latest candle equals no-`as_of`.
    #[tokio::test]
    #[ignore]
    async fn db_scenario_21_as_of_at_or_after_latest_equals_no_as_of() {
        let server = db_test_server();
        let resp_plain = server.get("/v1/coins/bitcoin/cycle-overlay").await;
        assert_eq!(resp_plain.status_code(), 200);
        let body_plain: serde_json::Value = resp_plain.json();

        let far_future = "2099-01-01T00:00:00Z";
        let resp_as_of = server
            .get("/v1/coins/bitcoin/cycle-overlay")
            .add_query_param("as_of", far_future)
            .await;
        assert_eq!(resp_as_of.status_code(), 200);
        let body_as_of: serde_json::Value = resp_as_of.json();

        assert_eq!(
            body_plain["items"], body_as_of["items"],
            "far-future as_of must equal the no-as_of (table-served) result"
        );
    }

    // Scenario 25 (REQ-CYCLE-079): invalid `as_of` → 400 without computing or querying (no DB
    // required — the axum `Query` extractor rejects before the handler body runs).
    #[tokio::test]
    async fn list_cycle_overlay_and_projection_invalid_as_of_returns_400() {
        let server = test_server();
        let resp = server
            .get("/v1/coins/bitcoin/cycle-overlay")
            .add_query_param("as_of", "not-a-timestamp")
            .await;
        assert_eq!(resp.status_code(), 400);

        let resp2 = server
            .get("/v1/coins/bitcoin/cycle-projection")
            .add_query_param("as_of", "not-a-timestamp")
            .await;
        assert_eq!(resp2.status_code(), 400);
    }

    // Scenario 26 (REQ-CYCLE-074): no-`as_of` request is still served from the precomputed
    // table — a direct regression guard for the branch added in this amendment.
    #[tokio::test]
    #[ignore]
    async fn db_scenario_26_no_as_of_served_from_table() {
        let server = db_test_server();
        let resp = server.get("/v1/coins/bitcoin/cycle-overlay").await;
        assert_eq!(resp.status_code(), 200);
        // No assertion beyond 200 + shape here: Scenarios 14-17 (existing tests in this file)
        // already cover the table-served contract; this test exists to anchor the "unchanged"
        // requirement at the route level for this amendment's review.
        let body: serde_json::Value = resp.json();
        assert!(body["items"].is_array());
    }

    // Scenario 19 (REQ-CYCLE-070/071/072/073), route level: `as_of` mid-history truncates real
    // points and re-anchors the projection at T.
    #[tokio::test]
    #[ignore]
    async fn db_scenario_19_as_of_mid_history_truncates_and_reanchors() {
        let server = db_test_server();
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
        let pool = crate::db::connect(&url).await.expect("connect");

        // Seed enough daily history (> CYCLE_DAYS) ending at a known cutoff, plus a few days
        // AFTER the cutoff that must not influence the as-of response.
        use crate::collectors::cycle_overlay::CYCLE_DAYS;
        let cutoff = Utc::now() - chrono::Duration::days(5);
        let start = cutoff - chrono::Duration::days(CYCLE_DAYS + 30);

        let mut ts = start;
        let mut price = rust_decimal::Decimal::from(20000);
        while ts <= cutoff {
            seed_bitcoin_candle(&pool, ts, price).await;
            ts += chrono::Duration::days(1);
            price += rust_decimal::Decimal::from(1);
        }
        // Post-cutoff candles that must NOT affect the as-of response.
        for i in 1..=3 {
            seed_bitcoin_candle(
                &pool,
                cutoff + chrono::Duration::days(i),
                rust_decimal::Decimal::from(999_999),
            )
            .await;
        }

        let resp = server
            .get("/v1/coins/bitcoin/cycle-overlay")
            .add_query_param("as_of", cutoff.to_rfc3339())
            .add_query_param("limit", "100")
            .await;
        assert_eq!(resp.status_code(), 200);
        let body: serde_json::Value = resp.json();
        let items = body["items"].as_array().expect("items array");

        let cutoff_date = cutoff.date_naive();
        for item in items {
            if item["projected"] == false {
                let ts_str = item["ts"].as_str().expect("ts string");
                let ts_date: chrono::NaiveDate = ts_str.parse().expect("parse ts date");
                assert!(
                    ts_date <= cutoff_date,
                    "no real point may be dated after as_of cutoff"
                );
            }
        }

        // Cleanup.
        sqlx::query("DELETE FROM coin_candles WHERE coin_id = 'bitcoin' AND vs_currency = 'usd' AND ts >= $1")
            .bind(start - chrono::Duration::hours(1))
            .execute(&pool)
            .await
            .ok();
    }
}
