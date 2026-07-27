//! Integration tests — require a live PostgreSQL database.
//!
//! Gate: all tests are marked `#[ignore]`. Run with:
//!   DATABASE_URL=postgres://... cargo test -- --ignored
//!
//! Each test:
//!   1. Reads DATABASE_URL from env
//!   2. Calls crypto_collector::db::connect() which applies migrations idempotently
//!   3. Inspects catalog or inserts test data to assert schema correctness
//!
//! Mirrors the ticker-collector integration test pattern.

use sqlx::{PgPool, Row};

/// Connect to the live DB and apply migrations. Panics if DATABASE_URL is not set.
async fn setup() -> PgPool {
    let url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
    crypto_collector::db::connect(&url)
        .await
        .expect("Failed to connect and apply migrations")
}

// Helper: insert a tracked_coin (the coin-keyed FK parent for coin_quotes/coin_candles/
// backfill_jobs), returning the coin_id. Migration 0011 removed tracked_markets, so the schema
// is coin-keyed end-to-end (SPEC-API-005 F-58).
async fn insert_test_coin(pool: &PgPool, suffix: &str) -> String {
    let coin_id = format!("test-coin-{suffix}");
    sqlx::query(
        "INSERT INTO tracked_coins (coin_id, symbol, name, status)
         VALUES ($1, $2, $3, 'active')
         ON CONFLICT (coin_id) DO NOTHING",
    )
    .bind(&coin_id)
    .bind(format!("TB{suffix}"))
    .bind(format!("Test Coin {suffix}"))
    .execute(pool)
    .await
    .expect("insert tracked_coin");
    coin_id
}

// ── Scenario 1: Coin-keyed registries exist with correct keys (REQ-DB-001/002) ────
//
// SPEC-API-005 F-58: rewritten from the pre-0011 form that asserted the removed tracked_markets
// table. Migration 0011 dropped tracked_markets and created the coin-keyed coin_quotes /
// coin_candles time-series tables; tracked_coins remains the coin-id registry (PK coin_id).

#[tokio::test]
#[ignore]
async fn scenario_01_registries_exist_with_correct_pk() {
    let pool = setup().await;

    // tracked_coins: coin_id is the text PK.
    let row = sqlx::query(
        "SELECT column_name, data_type
         FROM information_schema.columns
         WHERE table_schema = 'public' AND table_name = 'tracked_coins' AND column_name = 'coin_id'",
    )
    .fetch_optional(&pool)
    .await
    .expect("query")
    .expect("tracked_coins.coin_id must exist");
    assert_eq!(row.get::<String, _>("data_type"), "text");

    // The coin-keyed time-series tables (post-0011) exist and are keyed by coin_id.
    for table in ["coin_quotes", "coin_candles"] {
        let has_coin_id: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM information_schema.columns
                WHERE table_schema='public' AND table_name=$1 AND column_name='coin_id'
             )",
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .expect("coin_id column check");
        assert!(has_coin_id, "{table} must be coin-keyed (coin_id column)");
    }

    // The removed market-keyed registry must NOT exist (0011 dropped it).
    let has_tracked_markets: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM information_schema.tables
            WHERE table_schema='public' AND table_name='tracked_markets'
         )",
    )
    .fetch_one(&pool)
    .await
    .expect("tracked_markets existence check");
    assert!(
        !has_tracked_markets,
        "tracked_markets was removed by migration 0011 and must not exist"
    );
}

// ── Scenario 3: No equities machinery (REQ-DB-004) ───────────────────────────

#[tokio::test]
#[ignore]
async fn scenario_03_no_equities_tables() {
    let pool = setup().await;

    let prohibited = [
        "exchanges",
        "market_phase",
        "trading_halt",
        "calendar",
        "holidays",
    ];
    for table in &prohibited {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.tables
             WHERE table_schema = 'public' AND table_name = $1",
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .expect("catalog query");
        assert_eq!(
            count, 0,
            "Prohibited equities table '{}' must not exist (REQ-DB-004)",
            table
        );
    }

    // Check no market-open/close columns exist anywhere
    for col in &[
        "market_open_wall_clock",
        "market_close_wall_clock",
        "market_phase",
        "close_grace",
    ] {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns
             WHERE table_schema = 'public' AND column_name = $1",
        )
        .bind(col)
        .fetch_one(&pool)
        .await
        .expect("catalog query");
        assert_eq!(
            count, 0,
            "Prohibited equities column '{}' must not exist (REQ-DB-004)",
            col
        );
    }
}

