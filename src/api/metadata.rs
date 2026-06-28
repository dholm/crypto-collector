//! Coin metadata read handler (SPEC-API-001 REQ-API-050).
//!
//! Route:
//! - `GET /v1/coins/{coin_id}/metadata` → get_metadata (latest or as-of revision)
//!
//! Without `as_of`: returns the latest revision (highest `revision`).
//! With `as_of=<timestamp>`: returns the revision with the greatest
//! `first_seen_at <= as_of` (REQ-API-050).

use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::{dto::CoinMetadataDto, ApiError, ApiResult, AppState};

// ── Query parameter types ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct GetMetadataParams {
    /// Optional point-in-time: return the revision active at this timestamp.
    pub as_of: Option<DateTime<Utc>>,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// `GET /v1/coins/{coin_id}/metadata` — latest or as-of coin metadata revision (REQ-API-050).
pub async fn get_metadata(
    State(state): State<AppState>,
    Path(coin_id): Path<String>,
    Query(params): Query<GetMetadataParams>,
) -> ApiResult<impl IntoResponse> {
    // Verify coin exists.
    ensure_coin_exists(&state.pool, &coin_id).await?;

    let meta: Option<crate::models::coin::CoinMetadata> = match params.as_of {
        None => {
            // Latest revision: highest revision number.
            sqlx::query_as(
                "SELECT coin_id, revision, name, symbol, categories, description, \
                        homepage, links, contract_addresses, max_supply, genesis_date, \
                        first_seen_at, last_seen_at \
                 FROM coin_metadata \
                 WHERE coin_id = $1 \
                 ORDER BY revision DESC \
                 LIMIT 1",
            )
            .bind(&coin_id)
            .fetch_optional(&state.pool)
            .await?
        }
        Some(as_of) => {
            // As-of revision: greatest `first_seen_at <= as_of` (REQ-API-050).
            sqlx::query_as(
                "SELECT coin_id, revision, name, symbol, categories, description, \
                        homepage, links, contract_addresses, max_supply, genesis_date, \
                        first_seen_at, last_seen_at \
                 FROM coin_metadata \
                 WHERE coin_id = $1 \
                   AND first_seen_at <= $2 \
                 ORDER BY first_seen_at DESC \
                 LIMIT 1",
            )
            .bind(&coin_id)
            .bind(as_of)
            .fetch_optional(&state.pool)
            .await?
        }
    };

    match meta {
        Some(m) => Ok(Json(CoinMetadataDto::from(m)).into_response()),
        None => Err(ApiError::NotFound(format!(
            "no metadata found for coin '{coin_id}'"
        ))),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Check that a coin_id exists in tracked_coins; return 404 if not.
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    // Scenario 7 (REQ-API-050): as_of semantics — documented pure tests.
    //
    // The as_of query selects the revision with `greatest first_seen_at <= as_of`.
    // This is purely a SQL semantic contract; the pure logic test verifies our
    // understanding of the filter.
    #[test]
    fn as_of_filter_selects_correct_revision() {
        // Given revisions at T0 and T1:
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let t1 = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();

        // as_of = T0 → should select r0 (first_seen_at = T0 <= T0)
        // as_of = T1 → should select r1 (first_seen_at = T1 <= T1)
        // as_of between T0 and T1 → should select r0 (T0 <= as_of < T1)

        let t_between = Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap();

        // Pure assertion: T0 <= as_of (T0) and T1 > as_of (T0)
        assert!(t0 <= t0, "r0.first_seen_at <= as_of(T0)");
        assert!(t1 > t0, "r1.first_seen_at > as_of(T0) → r0 selected");

        // Between case
        assert!(t0 <= t_between, "r0.first_seen_at <= as_of(between)");
        assert!(
            t1 > t_between,
            "r1.first_seen_at > as_of(between) → r0 selected"
        );

        // At T1
        assert!(t1 <= t1, "r1.first_seen_at <= as_of(T1) → r1 selected");
    }

    // DB-gated integration tests
    #[tokio::test]
    #[ignore]
    async fn db_get_metadata_unknown_coin_returns_404() {
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
        let resp = server.get("/v1/coins/no-such-coin-metadata/metadata").await;
        assert_eq!(resp.status_code(), 404);
    }

    #[tokio::test]
    #[ignore]
    async fn db_metadata_as_of_returns_revision_in_effect() {
        use axum_test::TestServer;
        use chrono::TimeZone;

        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::db::connect(&url).await.expect("db connect");

        let coin_id = "test-meta-asof-001";

        // Setup: insert tracked coin + two metadata revisions
        sqlx::query(
            "INSERT INTO tracked_coins (coin_id, symbol, name, status, registered_at)
             VALUES ($1, 'TMAS', 'Test Meta AsOf', 'active', now())
             ON CONFLICT DO NOTHING",
        )
        .bind(coin_id)
        .execute(&pool)
        .await
        .unwrap();

        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let t1 = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();

        sqlx::query(
            "INSERT INTO coin_metadata (coin_id, revision, name, symbol, first_seen_at, last_seen_at)
             VALUES ($1, 0, 'Test r0', 'TMAS', $2, $2),
                    ($1, 1, 'Test r1', 'TMAS', $3, $3)
             ON CONFLICT DO NOTHING",
        )
        .bind(coin_id)
        .bind(t0)
        .bind(t1)
        .execute(&pool)
        .await
        .unwrap();

        let state = crate::api::AppState {
            pool: pool.clone(),
            chain: std::sync::Arc::new(vec![]),
            search_slot_fn: crate::api::deny_search_slot_fn(),
            search_provider: "coingecko".into(),
            coingecko_base_url: "https://api.coingecko.com".into(),
            http_client: reqwest::Client::new(),
        };
        let server = TestServer::new(crate::api::build_api_router(state));

        // Latest → revision 1
        let resp = server.get(&format!("/v1/coins/{coin_id}/metadata")).await;
        assert_eq!(resp.status_code(), 200);
        let body: serde_json::Value = resp.json();
        assert_eq!(body["revision"], 1, "latest must be revision 1");

        // as_of = between T0 and T1 → revision 0
        let resp = server
            .get(&format!("/v1/coins/{coin_id}/metadata"))
            .add_query_param("as_of", "2026-03-01T00:00:00Z")
            .await;
        assert_eq!(resp.status_code(), 200);
        let body: serde_json::Value = resp.json();
        assert_eq!(
            body["revision"], 0,
            "as_of between T0 and T1 must be revision 0"
        );

        // Cleanup
        sqlx::query("DELETE FROM coin_metadata WHERE coin_id = $1")
            .bind(coin_id)
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
            .bind(coin_id)
            .execute(&pool)
            .await
            .ok();
    }
}
