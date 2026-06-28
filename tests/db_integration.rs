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

// Helper: insert a tracked_coin and tracked_market, return the market id.
async fn insert_test_market(pool: &PgPool, suffix: &str) -> i64 {
    let coin_id = format!("test-coin-{suffix}");
    let base = format!("TBASE{suffix}");
    let quote = "USD";

    sqlx::query(
        "INSERT INTO tracked_coins (coin_id, symbol, name, status)
         VALUES ($1, $2, $3, 'active')
         ON CONFLICT (coin_id) DO NOTHING",
    )
    .bind(&coin_id)
    .bind(&base)
    .bind(format!("Test Coin {suffix}"))
    .execute(pool)
    .await
    .expect("insert tracked_coin");

    sqlx::query_scalar::<_, i64>(
        "INSERT INTO tracked_markets (base, quote, kind, status, coin_id)
         VALUES ($1, $2, 'spot', 'active', $3)
         ON CONFLICT (base, quote, COALESCE(venue, '')) DO UPDATE SET status = 'active'
         RETURNING id",
    )
    .bind(&base)
    .bind(quote)
    .bind(&coin_id)
    .fetch_one(pool)
    .await
    .expect("insert tracked_market")
}

// ── Scenario 1: Two registries created with correct keys (REQ-DB-001/002) ────

#[tokio::test]
#[ignore]
async fn scenario_01_registries_exist_with_correct_pk() {
    let pool = setup().await;

    // tracked_coins: coin_id is PK
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

    // tracked_markets: surrogate id pk, base/quote not null, kind, venue nullable
    let cols: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT column_name, data_type, is_nullable
         FROM information_schema.columns
         WHERE table_schema = 'public' AND table_name = 'tracked_markets'
         ORDER BY ordinal_position",
    )
    .fetch_all(&pool)
    .await
    .expect("query");

    let col_map: std::collections::HashMap<_, _> = cols
        .iter()
        .map(|(name, dtype, nullable)| (name.as_str(), (dtype.as_str(), nullable.as_str())))
        .collect();

    assert!(col_map.contains_key("id"), "tracked_markets must have id");
    assert!(
        col_map.contains_key("base"),
        "tracked_markets must have base"
    );
    assert!(
        col_map.contains_key("quote"),
        "tracked_markets must have quote"
    );
    assert!(
        col_map.contains_key("venue"),
        "tracked_markets must have venue"
    );
    assert!(
        col_map.contains_key("coin_id"),
        "tracked_markets must have coin_id FK"
    );
    assert!(
        col_map.contains_key("kind"),
        "tracked_markets must have kind"
    );

    // venue and coin_id must be nullable
    assert_eq!(col_map["venue"].1, "YES", "venue must be nullable");
    assert_eq!(col_map["coin_id"].1, "YES", "coin_id must be nullable");
    // base and quote must be NOT NULL
    assert_eq!(col_map["base"].1, "NO", "base must be NOT NULL");
    assert_eq!(col_map["quote"].1, "NO", "quote must be NOT NULL");
}

// ── Scenario 2: NULL-venue + named-venue coexistence, duplicate rejected (REQ-DB-003) ──

#[tokio::test]
#[ignore]
async fn scenario_02_pair_uniqueness_coalesce_venue() {
    let pool = setup().await;

    let suffix = uuid::Uuid::new_v4().to_string().replace('-', "")[..8].to_string();
    let base = format!("SC2{suffix}");

    // Insert coin
    sqlx::query(
        "INSERT INTO tracked_coins (coin_id, symbol, name, status) VALUES ($1, $2, $3, 'active')",
    )
    .bind(format!("sc2coin-{suffix}"))
    .bind(&base)
    .bind(format!("Scenario2 {suffix}"))
    .execute(&pool)
    .await
    .expect("insert coin");

    // First insert: NULL venue (aggregator row) — must succeed
    sqlx::query(
        "INSERT INTO tracked_markets (base, quote, venue, kind, status) VALUES ($1, 'USD', NULL, 'spot', 'active')",
    )
    .bind(&base)
    .execute(&pool)
    .await
    .expect("first insert (NULL venue) must succeed");

    // Second insert: named venue — must succeed (aggregator + venue coexist)
    sqlx::query(
        "INSERT INTO tracked_markets (base, quote, venue, kind, status) VALUES ($1, 'USD', 'binance', 'spot', 'active')",
    )
    .bind(&base)
    .execute(&pool)
    .await
    .expect("second insert (named venue) must succeed");

    // Third insert: second NULL venue — must fail (duplicate)
    let result = sqlx::query(
        "INSERT INTO tracked_markets (base, quote, venue, kind, status) VALUES ($1, 'USD', NULL, 'spot', 'active')",
    )
    .bind(&base)
    .execute(&pool)
    .await;
    assert!(
        result.is_err(),
        "second NULL-venue insert for the same pair must be rejected by unique index (REQ-DB-003)"
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

// ── Scenario 4: Time-series tables RANGE-partitioned with btree + BRIN (REQ-DB-014/015) ──

#[tokio::test]
#[ignore]
async fn scenario_04_partitioned_tables_with_indexes() {
    let pool = setup().await;

    let tables = [
        "live_quotes",
        "candles",
        "coin_market_snapshots",
        "derivatives_quotes",
    ];
    for table in &tables {
        // Verify RANGE partitioning via pg_catalog
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

        // Verify BRIN index exists on parent
        let has_brin: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM pg_indexes
                WHERE schemaname = 'public' AND tablename = $1
                  AND indexdef ILIKE '%using brin%'
             )",
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .expect("brin index check");
        assert!(
            has_brin,
            "Table '{table}' must have a BRIN index on ts (REQ-DB-015)"
        );

        // Verify btree index on ts DESC exists
        let has_btree_ts: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM pg_indexes
                WHERE schemaname = 'public' AND tablename = $1
                  AND indexdef ILIKE '%ts desc%'
             )",
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .expect("btree ts index check");
        assert!(
            has_btree_ts,
            "Table '{table}' must have a btree index with ts DESC (REQ-DB-015)"
        );

        // Verify at least one monthly partition exists
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
            "Table '{table}' must have at least 12 monthly partitions (REQ-DB-016), found {partition_count}"
        );
    }
}