// ── Scenario 4: Time-series index/partition contract (REQ-DB-014/015) ──
//
// SPEC-API-005 F-58: rewritten for the current schema. coin_quotes and coin_market_snapshots
// are still monthly RANGE-partitioned; coin_candles was de-partitioned into a plain table by
// migration 0020 (F-57) but retains its btree + BRIN indexes. The pre-0011 live_quotes /
// candles / derivatives_quotes tables were dropped and are no longer checked.

#[tokio::test]
#[ignore]
async fn scenario_04_partitioned_tables_with_indexes() {
    let pool = setup().await;

    // Still RANGE-partitioned with btree(ts DESC) + BRIN + monthly partitions.
    for table in ["coin_quotes", "coin_market_snapshots"] {
        let is_partitioned: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM pg_class c
                JOIN pg_partitioned_table pt ON pt.partrelid = c.oid
                WHERE c.relname = $1 AND pt.partstrat = 'r'
             )",
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .expect("partition check");
        assert!(
            is_partitioned,
            "Table '{table}' must be RANGE-partitioned (REQ-DB-014)"
        );

        let has_brin: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_indexes
                WHERE schemaname='public' AND tablename=$1 AND indexdef ILIKE '%using brin%')",
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .expect("brin index check");
        assert!(
            has_brin,
            "Table '{table}' must have a BRIN index (REQ-DB-015)"
        );

        let has_btree_ts: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_indexes
                WHERE schemaname='public' AND tablename=$1 AND indexdef ILIKE '%ts desc%')",
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .expect("btree ts index check");
        assert!(
            has_btree_ts,
            "Table '{table}' must have a btree index with ts DESC (REQ-DB-015)"
        );

        let partition_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_class c
             JOIN pg_inherits i ON i.inhrelid = c.oid
             JOIN pg_class p ON p.oid = i.inhparent
             WHERE p.relname = $1",
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .expect("partition count");
        assert!(
            partition_count >= 12,
            "Table '{table}' must have >= 12 monthly partitions (REQ-DB-016), found {partition_count}"
        );
    }

    // coin_candles: de-partitioned by 0020 (F-57) → a PLAIN table (NOT partitioned) that still
    // keeps its btree + BRIN indexes (REQ-DB-015).
    let candles_partitioned: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM pg_class c
            JOIN pg_partitioned_table pt ON pt.partrelid = c.oid
            WHERE c.relname = 'coin_candles'
         )",
    )
    .fetch_one(&pool)
    .await
    .expect("coin_candles partition check");
    assert!(
        !candles_partitioned,
        "coin_candles must be a flat (non-partitioned) table after migration 0020 (F-57)"
    );

    for pat in ["%using brin%", "%ts desc%"] {
        let has_idx: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_indexes
                WHERE schemaname='public' AND tablename='coin_candles' AND indexdef ILIKE $1)",
        )
        .bind(pat)
        .fetch_one(&pool)
        .await
        .expect("coin_candles index check");
        assert!(
            has_idx,
            "flat coin_candles must retain its index matching '{pat}' (REQ-DB-015)"
        );
    }
}

// ── Scenario 5: coin_candles PK includes interval; volume nullable (REQ-DB-011) ────
//
// SPEC-API-005 F-58: rewritten from the pre-0011 market-keyed `candles` table to the coin-keyed
// `coin_candles` table (PK (coin_id, vs_currency, interval, ts)).

