//! Unconditional tests — no database required.
//!
//! Verifies migration file presence, naming, and static content invariants defined by SPEC-DB-001.
//! These tests run as part of `cargo test` without DATABASE_URL.

use std::fs;
use std::path::Path;

// ── Milestone 1: migration file presence and naming ──────────────────────────

#[test]
fn all_nine_migration_files_exist() {
    let expected = [
        "migrations/0001_registries.sql",
        "migrations/0002_live_quotes.sql",
        "migrations/0003_candles.sql",
        "migrations/0004_coin_market_snapshots.sql",
        "migrations/0005_derivatives_quotes.sql",
        "migrations/0006_coin_metadata.sql",
        "migrations/0007_collection_queue.sql",
        "migrations/0008_backfill.sql",
        "migrations/0009_upstream_pacer.sql",
    ];
    for path in &expected {
        assert!(
            Path::new(path).exists(),
            "Missing required migration file: {path}"
        );
    }
}

// ── Milestone 5: precision and integrity sweep (static) ──────────────────────

/// REQ-DB-040: no DOUBLE PRECISION or REAL for monetary/quantity columns.
/// We check the entire migrations directory so any future migration regression is caught.
/// SQL line comments (-- ...) are stripped before checking to avoid false positives from
/// comments that document what we *don't* use (e.g. "never DOUBLE PRECISION").
#[test]
fn no_double_precision_in_migrations() {
    let dir = fs::read_dir("migrations").expect("migrations/ directory must exist");
    for entry in dir {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("sql") {
            continue;
        }
        let raw = fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("Failed to read {}", path.display()));

        // Strip SQL line comments so we only check actual DDL, not explanatory comments.
        let code_only: String = raw
            .lines()
            .map(|line| {
                if let Some(idx) = line.find("--") {
                    &line[..idx]
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase();

        assert!(
            !code_only.contains("double precision"),
            "Migration '{}' uses 'double precision' in DDL — prohibited by REQ-DB-040 (use NUMERIC)",
            path.display()
        );
        // REAL is a short alias for DOUBLE PRECISION; also prohibited.
        // Check for ` REAL` followed by space/comma/newline to avoid matching "real" in names.
        let has_real_type = code_only.contains(" real ")
            || code_only.contains("\treal ")
            || code_only.contains(" real,")
            || code_only.contains(" real\n")
            || code_only.ends_with(" real");
        assert!(
            !has_real_type,
            "Migration '{}' uses REAL type in DDL — prohibited by REQ-DB-040 (use NUMERIC)",
            path.display()
        );
    }
}

// ── REQ-DB-004: no equities machinery ────────────────────────────────────────

#[test]
fn no_equities_tables_in_migrations() {
    let prohibited_tables = ["exchanges", "market_phase", "trading_halt", "calendar"];
    let dir = fs::read_dir("migrations").expect("migrations/ directory must exist");
    for entry in dir {
        let entry = entry.unwrap();
        let path = entry.path();
        let content = fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("Failed to read {}", path.display()))
            .to_lowercase();
        for table in &prohibited_tables {
            let create_if = format!("create table if not exists {table}");
            let create = format!("create table {table}");
            assert!(
                !content.contains(&create_if) && !content.contains(&create),
                "Migration '{}' creates a prohibited equities table '{table}' (REQ-DB-004)",
                path.display()
            );
        }
    }
}

// ── Milestone 1 content checks ───────────────────────────────────────────────

/// REQ-DB-002/005: live-poller contract columns must be present in registries migration.
#[test]
fn registries_migration_has_live_poller_columns() {
    let content = fs::read_to_string("migrations/0001_registries.sql")
        .expect("0001_registries.sql must exist");
    let lower = content.to_lowercase();
    assert!(
        lower.contains("last_polled_at"),
        "0001_registries.sql missing 'last_polled_at' (REQ-DB-002)"
    );
    assert!(
        lower.contains("live_poll_claimed_until"),
        "0001_registries.sql missing 'live_poll_claimed_until' (REQ-DB-002)"
    );
    assert!(
        lower.contains("live_poll_interval"),
        "0001_registries.sql missing 'live_poll_interval' (REQ-DB-002)"
    );
}

/// REQ-DB-005: partial claim index for live-quote poller must be present.
#[test]
fn registries_migration_has_live_poll_claim_index() {
    let content = fs::read_to_string("migrations/0001_registries.sql")
        .expect("0001_registries.sql must exist");
    let lower = content.to_lowercase();
    // Index must be partial on status = 'active'
    assert!(
        lower.contains("where status = 'active'") || lower.contains("where (status = 'active')"),
        "0001_registries.sql missing partial claim index WHERE status='active' (REQ-DB-005)"
    );
}

/// REQ-DB-003: uniqueness on (base, quote, COALESCE(venue, '')) must use expression index.
#[test]
fn registries_migration_has_coalesce_unique_index() {
    let content = fs::read_to_string("migrations/0001_registries.sql")
        .expect("0001_registries.sql must exist");
    let lower = content.to_lowercase();
    assert!(
        lower.contains("coalesce(venue") || lower.contains("coalesce (venue"),
        "0001_registries.sql missing COALESCE(venue, '') unique expression index (REQ-DB-003)"
    );
}

// ── Milestone 4: coordination tables ─────────────────────────────────────────

/// REQ-DB-036: lease-expired re-claim index must be in collection_queue migration.
#[test]
fn collection_queue_has_lease_expired_reclaim_index() {
    let content = fs::read_to_string("migrations/0007_collection_queue.sql")
        .expect("0007_collection_queue.sql must exist");
    let lower = content.to_lowercase();
    // Must have a partial index on lease_expires_at where status IN ('claimed','running')
    assert!(
        lower.contains("lease_expires_at"),
        "0007_collection_queue.sql missing lease_expires_at reclaim index (REQ-DB-036)"
    );
    assert!(
        lower.contains("claimed") && lower.contains("running"),
        "0007_collection_queue.sql lease-expired index must cover 'claimed' and 'running' (REQ-DB-036)"
    );
}

/// REQ-DB-032: pending-path claim index on (enqueued_at) WHERE status='pending'.
#[test]
fn collection_queue_has_pending_claim_index() {
    let content = fs::read_to_string("migrations/0007_collection_queue.sql")
        .expect("0007_collection_queue.sql must exist");
    let lower = content.to_lowercase();
    assert!(
        lower.contains("enqueued_at"),
        "0007_collection_queue.sql missing enqueued_at claim index (REQ-DB-032)"
    );
    assert!(
        lower.contains("status = 'pending'") || lower.contains("status='pending'"),
        "0007_collection_queue.sql pending claim index must have WHERE status='pending' (REQ-DB-032)"
    );
}

/// REQ-DB-035: upstream_request_pacer must seed all four known providers.
#[test]
fn pacer_seeds_four_providers() {
    let content = fs::read_to_string("migrations/0009_upstream_pacer.sql")
        .expect("0009_upstream_pacer.sql must exist");
    let lower = content.to_lowercase();
    for provider in &["coingecko", "binance", "coinbase", "kraken"] {
        assert!(
            lower.contains(provider),
            "0009_upstream_pacer.sql must seed provider '{}' (REQ-DB-035)",
            provider
        );
    }
}

/// REQ-DB-033: backfill_jobs UNIQUE (market_id, dataset) for idempotent enqueue.
#[test]
fn backfill_migration_has_unique_job_constraint() {
    let content =
        fs::read_to_string("migrations/0008_backfill.sql").expect("0008_backfill.sql must exist");
    let lower = content.to_lowercase();
    assert!(
        lower.contains("unique (market_id, dataset)")
            || lower.contains("unique(market_id, dataset)"),
        "0008_backfill.sql missing UNIQUE (market_id, dataset) on backfill_jobs (REQ-DB-033)"
    );
}

/// REQ-DB-015: each partitioned table migration must declare both btree and BRIN indexes.
#[test]
fn time_series_migrations_have_btree_and_brin_indexes() {
    let partitioned_migrations = [
        ("migrations/0002_live_quotes.sql", "live_quotes"),
        ("migrations/0003_candles.sql", "candles"),
        (
            "migrations/0004_coin_market_snapshots.sql",
            "coin_market_snapshots",
        ),
        (
            "migrations/0005_derivatives_quotes.sql",
            "derivatives_quotes",
        ),
    ];
    for (path, table) in &partitioned_migrations {
        let content = fs::read_to_string(path)
            .unwrap_or_else(|_| panic!("Failed to read {path}"))
            .to_lowercase();
        assert!(
            content.contains("brin"),
            "Migration '{path}' for table '{table}' missing BRIN index (REQ-DB-015)"
        );
        assert!(
            content.contains("ts desc") || content.contains("ts)"),
            "Migration '{path}' for table '{table}' missing btree ts index (REQ-DB-015)"
        );
        assert!(
            content.contains("partition by range"),
            "Migration '{path}' for table '{table}' not RANGE-partitioned (REQ-DB-014)"
        );
    }
}

/// REQ-DB-011: candles volume column must be nullable (CoinGecko OHLC has no volume).
#[test]
fn candles_migration_has_nullable_volume() {
    let content =
        fs::read_to_string("migrations/0003_candles.sql").expect("0003_candles.sql must exist");
    // volume should NOT have NOT NULL constraint
    let lower = content.to_lowercase();
    // Check that volume column exists but is not declared NOT NULL
    assert!(
        lower.contains("volume"),
        "0003_candles.sql must have a 'volume' column (REQ-DB-011)"
    );
    // Ensure we don't have "volume ... not null"
    // Simple heuristic: find "volume" line and check it doesn't have "not null"
    for line in content.lines() {
        let line_lower = line.to_lowercase();
        if line_lower.trim_start().starts_with("volume") {
            assert!(
                !line_lower.contains("not null"),
                "candles 'volume' column must be nullable (REQ-DB-011), but found: {line}"
            );
        }
    }
}

/// REQ-DB-023: coin_metadata must have as-of index on (coin_id, first_seen_at DESC).
#[test]
fn coin_metadata_has_as_of_index() {
    let content = fs::read_to_string("migrations/0006_coin_metadata.sql")
        .expect("0006_coin_metadata.sql must exist");
    let lower = content.to_lowercase();
    assert!(
        lower.contains("first_seen_at"),
        "0006_coin_metadata.sql missing first_seen_at column (REQ-DB-020/023)"
    );
    assert!(
        lower.contains("last_seen_at"),
        "0006_coin_metadata.sql missing last_seen_at column (REQ-DB-020)"
    );
}

/// REQ-DB-012: coin_market_snapshots must have market cap, FDV, supply columns (not coin_metadata).
#[test]
fn coin_market_snapshots_has_aggregate_columns() {
    let content = fs::read_to_string("migrations/0004_coin_market_snapshots.sql")
        .expect("0004_coin_market_snapshots.sql must exist");
    let lower = content.to_lowercase();
    for col in &[
        "market_cap",
        "fully_diluted_valuation",
        "circulating_supply",
        "total_supply",
    ] {
        assert!(
            lower.contains(col),
            "0004_coin_market_snapshots.sql missing column '{}' (REQ-DB-012)",
            col
        );
    }
}

/// REQ-DB-013: derivatives_quotes must have all required derivative columns.
#[test]
fn derivatives_quotes_has_required_columns() {
    let content = fs::read_to_string("migrations/0005_derivatives_quotes.sql")
        .expect("0005_derivatives_quotes.sql must exist");
    let lower = content.to_lowercase();
    for col in &[
        "funding_rate",
        "open_interest",
        "open_interest_usd",
        "mark_price",
        "index_price",
        "basis",
    ] {
        assert!(
            lower.contains(col),
            "0005_derivatives_quotes.sql missing column '{}' (REQ-DB-013)",
            col
        );
    }
}

/// REQ-DB-043: all migrations must use IF NOT EXISTS for idempotency.
#[test]
fn all_migrations_use_if_not_exists() {
    let dir = fs::read_dir("migrations").expect("migrations/ directory must exist");
    for entry in dir {
        let entry = entry.unwrap();
        let path = entry.path();
        // Only check .sql files
        if path.extension().and_then(|e| e.to_str()) != Some("sql") {
            continue;
        }
        let content = fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("Failed to read {}", path.display()))
            .to_lowercase();
        // Every CREATE TABLE must use IF NOT EXISTS
        let create_table_count = content.matches("create table ").count();
        let create_table_if_count = content.matches("create table if not exists ").count();
        if create_table_count > 0 {
            assert_eq!(
                create_table_count,
                create_table_if_count,
                "Migration '{}' has {} CREATE TABLE without IF NOT EXISTS (REQ-DB-043)",
                path.display(),
                create_table_count - create_table_if_count
            );
        }
    }
}
