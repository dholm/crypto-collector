//! Bitcoin halving-cycle overlay read handler (SPEC-CYCLE-001 REQ-CYCLE-050..054).
//!
//! Route:
//! - `GET /v1/coins/{coin_id}/cycle-overlay` → list_cycle_overlay (keyset-paginated)
//!
//! Unlike most `/v1/coins/{coin_id}/...` routes, this endpoint never 404s on an unknown
//! or non-target coin (REQ-CYCLE-052): the query is simply scoped to
//! `(coin_id, vs_currency)` and an unmatched coin naturally yields an empty page.

use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use super::{
    cursor::{decode_keyset_cursor, encode_keyset_cursor, validate_limit, CycleOverlayKey},
    dto::{CycleOverlayPointDto, Page},
    ApiError, ApiResult, AppState,
};
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
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// `GET /v1/coins/{coin_id}/cycle-overlay` — keyset-paginated cycle-overlay read (REQ-CYCLE-050).
///
/// Ordered `(cycle_number ASC, days_since_halving ASC)`. An unknown/non-target coin, or a
/// coin with no computed overlay, yields HTTP 200 with an empty page — NOT 404 (REQ-CYCLE-052).
pub async fn list_cycle_overlay(
    State(state): State<AppState>,
    Path(coin_id): Path<String>,
    Query(params): Query<ListCycleOverlayParams>,
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

    // No `ensure_coin_exists` call here (REQ-CYCLE-052): an unknown/non-target coin or a
    // coin with no computed overlay simply matches zero rows below — HTTP 200 empty page.
    let items: Vec<CycleOverlayPoint> = sqlx::query_as(
        "SELECT coin_id, vs_currency, cycle_number, halving_date, days_since_halving, \
                ts, price, norm_halving, norm_cycle_low, halving_baseline_approximate, \
                projected \
         FROM cycle_overlay_points \
         WHERE coin_id = $1 \
           AND vs_currency = $2 \
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
    .fetch_all(&state.pool)
    .await?;

    let (items, next_cursor) = paginate_cycle_overlay(items, limit);

    Ok(Json(Page {
        items: items.into_iter().map(CycleOverlayPointDto::from).collect(),
        next_cursor,
    }))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

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
}