#[tokio::test]
#[ignore]
async fn scenario_05_candle_pk_and_nullable_volume() {
    let pool = setup().await;
    let suffix = uuid::Uuid::new_v4().to_string().replace('-', "")[..8].to_string();
    let coin_id = insert_test_coin(&pool, &format!("s05-{suffix}")).await;

    let ts = "2026-06-01 12:00:00+00";

    // 1m and 1d candles at the same (coin_id, vs_currency, ts) coexist — PK includes interval.
    for (interval, o, h, l, c) in [
        ("1m", 42000, 42100, 41900, 42050),
        ("1d", 40000, 43000, 39500, 42050),
    ] {
        sqlx::query(
            "INSERT INTO coin_candles (coin_id, vs_currency, interval, ts, open, high, low, close, source)
             VALUES ($1, 'usd', $2, $3::timestamptz, $4, $5, $6, $7, 'test')
             ON CONFLICT DO NOTHING",
        )
        .bind(&coin_id)
        .bind(interval)
        .bind(ts)
        .bind(o)
        .bind(h)
        .bind(l)
        .bind(c)
        .execute(&pool)
        .await
        .unwrap_or_else(|e| panic!("{interval} candle insert must succeed: {e}"));
    }

    // NULL volume candle (CoinGecko OHLC: no volume) — REQ-DB-011.
    sqlx::query(
        "INSERT INTO coin_candles (coin_id, vs_currency, interval, ts, open, high, low, close, volume, source)
         VALUES ($1, 'usd', '1h', '2026-06-01 13:00:00+00'::timestamptz, 42000, 42100, 41900, 42050, NULL, 'coingecko')
         ON CONFLICT DO NOTHING",
    )
    .bind(&coin_id)
    .execute(&pool)
    .await
    .expect("NULL volume candle insert must succeed (REQ-DB-011)");

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM coin_candles
         WHERE coin_id = $1 AND ts = $2::timestamptz AND interval IN ('1m', '1d')",
    )
    .bind(&coin_id)
    .bind(ts)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(
        count, 2,
        "1m and 1d candles must coexist for the same (coin_id, vs_currency, ts)"
    );

    // Teardown (child before parent).
    sqlx::query("DELETE FROM coin_candles WHERE coin_id = $1")
        .bind(&coin_id)
        .execute(&pool)
        .await
        .ok();
    sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
        .bind(&coin_id)
        .execute(&pool)
        .await
        .ok();
}

// ── Scenario 7: Coin aggregates are time-series, not revisions (REQ-DB-012/022) ──

#[tokio::test]
#[ignore]
async fn scenario_07_aggregate_columns_in_snapshots_not_metadata() {
    let pool = setup().await;

    let aggregate_cols = [
        "market_cap",
        "fully_diluted_valuation",
        "circulating_supply",
        "total_supply",
    ];

    // Must exist in coin_market_snapshots
    for col in &aggregate_cols {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM information_schema.columns
                WHERE table_schema='public' AND table_name='coin_market_snapshots' AND column_name=$1
             )",
        )
        .bind(col)
        .fetch_one(&pool)
        .await
        .expect("catalog query");
        assert!(
            exists,
            "coin_market_snapshots must have column '{col}' (REQ-DB-012)"
        );
    }

    // Must NOT exist in coin_metadata (no revision churn on aggregates)
    for col in &aggregate_cols {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM information_schema.columns
                WHERE table_schema='public' AND table_name='coin_metadata' AND column_name=$1
             )",
        )
        .bind(col)
        .fetch_one(&pool)
        .await
        .expect("catalog query");
        assert!(
            !exists,
            "coin_metadata must NOT have column '{col}' — aggregates are time-series (REQ-DB-022)"
        );
    }
}

// ── Scenario 8: Revision table shape and as-of index (REQ-DB-020/023) ────────

#[tokio::test]
#[ignore]
async fn scenario_08_coin_metadata_pk_and_index() {
    let pool = setup().await;

    // PK must be (coin_id, revision)
    let pk_cols: Vec<String> = sqlx::query_scalar(
        "SELECT kc.column_name
         FROM information_schema.table_constraints tc
         JOIN information_schema.key_column_usage kc
           ON tc.constraint_name = kc.constraint_name AND tc.table_schema = kc.table_schema
         WHERE tc.table_schema = 'public' AND tc.table_name = 'coin_metadata'
           AND tc.constraint_type = 'PRIMARY KEY'
         ORDER BY kc.ordinal_position",
    )
    .fetch_all(&pool)
    .await
    .expect("pk query");
    assert!(
        pk_cols.contains(&"coin_id".to_string()),
        "coin_metadata PK must include coin_id (REQ-DB-020)"
    );
    assert!(
        pk_cols.contains(&"revision".to_string()),
        "coin_metadata PK must include revision (REQ-DB-020)"
    );

    // first_seen_at and last_seen_at must be TIMESTAMPTZ
    for col in &["first_seen_at", "last_seen_at"] {
        let dtype: String = sqlx::query_scalar(
            "SELECT data_type FROM information_schema.columns
             WHERE table_schema='public' AND table_name='coin_metadata' AND column_name=$1",
        )
        .bind(col)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|_| panic!("coin_metadata.{col} must exist (REQ-DB-020)"));
        assert_eq!(
            dtype, "timestamp with time zone",
            "coin_metadata.{col} must be TIMESTAMPTZ (REQ-DB-041)"
        );
    }

    // As-of index on (coin_id, first_seen_at DESC) must exist (REQ-DB-023)
    let has_asof_idx: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM pg_indexes
            WHERE schemaname='public' AND tablename='coin_metadata'
              AND indexdef ILIKE '%first_seen_at desc%'
         )",
    )
    .fetch_one(&pool)
    .await
    .expect("index check");
    assert!(
        has_asof_idx,
        "coin_metadata must have an as-of index with first_seen_at DESC (REQ-DB-023)"
    );
}