// ── Scenario 5: Candle PK includes interval; volume nullable (REQ-DB-011) ────

#[tokio::test]
#[ignore]
async fn scenario_05_candle_pk_and_nullable_volume() {
    let pool = setup().await;
    let market_id = insert_test_market(&pool, "s05").await;

    let ts = "2026-06-01 12:00:00+00";

    // Insert 1m candle
    sqlx::query(
        "INSERT INTO candles (market_id, interval, ts, open, high, low, close, vs_currency, source)
         VALUES ($1, '1m', $2::timestamptz, 42000, 42100, 41900, 42050, 'usd', 'test')
         ON CONFLICT DO NOTHING",
    )
    .bind(market_id)
    .bind(ts)
    .execute(&pool)
    .await
    .expect("1m candle insert");

    // Insert 1d candle for same (market_id, ts) — PK includes interval so they coexist
    sqlx::query(
        "INSERT INTO candles (market_id, interval, ts, open, high, low, close, vs_currency, source)
         VALUES ($1, '1d', $2::timestamptz, 40000, 43000, 39500, 42050, 'usd', 'test')
         ON CONFLICT DO NOTHING",
    )
    .bind(market_id)
    .bind(ts)
    .execute(&pool)
    .await
    .expect("1d candle insert must succeed — different interval coexists");

    // Insert candle with NULL volume (CoinGecko OHLC: no volume)
    let ts2 = "2026-06-01 13:00:00+00";
    sqlx::query(
        "INSERT INTO candles (market_id, interval, ts, open, high, low, close, volume, vs_currency, source)
         VALUES ($1, '1h', $2::timestamptz, 42000, 42100, 41900, 42050, NULL, 'usd', 'coingecko')
         ON CONFLICT DO NOTHING",
    )
    .bind(market_id)
    .bind(ts2)
    .execute(&pool)
    .await
    .expect("NULL volume candle insert must succeed (REQ-DB-011)");

    // Verify both intervals exist for original ts
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM candles WHERE market_id = $1 AND ts = $2::timestamptz AND interval IN ('1m', '1d')")
            .bind(market_id)
            .bind(ts)
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(
        count, 2,
        "1m and 1d candles must coexist for same (market_id, ts)"
    );
}

// ── Scenario 6: Derivatives observables in one tick (REQ-DB-013) ─────────────

#[tokio::test]
#[ignore]
async fn scenario_06_derivatives_single_tick_all_columns() {
    let pool = setup().await;

    let required_cols = [
        "funding_rate",
        "open_interest",
        "open_interest_usd",
        "mark_price",
        "index_price",
        "basis",
    ];
    for col in &required_cols {
        let row = sqlx::query(
            "SELECT data_type FROM information_schema.columns
             WHERE table_schema = 'public' AND table_name = 'derivatives_quotes' AND column_name = $1",
        )
        .bind(col)
        .fetch_optional(&pool)
        .await
        .expect("catalog query")
        .unwrap_or_else(|| panic!("derivatives_quotes must have column '{col}' (REQ-DB-013)"));
        assert_eq!(
            row.get::<String, _>("data_type"),
            "numeric",
            "derivatives_quotes.{col} must be NUMERIC (REQ-DB-040)"
        );
    }

    // Assert no separate funding_rate_history or open_interest_history table
    let has_separate_funding: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema='public' AND table_name='funding_rates')",
    )
    .fetch_one(&pool)
    .await
    .expect("catalog query");
    assert!(
        !has_separate_funding,
        "funding rates must live in derivatives_quotes, not a separate table (REQ-DB-013)"
    );
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

