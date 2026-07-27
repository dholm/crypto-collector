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
use serde::{Deserialize, Serialize};

use super::{
    cursor::{decode_keyset_cursor, encode_keyset_cursor, validate_limit, TsKey},
    dto::{CoinQuoteDto, CoinQuoteOverviewDto, CoinQuoteOverviewPage, Page},
    ApiError, ApiResult, AppState,
};

// ── Query parameter types ─────────────────────────────────────────────────────
//
// `Serialize` is derived alongside `Deserialize` so the per-operation parameter-parity test
// (F-59, src/api/mod.rs) can reflect the struct's serde field set and assert every documented
// query parameter has a matching field — the guard that would have caught F-29.

#[derive(Debug, Deserialize, Serialize)]
pub struct ListQuotesParams {
    /// Quote currency filter; defaults to `usd` (REQ-API-401). Not allow-list-validated —
    /// an unrecognised value matches no rows.
    pub vs_currency: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<i64>,
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
}

/// Query parameters for `GET /v1/coins/{coin_id}/quotes/latest` (F-29, REQ-API-400).
#[derive(Debug, Deserialize, Serialize)]
pub struct GetLatestQuoteParams {
    /// Quote currency filter; defaults to `usd` (REQ-API-400). Not allow-list-validated —
    /// an unrecognised value matches no rows (→ 404).
    pub vs_currency: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ListLatestQuotesParams {
    pub vs_currency: Option<String>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// `GET /v1/coins/{coin_id}/quotes/latest` — newest spot quote for a coin (REQ-API-400/402).
///
/// Filters by `vs_currency` (default `usd`) and bounds the read to a 48h trailing freshness
/// window: a coin whose newest quote is older than 48h yields no row → the existing
/// `None → NotFound` branch returns 404 ("no current quote"). The `ts >= now() - interval`
/// bound is what makes PostgreSQL prune partitions (REQ-API-402/404).
pub async fn get_latest_quote(
    State(state): State<AppState>,
    Path(coin_id): Path<String>,
    Query(params): Query<GetLatestQuoteParams>,
) -> ApiResult<impl IntoResponse> {
    let vs_currency = params.vs_currency.as_deref().unwrap_or("usd");

    ensure_coin_exists(&state.pool, &coin_id).await?;

    let quote: Option<crate::models::quote::CoinQuote> = sqlx::query_as(
        "SELECT coin_id, vs_currency, ts, price, source \
         FROM coin_quotes \
         WHERE coin_id = $1 \
           AND vs_currency = $2 \
           AND ts >= now() - interval '48 hours' \
         ORDER BY ts DESC \
         LIMIT 1",
    )
    .bind(&coin_id)
    .bind(vs_currency)
    .fetch_optional(&state.pool)
    .await?;

    match quote {
        Some(q) => Ok(Json(CoinQuoteDto::from(q)).into_response()),
        None => Err(ApiError::NotFound(format!(
            "no current quote found for coin '{coin_id}'"
        ))),
    }
}

/// `GET /v1/coins/{coin_id}/quotes` — keyset-paginated quote history (REQ-API-401/403).
///
/// Filters by `vs_currency` (default `usd`); single-currency filtering also removes the
/// duplicate-`ts`-across-currencies keyset row loss at page boundaries (the strict `ts <`
/// cursor no longer skips a co-timestamped row of another currency; REQ-API-401).
///
/// When NEITHER `start` NOR `cursor` is supplied, a default 48h trailing window applies
/// (D1/OR-API5-1): anchored on `end` when supplied (`[end-48h, end]`), else on `now()`
/// (`[now()-48h, now()]`). The default-window branch uses a bare `ts >=` predicate so
/// PostgreSQL prunes partitions (REQ-API-403/404). A supplied `start`/`cursor` takes the
/// keyset path, where that value defines the bound instead of the 48h default.
pub async fn list_quotes(
    State(state): State<AppState>,
    Path(coin_id): Path<String>,
    Query(params): Query<ListQuotesParams>,
) -> ApiResult<impl IntoResponse> {
    let limit = validate_limit(params.limit).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let vs_currency = params.vs_currency.as_deref().unwrap_or("usd");

    let cursor_ts: Option<DateTime<Utc>> = params
        .cursor
        .as_deref()
        .map(|c| decode_keyset_cursor::<TsKey>(c).map(|k| k.ts))
        .transpose()
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    ensure_coin_exists(&state.pool, &coin_id).await?;

    let items: Vec<crate::models::quote::CoinQuote> =
        if params.start.is_none() && cursor_ts.is_none() {
            // Default 48h trailing window (D1/OR-API5-1): lower bound is a bare `ts >=` expression
            // anchored on `end` when supplied, else `now()` — so PostgreSQL prunes partitions
            // (REQ-API-403/404).
            sqlx::query_as(
                "SELECT coin_id, vs_currency, ts, price, source \
             FROM coin_quotes \
             WHERE coin_id = $1 \
               AND vs_currency = $2 \
               AND ($3::TIMESTAMPTZ IS NULL OR ts <= $3) \
               AND ts >= COALESCE($3, now()) - interval '48 hours' \
             ORDER BY ts DESC \
             LIMIT $4",
            )
            .bind(&coin_id)
            .bind(vs_currency)
            .bind(params.end)
            .bind(limit + 1)
            .fetch_all(&state.pool)
            .await?
        } else {
            // Keyset path: a supplied `start`/`cursor` defines the bound (no default window).
            sqlx::query_as(
                "SELECT coin_id, vs_currency, ts, price, source \
             FROM coin_quotes \
             WHERE coin_id = $1 \
               AND vs_currency = $2 \
               AND ($3::TIMESTAMPTZ IS NULL OR ts <= $3) \
               AND ($4::TIMESTAMPTZ IS NULL OR ts >= $4) \
               AND ($5::TIMESTAMPTZ IS NULL OR ts < $5) \
             ORDER BY ts DESC \
             LIMIT $6",
            )
            .bind(&coin_id)
            .bind(vs_currency)
            .bind(params.end)
            .bind(params.start)
            .bind(cursor_ts)
            .bind(limit + 1)
            .fetch_all(&state.pool)
            .await?
        };

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

    // @MX:ANCHOR: [AUTO] coin_quotes ts-bound invariant — EVERY coin_quotes read in src/api is ts-bounded
    // @MX:REASON: coin_quotes is PARTITION BY RANGE(ts) with 48 monthly partitions. The invariant
    //             covers all THREE readers in this module — get_latest_quote and list_quotes (the
    //             48h/default-window bounds, F-30) AND this all-coin overview: each carries a
    //             `ts >= now() - interval` (or `ts >= COALESCE(end, now()) - interval`) lower bound
    //             so PostgreSQL prunes partitions at execution time (now() is STABLE → runtime
    //             pruning, PG11+). No coin_quotes read is exempt (REQ-API-404, D1).
    // @MX:WARN: NEVER remove the `ts >=` lower bound from ANY coin_quotes read (this LATERAL pair,
    //           get_latest_quote, or list_quotes). An unbounded DISTINCT ON / parent scan touches
    //           all 48 partitions — a sibling service shipped that shape and produced a 41s query
    //           that blew a 30s client timeout (REQ-API-305/404, D7 of SPEC-API-004 / D1 of
    //           SPEC-API-005).
    // @MX:SPEC: SPEC-API-004 REQ-API-305 SPEC-API-005 REQ-API-404
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

    // ── SPEC-API-005 M1 (F-29/F-30) ─────────────────────────────────────────────

    // Non-DB: the F-29 vs_currency field deserializes on both quote-read param structs.
    #[test]
    fn quote_params_accept_vs_currency() {
        let lq: ListQuotesParams =
            serde_json::from_value(serde_json::json!({ "vs_currency": "eur", "limit": 10 }))
                .expect("list params");
        assert_eq!(lq.vs_currency.as_deref(), Some("eur"));
        let gl: GetLatestQuoteParams =
            serde_json::from_value(serde_json::json!({ "vs_currency": "eur" }))
                .expect("latest params");
        assert_eq!(gl.vs_currency.as_deref(), Some("eur"));
        // Absent → None → resolves to "usd" default in the handler.
        let gl_default: GetLatestQuoteParams =
            serde_json::from_value(serde_json::json!({})).expect("latest params default");
        assert!(gl_default.vs_currency.is_none());
    }

    // DB-gated seed helpers (FK discipline: seed parent tracked_coins first; teardown cascades).
    #[cfg(test)]
    async fn seed_coin_active(pool: &sqlx::PgPool, coin_id: &str) {
        sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
            .bind(coin_id)
            .execute(pool)
            .await
            .expect("pre-clean");
        sqlx::query(
            "INSERT INTO tracked_coins (coin_id, symbol, name, status) \
             VALUES ($1, 'T', 'Test', 'active')",
        )
        .bind(coin_id)
        .execute(pool)
        .await
        .expect("seed tracked_coins");
    }

    #[cfg(test)]
    async fn seed_quote_cur(
        pool: &sqlx::PgPool,
        coin_id: &str,
        vs_currency: &str,
        ts: DateTime<Utc>,
        price_str: &str,
    ) {
        use rust_decimal::Decimal;
        use std::str::FromStr;
        sqlx::query(
            "INSERT INTO coin_quotes (coin_id, vs_currency, ts, price, source) \
             VALUES ($1, $2, $3, $4, 'test')",
        )
        .bind(coin_id)
        .bind(vs_currency)
        .bind(ts)
        .bind(Decimal::from_str(price_str).unwrap())
        .execute(pool)
        .await
        .expect("seed coin_quotes");
    }

    // AC-API-400 [DB-backed]: vs_currency default usd, explicit eur, unrecognised → 404 (no allow-list).
    #[tokio::test]
    #[ignore]
    async fn db_get_latest_quote_vs_currency_default_explicit_and_unknown() {
        use axum_test::TestServer;
        use chrono::Duration;
        use serde_json::Value;

        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::db::connect(&url).await.expect("db connect");
        let sfx = chrono::Utc::now().timestamp_nanos_opt().unwrap();
        let coin = format!("api5-vscur-{sfx}");
        seed_coin_active(&pool, &coin).await;
        let now = chrono::Utc::now();
        // Recent (< 48h) rows in both currencies.
        seed_quote_cur(&pool, &coin, "usd", now - Duration::hours(1), "100").await;
        seed_quote_cur(&pool, &coin, "eur", now - Duration::hours(1), "90").await;

        let server = TestServer::new(crate::api::build_api_router(test_state(pool.clone())));

        // No vs_currency → usd default.
        let r = server.get(&format!("/v1/coins/{coin}/quotes/latest")).await;
        assert_eq!(r.status_code(), 200);
        let b: Value = r.json();
        assert_eq!(b["vs_currency"], "usd");
        assert_eq!(b["price"], "100");

        // Explicit eur.
        let r = server
            .get(&format!("/v1/coins/{coin}/quotes/latest?vs_currency=eur"))
            .await;
        assert_eq!(r.status_code(), 200);
        let b: Value = r.json();
        assert_eq!(b["vs_currency"], "eur");
        assert_eq!(b["price"], "90");

        // Unrecognised currency matches no rows → 404 (never 400 — no allow-list).
        let r = server
            .get(&format!("/v1/coins/{coin}/quotes/latest?vs_currency=zzz"))
            .await;
        assert_eq!(r.status_code(), 404);

        sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
            .bind(&coin)
            .execute(&pool)
            .await
            .ok();
    }

    // AC-API-401 [DB-backed]: single-currency filter removes the duplicate-ts keyset row loss.
    #[tokio::test]
    #[ignore]
    async fn db_list_quotes_duplicate_ts_across_currencies_no_row_loss() {
        use axum_test::TestServer;
        use chrono::Duration;
        use serde_json::Value;

        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::db::connect(&url).await.expect("db connect");
        let sfx = chrono::Utc::now().timestamp_nanos_opt().unwrap();
        let coin = format!("api5-dupts-{sfx}");
        seed_coin_active(&pool, &coin).await;
        let now = chrono::Utc::now();
        // usd rows at t1 > t2 > t3; an eur row co-timestamped with the middle usd row (t2).
        let t1 = now - Duration::hours(1);
        let t2 = now - Duration::hours(2);
        let t3 = now - Duration::hours(3);
        seed_quote_cur(&pool, &coin, "usd", t1, "101").await;
        seed_quote_cur(&pool, &coin, "usd", t2, "102").await;
        seed_quote_cur(&pool, &coin, "usd", t3, "103").await;
        seed_quote_cur(&pool, &coin, "eur", t2, "999").await; // co-timestamped decoy

        let server = TestServer::new(crate::api::build_api_router(test_state(pool.clone())));

        // Page 1 (limit=1): newest usd row @ t1, with a continuation cursor.
        let r1 = server
            .get(&format!("/v1/coins/{coin}/quotes?vs_currency=usd&limit=1"))
            .await;
        assert_eq!(r1.status_code(), 200);
        let b1: Value = r1.json();
        assert_eq!(b1["items"][0]["price"], "101");
        assert_eq!(b1["items"][0]["vs_currency"], "usd");
        let cursor = b1["next_cursor"]
            .as_str()
            .expect("cursor present")
            .to_string();

        // Page 2: the strict `ts < cursor` cursor must NOT skip the usd row co-timestamped with
        // the eur decoy — vs_currency filtering returns usd @ t2 (price 102), not the eur decoy.
        let r2 = server
            .get(&format!(
                "/v1/coins/{coin}/quotes?vs_currency=usd&limit=1&cursor={cursor}"
            ))
            .await;
        assert_eq!(r2.status_code(), 200);
        let b2: Value = r2.json();
        assert_eq!(
            b2["items"][0]["price"], "102",
            "the usd row co-timestamped with an eur row must not be skipped (REQ-API-401)"
        );
        assert_eq!(b2["items"][0]["vs_currency"], "usd");

        sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
            .bind(&coin)
            .execute(&pool)
            .await
            .ok();
    }

    // AC-API-402 [DB-backed]: 48h trailing bound; a coin whose newest quote is >48h old → 404.
    #[tokio::test]
    #[ignore]
    async fn db_get_latest_quote_stale_returns_404_fresh_returns_200() {
        use axum_test::TestServer;
        use chrono::Duration;

        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::db::connect(&url).await.expect("db connect");
        let sfx = chrono::Utc::now().timestamp_nanos_opt().unwrap();
        let stale = format!("api5-stale-{sfx}");
        let fresh = format!("api5-fresh-{sfx}");
        seed_coin_active(&pool, &stale).await;
        seed_coin_active(&pool, &fresh).await;
        let now = chrono::Utc::now();
        // stale: only quote is 49h old (past the 48h boundary) → 404.
        seed_quote_cur(&pool, &stale, "usd", now - Duration::hours(49), "50").await;
        // fresh: quote 47h old (inside the 48h boundary) → 200.
        seed_quote_cur(&pool, &fresh, "usd", now - Duration::hours(47), "60").await;

        let server = TestServer::new(crate::api::build_api_router(test_state(pool.clone())));

        let r_stale = server
            .get(&format!("/v1/coins/{stale}/quotes/latest"))
            .await;
        assert_eq!(
            r_stale.status_code(),
            404,
            "a coin whose only quote is >48h old has no current quote (REQ-API-402)"
        );

        let r_fresh = server
            .get(&format!("/v1/coins/{fresh}/quotes/latest"))
            .await;
        assert_eq!(r_fresh.status_code(), 200);

        for c in [&stale, &fresh] {
            sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
                .bind(c)
                .execute(&pool)
                .await
                .ok();
        }
    }

    // AC-API-403 [DB-backed]: default 48h window when no start/cursor; explicit start overrides it.
    #[tokio::test]
    #[ignore]
    async fn db_list_quotes_default_48h_window_and_explicit_start() {
        use axum_test::TestServer;
        use chrono::Duration;
        use serde_json::Value;

        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::db::connect(&url).await.expect("db connect");
        let sfx = chrono::Utc::now().timestamp_nanos_opt().unwrap();
        let coin = format!("api5-window-{sfx}");
        seed_coin_active(&pool, &coin).await;
        let now = chrono::Utc::now();
        seed_quote_cur(&pool, &coin, "usd", now - Duration::hours(1), "100").await; // inside 48h
        seed_quote_cur(&pool, &coin, "usd", now - Duration::hours(49), "80").await; // older than 48h

        let server = TestServer::new(crate::api::build_api_router(test_state(pool.clone())));

        // No start/cursor → default 48h window → only the inside-window row.
        let r = server.get(&format!("/v1/coins/{coin}/quotes")).await;
        assert_eq!(r.status_code(), 200);
        let b: Value = r.json();
        let items = b["items"].as_array().unwrap();
        assert_eq!(
            items.len(),
            1,
            "default window returns only rows within 48h"
        );
        assert_eq!(items[0]["price"], "100");

        // Explicit start reaching back past 48h → both rows returned (start defines the bound).
        let start = (now - Duration::hours(72)).to_rfc3339();
        let r = server
            .get(&format!("/v1/coins/{coin}/quotes?start={start}"))
            .await;
        assert_eq!(r.status_code(), 200);
        let b: Value = r.json();
        assert_eq!(
            b["items"].as_array().unwrap().len(),
            2,
            "an explicit start defines the lower bound instead of the 48h default (REQ-API-403)"
        );

        sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
            .bind(&coin)
            .execute(&pool)
            .await
            .ok();
    }

    // AC-API-404 [DB-backed]: get_latest_quote and the default-window list_quotes prune partitions
    // and never seq-scan the coin_quotes parent. Mirrors the overview EXPLAIN guard.
    #[tokio::test]
    #[ignore]
    async fn db_quote_reads_explain_prune_partitions() {
        use chrono::Duration;

        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::db::connect(&url).await.expect("db connect");
        let sfx = chrono::Utc::now().timestamp_nanos_opt().unwrap();
        let coin = format!("api5-explain-{sfx}");
        seed_coin_active(&pool, &coin).await;
        let now = chrono::Utc::now();
        // Seed across two monthly partitions so pruning has something to remove.
        seed_quote_cur(&pool, &coin, "usd", now - Duration::hours(1), "100").await;
        seed_quote_cur(&pool, &coin, "usd", now - Duration::days(60), "80").await;

        let mut conn = pool.acquire().await.expect("acquire");
        sqlx::query("SET enable_seqscan = off")
            .execute(&mut *conn)
            .await
            .expect("set enable_seqscan");

        // get_latest_quote query shape.
        let latest_plan: Vec<(String,)> = sqlx::query_as(
            "EXPLAIN (ANALYZE, BUFFERS) \
             SELECT coin_id, vs_currency, ts, price, source \
             FROM coin_quotes \
             WHERE coin_id = $1 AND vs_currency = $2 \
               AND ts >= now() - interval '48 hours' \
             ORDER BY ts DESC LIMIT 1",
        )
        .bind(&coin)
        .bind("usd")
        .fetch_all(&mut *conn)
        .await
        .expect("explain latest");
        let latest = latest_plan
            .into_iter()
            .map(|(l,)| l)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            latest.contains("Subplans Removed"),
            "get_latest_quote must prune partitions (Subplans Removed); got:\n{latest}"
        );
        assert!(
            !latest.contains("Seq Scan on coin_quotes\n")
                && !latest.contains("Seq Scan on coin_quotes "),
            "get_latest_quote must not seq-scan the coin_quotes parent; got:\n{latest}"
        );

        // list_quotes default-window query shape (no start/cursor, end NULL).
        let list_plan: Vec<(String,)> = sqlx::query_as(
            "EXPLAIN (ANALYZE, BUFFERS) \
             SELECT coin_id, vs_currency, ts, price, source \
             FROM coin_quotes \
             WHERE coin_id = $1 AND vs_currency = $2 \
               AND ($3::TIMESTAMPTZ IS NULL OR ts <= $3) \
               AND ts >= COALESCE($3, now()) - interval '48 hours' \
             ORDER BY ts DESC LIMIT 100",
        )
        .bind(&coin)
        .bind("usd")
        .bind(Option::<DateTime<Utc>>::None)
        .fetch_all(&mut *conn)
        .await
        .expect("explain list");
        let list = list_plan
            .into_iter()
            .map(|(l,)| l)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            list.contains("Subplans Removed"),
            "default-window list_quotes must prune partitions (Subplans Removed); got:\n{list}"
        );
        assert!(
            !list.contains("Seq Scan on coin_quotes\n")
                && !list.contains("Seq Scan on coin_quotes "),
            "default-window list_quotes must not seq-scan the coin_quotes parent; got:\n{list}"
        );

        sqlx::query("SET enable_seqscan = on")
            .execute(&mut *conn)
            .await
            .ok();
        drop(conn);
        sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
            .bind(&coin)
            .execute(&pool)
            .await
            .ok();
    }
}