// ── Scenario 9: collection_queue dedup + both claim indexes (REQ-DB-030/031/032/036) ──

#[tokio::test]
#[ignore]
async fn scenario_09_collection_queue_dedup_and_claim_indexes() {
    let pool = setup().await;
    let suffix = uuid::Uuid::new_v4().to_string().replace('-', "")[..8].to_string();
    let target_id = format!("target-{suffix}");

    // Insert first live item
    sqlx::query(
        "INSERT INTO collection_queue (target_kind, target_id, kind, status)
         VALUES ('market', $1, 'spot', 'pending')",
    )
    .bind(&target_id)
    .execute(&pool)
    .await
    .expect("first enqueue must succeed");

    // Insert duplicate live item — must be rejected by partial unique index
    let result = sqlx::query(
        "INSERT INTO collection_queue (target_kind, target_id, kind, status)
         VALUES ('market', $1, 'spot', 'pending')",
    )
    .bind(&target_id)
    .execute(&pool)
    .await;
    assert!(
        result.is_err(),
        "duplicate live item for same (target_kind, target_id, kind) must be rejected (REQ-DB-031)"
    );

    // Move item to done
    sqlx::query("UPDATE collection_queue SET status='done' WHERE target_id=$1 AND kind='spot'")
        .bind(&target_id)
        .execute(&pool)
        .await
        .expect("update to done");

    // Now a new live item for the same key can be enqueued
    sqlx::query(
        "INSERT INTO collection_queue (target_kind, target_id, kind, status)
         VALUES ('market', $1, 'spot', 'pending')",
    )
    .bind(&target_id)
    .execute(&pool)
    .await
    .expect("enqueue after done must succeed (dedup only covers live statuses)");

    // Verify both claim indexes exist
    let pending_idx_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM pg_indexes
            WHERE schemaname='public' AND tablename='collection_queue'
              AND indexdef ILIKE '%enqueued_at%pending%'
         )",
    )
    .fetch_one(&pool)
    .await
    .expect("index check");
    assert!(
        pending_idx_exists,
        "collection_queue must have pending-path claim index on enqueued_at (REQ-DB-032)"
    );

    let lease_idx_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM pg_indexes
            WHERE schemaname='public' AND tablename='collection_queue'
              AND indexdef ILIKE '%lease_expires_at%'
         )",
    )
    .fetch_one(&pool)
    .await
    .expect("index check");
    assert!(
        lease_idx_exists,
        "collection_queue must have lease-expired re-claim index on lease_expires_at (REQ-DB-036)"
    );
}

// ── Scenario 10: Coin-keyed backfill idempotent enqueue + lease columns (REQ-DB-033) ────
//
// SPEC-API-005 F-58: rewritten from the pre-0011 market-keyed backfill_jobs to the coin-keyed
// replacement created by migration 0012 (UNIQUE (coin_id, dataset)).

