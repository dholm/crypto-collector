//! Coin-keyed spot quote read handlers (SPEC-API-002 REQ-API-131/132).
//!
//! Routes:
//! - `GET /v1/coins/{coin_id}/quotes/latest` → get_latest_quote
//! - `GET /v1/coins/{coin_id}/quotes`        → list_quotes (keyset-paginated, time-range)

use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::{
    cursor::{decode_keyset_cursor, encode_keyset_cursor, validate_limit, TsKey},
    dto::{CoinQuoteDto, CoinQuoteOverviewDto, CoinQuoteOverviewPage, Page},
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

#[derive(Debug, Deserialize)]
pub struct ListLatestQuotesParams {
    pub vs_currency: Option<String>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// `GET /v1/coins/{coin_id}/quotes/latest` — newest spot quote for a coin (REQ-API-131).
pub async fn get_latest_quote(
    State(state): State<AppState>,
    Path(coin_id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    ensure_coin_exists(&state.pool, &coin_id).await?;

    let quote: Option<crate::models::quote::CoinQuote> = sqlx::query_as(
        "SELECT coin_id, vs_currency, ts, price, source \
         FROM coin_quotes \
         WHERE coin_id = $1 \
         ORDER BY ts DESC \
         LIMIT 1",
    )
    .bind(&coin_id)
    .fetch_optional(&state.pool)
    .await?;

    match quote {
        Some(q) => Ok(Json(CoinQuoteDto::from(q)).into_response()),
        None => Err(ApiError::NotFound(format!(
            "no quotes found for coin '{coin_id}'"
        ))),
    }
}

/// `GET /v1/coins/{coin_id}/quotes` — keyset-paginated quote history (REQ-API-132).
pub async fn list_quotes(
    State(state): State<AppState>,
    Path(coin_id): Path<String>,
    Query(params): Query<ListQuotesParams>,
) -> ApiResult<impl IntoResponse> {
    let limit = validate_limit(params.limit).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let cursor_ts: Option<DateTime<Utc>> = params
        .cursor
        .as_deref()
        .map(|c| decode_keyset_cursor::<TsKey>(c).map(|k| k.ts))
        .transpose()
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    ensure_coin_exists(&state.pool, &coin_id).await?;

    let items: Vec<crate::models::quote::CoinQuote> = sqlx::query_as(
        "SELECT coin_id, vs_currency, ts, price, source \
         FROM coin_quotes \
         WHERE coin_id = $1 \
           AND ($2::TIMESTAMPTZ IS NULL OR ts <= $2) \
           AND ($3::TIMESTAMPTZ IS NULL OR ts >= $3) \
           AND ($4::TIMESTAMPTZ IS NULL OR ts < $4) \
         ORDER BY ts DESC \
         LIMIT $5",
    )
    .bind(&coin_id)
    .bind(params.end)
    .bind(params.start)
    .bind(cursor_ts)
    .bind(limit + 1)
    .fetch_all(&state.pool)
    .await?;

    let (items, next_cursor) = paginate_ts(items, limit, |q| q.ts);
    Ok(Json(Page {
        items: items.into_iter().map(CoinQuoteDto::from).collect(),
        next_cursor,
    }))
}

/// `GET /v1/coins/quotes/latest` — all-coin latest-quote overview (SPEC-API-004 REQ-API-300).
///
/// Returns one overview row per **active** tracked coin with a current quote in the bounded
/// window, each carrying the coin's current spot price and a nullable 24h-ago baseline
/// (`open_24h`). Bare `{"quotes":[...]}` envelope (REQ-API-301); empty result is `{"quotes":[]}`.
///
/// `open_24h` is the earliest quote in the trailing 24h window that is **strictly older than the
/// current quote** (`ts < q.ts`); it is `null` when the window holds no such earlier quote — e.g.
/// a newly-tracked coin whose only quote is the current one. The current quote is never reused as
/// its own baseline (that would report a fabricated 0% change) (REQ-API-303, D3/D4).
///
// @MX:NOTE: [AUTO] vs_currency defaults to `usd` via `.unwrap_or("usd")`; no allow-list —
//           an unrecognised currency simply matches no rows (200 empty), not a 400. Only
//           `status='active'` coins are considered; absent-on-stale drops any coin with no
//           quote in the 48h window (REQ-API-306/307, D5/D6/D8). The baseline LATERAL adds
//           `ts < q.ts` so open_24h is strictly older than the current quote → null for a
//           newly-tracked coin, never a fake 0% change (REQ-API-303, D3).
pub async fn list_latest_quotes(
    State(state): State<AppState>,
    Query(params): Query<ListLatestQuotesParams>,
) -> ApiResult<impl IntoResponse> {
    let vs_currency = params.vs_currency.as_deref().unwrap_or("usd");

    // @MX:ANCHOR: [AUTO] all-coin overview query — every coin_quotes read is ts-bounded
    // @MX:REASON: coin_quotes is PARTITION BY RANGE(ts) with 48 monthly partitions. Each LATERAL
    //             carries `ts >= now() - interval '...'` so PostgreSQL prunes partitions at
    //             execution time (now() is STABLE → runtime pruning, PG11+). The whole endpoint's
    //             correctness AND performance depend on this shape.
    // @MX:WARN: NEVER remove the `ts >= now() - interval` lower bound from either LATERAL. An
    //           unbounded DISTINCT ON over the coin_quotes parent touches all 48 partitions — a
    //           sibling service shipped that shape and produced a 41s query that blew a 30s
    //           client timeout (REQ-API-305, D7).
    // @MX:SPEC: SPEC-API-004 REQ-API-305
    let quotes: Vec<CoinQuoteOverviewDto> = sqlx::query_as(
        "SELECT c.coin_id, q.vs_currency, q.ts, q.price, q.source, b.price AS open_24h \
         FROM tracked_coins c \
         CROSS JOIN LATERAL ( \
             SELECT vs_currency, ts, price, source FROM coin_quotes \
             WHERE coin_id = c.coin_id AND vs_currency = $1 \
               AND ts >= now() - interval '48 hours' \
             ORDER BY ts DESC LIMIT 1 \
         ) q \
         LEFT JOIN LATERAL ( \
             SELECT price FROM coin_quotes \
             WHERE coin_id = c.coin_id AND vs_currency = $1 \
               AND ts >= now() - interval '24 hours' \
               AND ts < q.ts \
             ORDER BY ts ASC LIMIT 1 \
         ) b ON TRUE \
         WHERE c.status = 'active'",
    )
    .bind(vs_currency)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(CoinQuoteOverviewPage { quotes }))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Check that a coin_id exists; return 404 if not.
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

    fn make_coin_quote(h: u32, price_str: &str) -> crate::models::quote::CoinQuote {
        use rust_decimal::Decimal;
        use std::str::FromStr;
        crate::models::quote::CoinQuote {
            coin_id: "bitcoin".into(),
            vs_currency: "usd".into(),
            ts: ts(h),
            price: Decimal::from_str(price_str).unwrap(),
            source: "test".into(),
        }
    }

    // paginate_ts: has_more → next_cursor encodes last item ts
    #[test]
    fn paginate_ts_has_more_returns_cursor() {
        let items = vec![
            make_coin_quote(12, "100"),
            make_coin_quote(11, "99"),
            make_coin_quote(10, "98"),
        ];

        let (trimmed, next_cursor) = paginate_ts(items, 2, |q| q.ts);
        assert_eq!(trimmed.len(), 2);
        assert!(next_cursor.is_some());
        let key: TsKey = decode_keyset_cursor(next_cursor.as_ref().unwrap()).unwrap();
        assert_eq!(key.ts, ts(11), "cursor must encode last returned row ts");
    }

    #[test]
    fn paginate_ts_no_more_returns_null_cursor() {
        let items = vec![make_coin_quote(12, "100")];
        let (_, next_cursor) = paginate_ts(items, 100, |q| q.ts);
        assert!(next_cursor.is_none());
    }

    // Build an AppState with empty provider chain for DB-gated router tests.
    #[cfg(test)]
    fn test_state(pool: sqlx::PgPool) -> crate::api::AppState {
        crate::api::AppState {
            pool,
            chain: std::sync::Arc::new(vec![]),
            search_provider: "coingecko".into(),
            coingecko_base_url: "https://api.coingecko.com".into(),
            http_client: reqwest::Client::new(),
            coin_quote_tx: tokio::sync::broadcast::channel(16).0,
            coin_candle_tx: tokio::sync::broadcast::channel(16).0,
        }
    }

    // DB-gated integration tests
    #[tokio::test]
    #[ignore]
    async fn db_latest_quote_unknown_coin_returns_404() {
        use axum_test::TestServer;
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::db::connect(&url).await.expect("db connect");
        let server = TestServer::new(crate::api::build_api_router(test_state(pool)));
        let resp = server.get("/v1/coins/no-such-coin-xyz/quotes/latest").await;
        assert_eq!(resp.status_code(), 404);
    }

    // SPEC-API-004 Scenarios 1/3/4/5/8 [DB-backed]: the all-coin overview returns a current coin
    // with its 24h baseline, omits a stale-only (>48h) coin, sets open_24h=null for a coin with
    // no quote in the trailing 24h window, and yields {"quotes":[]} for an unrecognised currency.
    #[tokio::test]
    #[ignore]
    async fn db_latest_quotes_overview_current_stale_and_null_baseline() {
        use axum_test::TestServer;
        use chrono::Duration;
        use rust_decimal::Decimal;
        use rust_decimal_macros::dec;
        use serde_json::Value;

        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::db::connect(&url).await.expect("db connect");

        // Unique coin ids so the test is isolated on a shared DB.
        let sfx = chrono::Utc::now().timestamp_nanos_opt().unwrap();
        let coin_current = format!("sp4-current-{sfx}");
        let coin_stale = format!("sp4-stale-{sfx}");
        let coin_nobaseline = format!("sp4-nobaseline-{sfx}");
        let coin_recent_only = format!("sp4-recent-only-{sfx}");
        let coins = [
            &coin_current,
            &coin_stale,
            &coin_nobaseline,
            &coin_recent_only,
        ];
        let now = chrono::Utc::now();

        async fn seed_quote(
            pool: &sqlx::PgPool,
            coin: &str,
            ts: chrono::DateTime<chrono::Utc>,
            price: Decimal,
        ) {
            sqlx::query(
                "INSERT INTO coin_quotes (coin_id, vs_currency, ts, price, source) \
                 VALUES ($1, 'usd', $2, $3, 'test')",
            )
            .bind(coin)
            .bind(ts)
            .bind(price)
            .execute(pool)
            .await
            .expect("insert quote");
        }

        // Defensive cleanup then seed active coins.
        for c in coins {
            sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
                .bind(c)
                .execute(&pool)
                .await
                .expect("pre-cleanup");
            sqlx::query(
                "INSERT INTO tracked_coins (coin_id, symbol, name, status) \
                 VALUES ($1, 'T', 'Test', 'active')",
            )
            .bind(c)
            .execute(&pool)
            .await
            .expect("insert coin");
        }

        // current: newest at now-2h + baseline at now-23h (inside the 24h window).
        seed_quote(&pool, &coin_current, now - Duration::hours(2), dec!(100)).await;
        seed_quote(&pool, &coin_current, now - Duration::hours(23), dec!(90)).await;
        // stale: only >48h old → dropped by the 48h current-price window.
        seed_quote(&pool, &coin_stale, now - Duration::hours(60), dec!(50)).await;
        // nobaseline: single quote in (24h, 48h) → appears, but open_24h null (nothing in 24h).
        seed_quote(&pool, &coin_nobaseline, now - Duration::hours(30), dec!(70)).await;
        // recent-only (newly-tracked): a single recent quote inside the 24h window is BOTH the
        // current price and the only 24h-window row. open_24h MUST be null (the baseline must be
        // strictly older than the current quote — no fabricated 0% change) (REQ-API-303, D3).
        seed_quote(
            &pool,
            &coin_recent_only,
            now - Duration::minutes(1),
            dec!(150),
        )
        .await;

        let server = TestServer::new(crate::api::build_api_router(test_state(pool.clone())));

        let resp = server.get("/v1/coins/quotes/latest").await;
        assert_eq!(resp.status_code(), 200);
        let body: Value = resp.json();
        let quotes = body["quotes"].as_array().expect("quotes array").clone();
        let find = |id: &str| quotes.iter().find(|r| r["coin_id"] == id).cloned();

        let a = find(&coin_current).expect("current coin present");
        assert_eq!(a["price"], "100", "current price is the newest quote");
        assert_eq!(
            a["open_24h"], "90",
            "open_24h is the earliest quote in the 24h window"
        );
        assert_eq!(a["vs_currency"], "usd", "default vs_currency is usd");

        assert!(
            find(&coin_stale).is_none(),
            "a coin with only stale (>48h) quotes must be omitted"
        );

        let c = find(&coin_nobaseline).expect("nobaseline coin present");
        assert_eq!(c["price"], "70");
        assert!(
            c["open_24h"].is_null(),
            "open_24h must be null (not 0) when no quote exists in the 24h window"
        );

        // Newly-tracked coin with only a recent quote: present with price, but open_24h null —
        // the current quote must NOT be reused as its own baseline (no fabricated 0% change).
        let r = find(&coin_recent_only).expect("recent-only coin present");
        assert_eq!(
            r["price"], "150",
            "recent-only coin shows its current price"
        );
        assert!(
            r["open_24h"].is_null(),
            "open_24h must be null (not the current price) when the only quote is the current one"
        );

        // Unrecognised vs_currency → HTTP 200 with the bare empty envelope (Scenarios 3/8).
        let empty = server
            .get("/v1/coins/quotes/latest?vs_currency=zzz-nonexistent")
            .await;
        assert_eq!(empty.status_code(), 200);
        assert_eq!(empty.text(), r#"{"quotes":[]}"#);

        // Cleanup (cascades to coin_quotes rows).
        for c in coins {
            sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
                .bind(c)
                .execute(&pool)
                .await
                .expect("cleanup");
        }
    }

    // SPEC-API-004 Scenario 10 (REQ-API-305) [DB-backed]: EXPLAIN (ANALYZE, BUFFERS) on the
    // overview query shows execution-time partition pruning and index scans, and does NOT
    // seq-scan the coin_quotes parent. now() is STABLE → runtime pruning ("Subplans Removed").
    #[tokio::test]
    #[ignore]
    async fn db_latest_quotes_overview_explain_prunes_partitions() {
        use chrono::Duration;
        use rust_decimal_macros::dec;

        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::db::connect(&url).await.expect("db connect");

        let sfx = chrono::Utc::now().timestamp_nanos_opt().unwrap();
        let coin_a = format!("sp4-explain-a-{sfx}");
        let coin_b = format!("sp4-explain-b-{sfx}");
        let coins = [&coin_a, &coin_b];
        let now = chrono::Utc::now();

        for c in coins {
            sqlx::query(
                "INSERT INTO tracked_coins (coin_id, symbol, name, status) \
                 VALUES ($1, 'T', 'Test', 'active')",
            )
            .bind(c)
            .execute(&pool)
            .await
            .expect("insert coin");
        }
        // Seed across two monthly partitions: recent (this month) + ~60 days ago.
        for (coin, ts, price) in [
            (&coin_a, now - Duration::hours(1), dec!(100)),
            (&coin_a, now - Duration::hours(23), dec!(90)),
            (&coin_a, now - Duration::days(60), dec!(80)),
            (&coin_b, now - Duration::hours(2), dec!(200)),
        ] {
            sqlx::query(
                "INSERT INTO coin_quotes (coin_id, vs_currency, ts, price, source) \
                 VALUES ($1, 'usd', $2, $3, 'test')",
            )
            .bind(coin)
            .bind(ts)
            .bind(price)
            .execute(&pool)
            .await
            .expect("insert quote");
        }

        // Force index usage so the per-LATERAL index-scan sub-assertion is planner-independent
        // on a lightly-seeded DB (Scenario 10 note). Applied to a dedicated connection only.
        let mut conn = pool.acquire().await.expect("acquire");
        sqlx::query("SET enable_seqscan = off")
            .execute(&mut *conn)
            .await
            .expect("set enable_seqscan");

        let plan_rows: Vec<(String,)> = sqlx::query_as(
            "EXPLAIN (ANALYZE, BUFFERS) \
             SELECT c.coin_id, q.vs_currency, q.ts, q.price, q.source, b.price AS open_24h \
             FROM tracked_coins c \
             CROSS JOIN LATERAL ( \
                 SELECT vs_currency, ts, price, source FROM coin_quotes \
                 WHERE coin_id = c.coin_id AND vs_currency = $1 \
                   AND ts >= now() - interval '48 hours' \
                 ORDER BY ts DESC LIMIT 1 \
             ) q \
             LEFT JOIN LATERAL ( \
                 SELECT price FROM coin_quotes \
                 WHERE coin_id = c.coin_id AND vs_currency = $1 \
                   AND ts >= now() - interval '24 hours' \
                   AND ts < q.ts \
                 ORDER BY ts ASC LIMIT 1 \
             ) b ON TRUE \
             WHERE c.status = 'active'",
        )
        .bind("usd")
        .fetch_all(&mut *conn)
        .await
        .expect("explain");

        let plan = plan_rows
            .into_iter()
            .map(|(l,)| l)
            .collect::<Vec<_>>()
            .join("\n");

        // REQ-API-305 primary guard: execution-time partition pruning applies.
        assert!(
            plan.contains("Subplans Removed"),
            "EXPLAIN must show execution-time partition pruning (Subplans Removed); got:\n{plan}"
        );
        // REQ-API-305 primary guard: no sequential scan hits the coin_quotes parent table.
        assert!(
            !plan.contains("Seq Scan on coin_quotes\n")
                && !plan.contains("Seq Scan on coin_quotes "),
            "plan must not seq-scan the coin_quotes parent; got:\n{plan}"
        );
        // Sub-assertion (forced via enable_seqscan=off): the LATERALs use the coin_quotes index.
        assert!(
            plan.contains("Index Scan") && plan.contains("coin_quotes"),
            "each LATERAL should use an index scan on coin_quotes; got:\n{plan}"
        );

        sqlx::query("SET enable_seqscan = on")
            .execute(&mut *conn)
            .await
            .ok();
        drop(conn);

        for c in coins {
            sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
                .bind(c)
                .execute(&pool)
                .await
                .expect("cleanup");
        }
    }
}