// ── Scenario 10: Backfill idempotent enqueue + lease columns (REQ-DB-033) ────

#[tokio::test]
#[ignore]
async fn scenario_10_backfill_idempotent_enqueue_and_lease_columns() {
    let pool = setup().await;
    let market_id = insert_test_market(&pool, "s10").await;

    let dataset = "candles:1h";

    // First enqueue
    sqlx::query(
        "INSERT INTO backfill_jobs (market_id, dataset, status) VALUES ($1, $2, 'pending')",
    )
    .bind(market_id)
    .bind(dataset)
    .execute(&pool)
    .await
    .expect("first backfill job insert");

    // Second enqueue with same (market_id, dataset) — must be no-op due to UNIQUE
    let result = sqlx::query(
        "INSERT INTO backfill_jobs (market_id, dataset, status) VALUES ($1, $2, 'pending')",
    )
    .bind(market_id)
    .bind(dataset)
    .execute(&pool)
    .await;
    assert!(
        result.is_err(),
        "duplicate backfill job must be rejected by UNIQUE(market_id, dataset) (REQ-DB-033)"
    );

    // Verify backfill_chunks has all required lease columns
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

    // All monetary/quantity columns must be NUMERIC
    let monetary_cols = [
        ("live_quotes", "price"),
        ("live_quotes", "bid"),
        ("live_quotes", "ask"),
        ("live_quotes", "volume_24h"),
        ("candles", "open"),
        ("candles", "high"),
        ("candles", "low"),
        ("candles", "close"),
        ("candles", "volume"),
        ("coin_market_snapshots", "price"),
        ("coin_market_snapshots", "market_cap"),
        ("coin_market_snapshots", "fully_diluted_valuation"),
        ("coin_market_snapshots", "circulating_supply"),
        ("coin_market_snapshots", "total_supply"),
        ("coin_market_snapshots", "volume_24h"),
        ("derivatives_quotes", "funding_rate"),
        ("derivatives_quotes", "open_interest"),
        ("derivatives_quotes", "open_interest_usd"),
        ("derivatives_quotes", "mark_price"),
        ("derivatives_quotes", "index_price"),
        ("derivatives_quotes", "basis"),
        ("derivatives_quotes", "volume_24h"),
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

    // All timestamp columns must be TIMESTAMPTZ
    let ts_cols: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_name, column_name
         FROM information_schema.columns
         WHERE table_schema = 'public'
           AND column_name LIKE '%_at' OR column_name = 'ts' OR column_name = 'as_of'
           AND data_type NOT IN ('timestamp with time zone', 'interval')
           AND table_name IN (
               'tracked_coins', 'tracked_markets', 'live_quotes', 'candles',
               'coin_market_snapshots', 'derivatives_quotes', 'coin_metadata',
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

#[tokio::test]
#[ignore]
async fn scenario_13_write_to_unseeded_partition_fails() {
    let pool = setup().await;
    let market_id = insert_test_market(&pool, "s13").await;

    // ts = 2028-06-15 is beyond the last partition (2027-12-31)
    let result = sqlx::query(
        "INSERT INTO live_quotes (market_id, ts, price, vs_currency, source)
         VALUES ($1, '2028-06-15 00:00:00+00'::timestamptz, 42000.0, 'usd', 'test')",
    )
    .bind(market_id)
    .execute(&pool)
    .await;

    assert!(
        result.is_err(),
        "Write to unseeded partition (2028-06) must fail loudly, not silently drop (REQ-DB-017)"
    );
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
             WHERE table_schema='public' AND table_name='tracked_markets' AND column_name=$1",
        )
        .bind(col)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|_| panic!("tracked_markets.{col} must exist (REQ-DB-002)"));
        assert_eq!(
            &dtype, expected_type,
            "tracked_markets.{col} must be '{expected_type}' (REQ-DB-002/041)"
        );
    }

    // status must restrict to active/paused/error
    sqlx::query(
        "INSERT INTO tracked_markets (base, quote, kind, status) VALUES ('S15TST', 'USD', 'spot', 'invalid_status')",
    )
    .execute(&pool)
    .await
    .expect_err("invalid status must be rejected by CHECK constraint (REQ-DB-002)");

    // Partial claim index WHERE status='active' must exist
    let has_idx: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM pg_indexes
            WHERE schemaname='public' AND tablename='tracked_markets'
              AND indexdef ILIKE '%last_polled_at%'
              AND indexdef ILIKE '%active%'
         )",
    )
    .fetch_one(&pool)
    .await
    .expect("index check");
    assert!(
        has_idx,
        "tracked_markets must have partial index on last_polled_at WHERE status='active' (REQ-DB-005)"
    );
}