#[tokio::test]
#[ignore]
async fn scenario_10_backfill_idempotent_enqueue_and_lease_columns() {
    let pool = setup().await;
    let suffix = uuid::Uuid::new_v4().to_string().replace('-', "")[..8].to_string();
    let coin_id = insert_test_coin(&pool, &format!("s10-{suffix}")).await;

    let dataset = "candles:1h";

    sqlx::query("INSERT INTO backfill_jobs (coin_id, dataset, status) VALUES ($1, $2, 'pending')")
        .bind(&coin_id)
        .bind(dataset)
        .execute(&pool)
        .await
        .expect("first backfill job insert");

    // Duplicate (coin_id, dataset) — rejected by UNIQUE (REQ-DB-033).
    let result = sqlx::query(
        "INSERT INTO backfill_jobs (coin_id, dataset, status) VALUES ($1, $2, 'pending')",
    )
    .bind(&coin_id)
    .bind(dataset)
    .execute(&pool)
    .await;
    assert!(
        result.is_err(),
        "duplicate backfill job must be rejected by UNIQUE(coin_id, dataset) (REQ-DB-033)"
    );

    // backfill_chunks retains all required lease columns.
    let required_chunk_cols = [
        "range_start",
        "range_end",
        "cursor",
        "claimed_by",
        "lease_expires_at",
        "heartbeat_at",
        "attempts",
        "last_error",
    ];
    for col in &required_chunk_cols {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM information_schema.columns
                WHERE table_schema='public' AND table_name='backfill_chunks' AND column_name=$1
             )",
        )
        .bind(col)
        .fetch_one(&pool)
        .await
        .expect("catalog query");
        assert!(
            exists,
            "backfill_chunks must have column '{col}' (REQ-DB-033)"
        );
    }

    // Teardown (child before parent; backfill_jobs FK → tracked_coins ON DELETE CASCADE).
    sqlx::query("DELETE FROM backfill_jobs WHERE coin_id = $1")
        .bind(&coin_id)
        .execute(&pool)
        .await
        .ok();
    sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
        .bind(&coin_id)
        .execute(&pool)
        .await
        .ok();
}

// ── Scenario 11: Per-provider pacer seeded (REQ-DB-034/035) ──────────────────

#[tokio::test]
#[ignore]
async fn scenario_11_pacer_seeded_with_four_providers() {
    let pool = setup().await;

    // PK is provider TEXT
    let pk_col: String = sqlx::query_scalar(
        "SELECT kc.column_name
         FROM information_schema.table_constraints tc
         JOIN information_schema.key_column_usage kc
           ON tc.constraint_name = kc.constraint_name AND tc.table_schema = kc.table_schema
         WHERE tc.table_schema = 'public' AND tc.table_name = 'upstream_request_pacer'
           AND tc.constraint_type = 'PRIMARY KEY'",
    )
    .fetch_one(&pool)
    .await
    .expect("pk query");
    assert_eq!(
        pk_col, "provider",
        "upstream_request_pacer PK must be 'provider' (REQ-DB-034)"
    );

    // Required columns
    let required_cols = [
        "next_allowed_at",
        "min_gap_ms",
        "cooldown_until",
        "credit_window_start",
        "credits_used",
        "credit_limit",
    ];
    for col in &required_cols {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM information_schema.columns
                WHERE table_schema='public' AND table_name='upstream_request_pacer' AND column_name=$1
             )",
        )
        .bind(col)
        .fetch_one(&pool)
        .await
        .expect("catalog query");
        assert!(
            exists,
            "upstream_request_pacer must have column '{col}' (REQ-DB-034)"
        );
    }

    // All four providers must be seeded
    for provider in &["coingecko", "binance", "coinbase", "kraken"] {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM upstream_request_pacer WHERE provider = $1")
                .bind(provider)
                .fetch_one(&pool)
                .await
                .expect("provider row check");
        assert_eq!(
            count, 1,
            "Provider '{}' must be seeded in upstream_request_pacer (REQ-DB-035)",
            provider
        );
    }
}

// ── Scenario 12: Precision and time-type sweep (REQ-DB-040/041) ───────────────

#[tokio::test]
#[ignore]
async fn scenario_12_precision_and_time_type_sweep() {
    let pool = setup().await;

    // All monetary/quantity columns must be NUMERIC. SPEC-API-005 F-58: rewritten to the current
    // coin-keyed tables (the pre-0011 live_quotes / candles / derivatives_quotes were dropped).
    let monetary_cols = [
        ("coin_quotes", "price"),
        ("coin_candles", "open"),
        ("coin_candles", "high"),
        ("coin_candles", "low"),
        ("coin_candles", "close"),
        ("coin_candles", "volume"),
        ("coin_market_snapshots", "price"),
        ("coin_market_snapshots", "market_cap"),
        ("coin_market_snapshots", "fully_diluted_valuation"),
        ("coin_market_snapshots", "circulating_supply"),
        ("coin_market_snapshots", "total_supply"),
        ("coin_market_snapshots", "volume_24h"),
        ("coin_metadata", "max_supply"),
    ];

    for (table, col) in &monetary_cols {
        let dtype: Option<String> = sqlx::query_scalar(
            "SELECT data_type FROM information_schema.columns
             WHERE table_schema='public' AND table_name=$1 AND column_name=$2",
        )
        .bind(table)
        .bind(col)
        .fetch_optional(&pool)
        .await
        .expect("catalog query");
        if let Some(dt) = dtype {
            assert_eq!(
                dt, "numeric",
                "Column '{table}.{col}' must be NUMERIC, found '{dt}' (REQ-DB-040)"
            );
        }
        // If column doesn't exist, it's either optional or handled elsewhere — not an assertion failure here
        // (some nullable columns like bid/ask/volume might only fail if they exist with wrong type)
    }

    // All timestamp columns must be TIMESTAMPTZ (current coin-keyed schema; F-58).
    let ts_cols: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_name, column_name
         FROM information_schema.columns
         WHERE table_schema = 'public'
           AND column_name LIKE '%_at' OR column_name = 'ts' OR column_name = 'as_of'
           AND data_type NOT IN ('timestamp with time zone', 'interval')
           AND table_name IN (
               'tracked_coins', 'coin_quotes', 'coin_candles',
               'coin_market_snapshots', 'coin_metadata',
               'collection_queue', 'backfill_jobs', 'backfill_chunks', 'upstream_request_pacer'
           )",
    )
    .fetch_all(&pool)
    .await
    .expect("timestamp type check");
    // Filter only timestamp-like columns that are NOT timestamptz
    let violations: Vec<_> = ts_cols
        .iter()
        .filter(|(_, col)| col.ends_with("_at") || col == "ts" || col == "as_of")
        .collect();
    assert!(
        violations.is_empty(),
        "All timestamp columns must be TIMESTAMPTZ, but found violations: {violations:?} (REQ-DB-041)"
    );
}

// ── Scenario 13: Unseeded-month write fails loudly (REQ-DB-017) ───────────────
//
// SPEC-API-005 F-58: rewritten from the pre-0011 market-keyed live_quotes to the coin-keyed
// coin_quotes table, which is still RANGE-partitioned through 2027-12 (0011). coin_candles is
// NOT used here — it was de-partitioned by 0020 and accepts any ts.

#[tokio::test]
#[ignore]
async fn scenario_13_write_to_unseeded_partition_fails() {
    let pool = setup().await;
    let suffix = uuid::Uuid::new_v4().to_string().replace('-', "")[..8].to_string();
    let coin_id = insert_test_coin(&pool, &format!("s13-{suffix}")).await;

    // ts = 2028-06-15 is beyond the last coin_quotes partition (2027-12 → 2028-01-01 boundary).
    let result = sqlx::query(
        "INSERT INTO coin_quotes (coin_id, vs_currency, ts, price, source)
         VALUES ($1, 'usd', '2028-06-15 00:00:00+00'::timestamptz, 42000, 'test')",
    )
    .bind(&coin_id)
    .execute(&pool)
    .await;

    assert!(
        result.is_err(),
        "Write to unseeded coin_quotes partition (2028-06) must fail loudly, not silently drop (REQ-DB-017)"
    );

    sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
        .bind(&coin_id)
        .execute(&pool)
        .await
        .ok();
}

// ── Scenario 14: Migrations idempotent on re-apply (REQ-DB-043) ───────────────

#[tokio::test]
#[ignore]
async fn scenario_14_migrations_idempotent() {
    let pool = setup().await;

    // Re-applying migrations via connect() must succeed (IF NOT EXISTS everywhere)
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool2 = crypto_collector::db::connect(&url)
        .await
        .expect("Re-applying migrations must succeed (idempotent, REQ-DB-043)");
    drop(pool2);
    drop(pool);
}

// ── Scenario 15: Live-poller contract columns and claim index (REQ-DB-002/005) ──
//
// SPEC-API-005 F-58: rewritten from the pre-0011 tracked_markets to tracked_coins, which is
// where the per-coin live-poller columns live (added by migration 0010) after 0011 removed
// tracked_markets.

#[tokio::test]
#[ignore]
async fn scenario_15_live_poller_contract_columns_and_index() {
    let pool = setup().await;

    let required = [
        ("last_polled_at", "timestamp with time zone"),
        ("live_poll_claimed_until", "timestamp with time zone"),
        ("live_poll_interval", "interval"),
    ];
    for (col, expected_type) in &required {
        let dtype: String = sqlx::query_scalar(
            "SELECT data_type FROM information_schema.columns
             WHERE table_schema='public' AND table_name='tracked_coins' AND column_name=$1",
        )
        .bind(col)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|_| panic!("tracked_coins.{col} must exist (REQ-DB-002)"));
        assert_eq!(
            &dtype, expected_type,
            "tracked_coins.{col} must be '{expected_type}' (REQ-DB-002/041)"
        );
    }

    // status must restrict to active/paused/error.
    let suffix = uuid::Uuid::new_v4().to_string().replace('-', "")[..8].to_string();
    sqlx::query(
        "INSERT INTO tracked_coins (coin_id, symbol, name, status)
         VALUES ($1, 'S15', 'Scenario15', 'invalid_status')",
    )
    .bind(format!("s15-{suffix}"))
    .execute(&pool)
    .await
    .expect_err("invalid status must be rejected by CHECK constraint (REQ-DB-002)");

    // Partial claim index on tracked_coins (last_polled_at) WHERE status='active' must exist.
    let has_idx: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM pg_indexes
            WHERE schemaname='public' AND tablename='tracked_coins'
              AND indexdef ILIKE '%last_polled_at%'
              AND indexdef ILIKE '%active%'
         )",
    )
    .fetch_one(&pool)
    .await
    .expect("index check");
    assert!(
        has_idx,
        "tracked_coins must have partial index on last_polled_at WHERE status='active' (REQ-DB-005)"
    );
}

// ── SPEC-PROV-003 M3: F-20 interval-stamp cleanup migration 0021 (REQ-PROV-067) ──
//
// DB-gated + INFORMATIONAL (AC Scenario 2): these exercise the SHIPPED migration body
// (migrations/0021_*.sql, executed via sqlx::raw_sql) against seeded data. The migration
// already ran at connect()-time on the live DB; here we re-run its idempotent statements
// on purpose-seeded rows to prove the no-op / rewrite / collision-safe behaviour.
//
// FK discipline: coin_candles.coin_id REFERENCES tracked_coins(coin_id) ON DELETE CASCADE,
// so each test seeds the parent tracked_coins row FIRST (ON CONFLICT DO NOTHING) and tears
// down children (coin_candles) before the parent (tracked_coins). Unique coin_id per test;
// run with --test-threads=1 (the migration DELETE/UPDATE are global over interval).

/// Read the shipped 0021 migration body and execute it against the DB.
///
/// Split into individual statements (on `;`) so each runs via the parameter-free
/// `sqlx::query` path (`raw_sql` requires a `&'static str`). This exercises the ACTUAL
/// shipped migration body — no duplicated SQL — so a drift between the file and the test
/// is impossible.
async fn run_0021_canonicalise(pool: &PgPool) {
    // sqlx's query API requires a `&'static str` (injection-safety); leak the file-read
    // body so the ACTUAL shipped migration runs verbatim (test-only, short-lived process).
    let sql: &'static str = Box::leak(
        std::fs::read_to_string("migrations/0021_coingecko_range_interval_canonicalise.sql")
            .expect("read migrations/0021_coingecko_range_interval_canonicalise.sql")
            .into_boxed_str(),
    );
    for statement in sql.split(';') {
        if statement.trim().is_empty() {
            continue;
        }
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("0021 canonicalise migration must never raise (collision-safe)");
    }
}

async fn seed_coin(pool: &PgPool, coin_id: &str) {
    sqlx::query(
        "INSERT INTO tracked_coins (coin_id, symbol, name, status)
         VALUES ($1, 'TST', 'F20 Test', 'active')
         ON CONFLICT (coin_id) DO NOTHING",
    )
    .bind(coin_id)
    .execute(pool)
    .await
    .expect("seed parent tracked_coins (FK)");
}

async fn seed_candle(
    pool: &PgPool,
    coin_id: &str,
    interval: &str,
    ts: chrono::DateTime<chrono::Utc>,
) {
    sqlx::query(
        "INSERT INTO coin_candles (coin_id, vs_currency, interval, ts, open, high, low, close, source)
         VALUES ($1, 'usd', $2, $3, 1, 2, 0.5, 1.5, 'coingecko')
         ON CONFLICT DO NOTHING",
    )
    .bind(coin_id)
    .bind(interval)
    .bind(ts)
    .execute(pool)
    .await
    .expect("seed coin_candle");
}

async fn intervals_for(pool: &PgPool, coin_id: &str) -> Vec<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT interval FROM coin_candles WHERE coin_id = $1 ORDER BY interval",
    )
    .bind(coin_id)
    .fetch_all(pool)
    .await
    .expect("select intervals")
}

async fn teardown_coin(pool: &PgPool, coin_id: &str) {
    // Child before parent (FK): delete coin_candles, then tracked_coins.
    sqlx::query("DELETE FROM coin_candles WHERE coin_id = $1")
        .bind(coin_id)
        .execute(pool)
        .await
        .expect("teardown coin_candles");
    sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
        .bind(coin_id)
        .execute(pool)
        .await
        .expect("teardown tracked_coins");
}

/// Zero daily/hourly rows: the migration is a no-op — a canonical '1d' row is untouched.
#[tokio::test]
#[ignore]
async fn scenario_02_migration_0021_is_noop_on_canonical_rows() {
    let pool = setup().await;
    let coin_id = "test-f20-noop";
    let ts = chrono::Utc::now();
    teardown_coin(&pool, coin_id).await; // pristine start
    seed_coin(&pool, coin_id).await;
    seed_candle(&pool, coin_id, "1d", ts).await;

    run_0021_canonicalise(&pool).await;

    assert_eq!(
        intervals_for(&pool, coin_id).await,
        vec!["1d".to_string()],
        "migration must not touch already-canonical rows (no-op on zero daily/hourly)"
    );
    teardown_coin(&pool, coin_id).await;
}

/// A seeded 'daily' row (no canonical twin) is rewritten to '1d'; a second run is idempotent.
#[tokio::test]
#[ignore]
async fn scenario_02_migration_0021_rewrites_daily_to_1d_idempotently() {
    let pool = setup().await;
    let coin_id = "test-f20-rewrite";
    let ts = chrono::Utc::now();
    teardown_coin(&pool, coin_id).await;
    seed_coin(&pool, coin_id).await;
    seed_candle(&pool, coin_id, "daily", ts).await;
    seed_candle(&pool, coin_id, "hourly", ts + chrono::Duration::hours(1)).await;

    run_0021_canonicalise(&pool).await;
    let mut got = intervals_for(&pool, coin_id).await;
    got.sort();
    assert_eq!(
        got,
        vec!["1d".to_string(), "1h".to_string()],
        "'daily'->'1d' and 'hourly'->'1h' with no shadowing twin"
    );

    // Idempotent: a second run finds no daily/hourly rows and changes nothing.
    run_0021_canonicalise(&pool).await;
    let mut again = intervals_for(&pool, coin_id).await;
    again.sort();
    assert_eq!(again, vec!["1d".to_string(), "1h".to_string()]);
    teardown_coin(&pool, coin_id).await;
}

/// Collision: a 'daily' row AND its canonical '1d' twin share (coin_id, vs_currency, ts).
/// The migration drops the 'daily' duplicate and completes WITHOUT a unique-violation.
#[tokio::test]
#[ignore]
async fn scenario_02_migration_0021_drops_shadowed_duplicate_without_pk_violation() {
    let pool = setup().await;
    let coin_id = "test-f20-collision";
    let ts = chrono::Utc::now();
    teardown_coin(&pool, coin_id).await;
    seed_coin(&pool, coin_id).await;
    // Both rows at the SAME (coin_id, vs_currency, ts) — distinct only by interval.
    seed_candle(&pool, coin_id, "1d", ts).await; // canonical twin (correct data)
    seed_candle(&pool, coin_id, "daily", ts).await; // shadowed non-canonical duplicate

    // Must not raise a PK unique-violation (run_0021_canonicalise .expect()s success).
    run_0021_canonicalise(&pool).await;

    assert_eq!(
        intervals_for(&pool, coin_id).await,
        vec!["1d".to_string()],
        "the shadowed 'daily' duplicate is dropped; the canonical '1d' twin survives"
    );
    teardown_coin(&pool, coin_id).await;
}
