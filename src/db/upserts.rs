//! Idempotent upsert helpers for all collected data tables (SPEC-SCHED-001 REQ-SCHED-040).
//!
//! Every function uses natural-key conflict targets so re-executing a crashed work unit
//! overwrites identical rows rather than duplicating them (REQ-SCHED-040).
//!
//! # Natural keys
//! - `coin_quotes`:            `(coin_id, vs_currency, ts)` — SPEC-API-002
//! - `coin_candles`:           `(coin_id, vs_currency, interval, ts)` — SPEC-API-002
//! - `coin_market_snapshots`:  `(coin_id, vs_currency, ts)`
//! - `coin_metadata`:          `(coin_id, revision)` — revision logic is application-controlled

use anyhow::Result;
use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::PgPool;

use crate::db::partitions::ensure_candle_partition;
use crate::models::quote::CoinCandle;
use crate::providers::{CoinMarket, CoinMeta, SpotQuote};

// ── @MX annotation ────────────────────────────────────────────────────────────
// @MX:NOTE: [AUTO] All upserts use ON CONFLICT DO UPDATE on natural keys (REQ-SCHED-040).
//   Re-executing a crashed work unit overwrites the same rows — no duplicates.
//   coin_market_snapshots/derivatives_quotes are partitioned by ts;
//   ON CONFLICT requires the full PK including the partition key (ts).
// @MX:SPEC: SPEC-SCHED-001 REQ-SCHED-040

// ── coin_quotes (SPEC-API-002 REQ-SCHED-040) ─────────────────────────────────

/// Upsert a coin spot quote and notify WebSocket listeners. Natural key: `(coin_id, vs_currency, ts)`.
///
/// Runs in a short transaction so the upsert and `pg_notify` are atomic.
/// Downstream: `src/listener.rs` relays the NOTIFY payload to `AppState.coin_quote_tx`.
///
// @MX:NOTE: [AUTO] upsert_coin_quote — idempotent on (coin_id, vs_currency, ts); emits pg_notify
// @MX:SPEC: SPEC-API-002 SPEC-SCHED-001 REQ-SCHED-040 REQ-API-148
pub const UPSERT_COIN_QUOTE_SQL: &str = "\
    INSERT INTO coin_quotes \
        (coin_id, vs_currency, ts, price, source) \
    VALUES ($1, $2, $3, $4, $5) \
    ON CONFLICT (coin_id, vs_currency, ts) DO UPDATE SET \
        price  = EXCLUDED.price, \
        source = EXCLUDED.source";

pub async fn upsert_coin_quote(
    pool: &PgPool,
    coin_id: &str,
    q: &SpotQuote,
) -> Result<(), sqlx::Error> {
    let start = std::time::Instant::now();

    let payload = serde_json::json!({
        "coin_id": coin_id,
        "vs_currency": q.vs_currency,
        "ts": q.ts.to_rfc3339(),
        "price": q.price.to_string(),
        "source": q.source,
    })
    .to_string();

    let mut tx = pool.begin().await?;

    sqlx::query(UPSERT_COIN_QUOTE_SQL)
        .bind(coin_id)
        .bind(&q.vs_currency)
        .bind(q.ts)
        .bind(q.price)
        .bind(&q.source)
        .execute(&mut *tx)
        .await?;

    // Emit notify within the same tx (atomic upsert + notify, REQ-API-148).
    sqlx::query("SELECT pg_notify('coin_quote_updated', $1)")
        .bind(&payload)
        .execute(&mut *tx)
        .await?;

    let result = tx.commit().await;
    metrics::histogram!("coin_quote_insert_duration_seconds").record(start.elapsed().as_secs_f64());
    result?;
    Ok(())
}

// ── coin_candles (SPEC-API-002 REQ-SCHED-040) ────────────────────────────────

/// Upsert a coin OHLCV candle and notify WebSocket listeners.
/// Natural key: `(coin_id, vs_currency, interval, ts)`.
///
/// Runs in a short transaction so the upsert and `pg_notify` are atomic.
///
// @MX:NOTE: [AUTO] upsert_coin_candle — idempotent on (coin_id, vs_currency, interval, ts); emits pg_notify
// @MX:SPEC: SPEC-API-002 SPEC-SCHED-001 REQ-SCHED-040 REQ-API-148
pub const UPSERT_COIN_CANDLE_SQL: &str = "\
    INSERT INTO coin_candles \
        (coin_id, vs_currency, interval, ts, open, high, low, close, volume, source) \
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
    ON CONFLICT (coin_id, vs_currency, interval, ts) DO UPDATE SET \
        open   = EXCLUDED.open, \
        high   = EXCLUDED.high, \
        low    = EXCLUDED.low, \
        close  = EXCLUDED.close, \
        volume = EXCLUDED.volume, \
        source = EXCLUDED.source";

pub async fn upsert_coin_candle(pool: &PgPool, candle: &CoinCandle) -> Result<(), sqlx::Error> {
    let start = std::time::Instant::now();

    let payload = serde_json::json!({
        "coin_id": candle.coin_id,
        "vs_currency": candle.vs_currency,
        "interval": candle.interval,
        "ts": candle.ts.to_rfc3339(),
        "open": candle.open.to_string(),
        "high": candle.high.to_string(),
        "low": candle.low.to_string(),
        "close": candle.close.to_string(),
        "volume": candle.volume.map(|v| v.to_string()),
        "source": candle.source,
    })
    .to_string();

    // Ensure the covering monthly partition exists before insert (REQ historical
    // backfill: coin_candles only has static partitions for 2024-01..2027-12).
    // Runs in its own transaction, ahead of the insert transaction below.
    ensure_candle_partition(pool, candle.ts).await?;

    let mut tx = pool.begin().await?;

    sqlx::query(UPSERT_COIN_CANDLE_SQL)
        .bind(&candle.coin_id)
        .bind(&candle.vs_currency)
        .bind(&candle.interval)
        .bind(candle.ts)
        .bind(candle.open)
        .bind(candle.high)
        .bind(candle.low)
        .bind(candle.close)
        .bind(candle.volume)
        .bind(&candle.source)
        .execute(&mut *tx)
        .await?;

    sqlx::query("SELECT pg_notify('coin_candle_updated', $1)")
        .bind(&payload)
        .execute(&mut *tx)
        .await?;

    let result = tx.commit().await;
    metrics::histogram!("coin_candle_insert_duration_seconds")
        .record(start.elapsed().as_secs_f64());
    result?;
    Ok(())
}

// ── coin_market_snapshots ─────────────────────────────────────────────────────

/// Upsert a coin market snapshot. Natural key: `(coin_id, vs_currency, ts)`.
///
// @MX:NOTE: [AUTO] upsert_coin_market — idempotent on (coin_id, vs_currency, ts)
// @MX:SPEC: SPEC-SCHED-001 REQ-SCHED-040
pub const UPSERT_COIN_MARKET_SQL: &str = "\
    INSERT INTO coin_market_snapshots \
        (coin_id, vs_currency, ts, price, market_cap, fully_diluted_valuation, \
         circulating_supply, total_supply, volume_24h, source) \
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
    ON CONFLICT (coin_id, vs_currency, ts) DO UPDATE SET \
        price                   = EXCLUDED.price, \
        market_cap              = EXCLUDED.market_cap, \
        fully_diluted_valuation = EXCLUDED.fully_diluted_valuation, \
        circulating_supply      = EXCLUDED.circulating_supply, \
        total_supply            = EXCLUDED.total_supply, \
        volume_24h              = EXCLUDED.volume_24h, \
        source                  = EXCLUDED.source";

pub async fn upsert_coin_market_snapshot(pool: &PgPool, m: &CoinMarket) -> Result<(), sqlx::Error> {
    sqlx::query(UPSERT_COIN_MARKET_SQL)
        .bind(&m.coin_id)
        .bind(&m.vs_currency)
        .bind(m.ts)
        .bind(m.price)
        .bind(m.market_cap)
        .bind(m.fully_diluted_valuation)
        .bind(m.circulating_supply)
        .bind(m.total_supply)
        .bind(m.volume_24h)
        .bind(&m.source)
        .execute(pool)
        .await?;
    Ok(())
}

// ── coin_metadata (revision pattern) ─────────────────────────────────────────

/// Snapshot of an existing `coin_metadata` revision for change detection.
#[derive(Debug, sqlx::FromRow)]
pub struct LatestMetadata {
    pub revision: i32,
    pub name: String,
    pub symbol: String,
    pub categories: Option<Vec<String>>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub links: Option<serde_json::Value>,
    pub contract_addresses: Option<serde_json::Value>,
    pub max_supply: Option<Decimal>,
    pub genesis_date: Option<NaiveDate>,
}

/// Returns `true` if any tracked metadata field has changed (REQ-DB-021, REQ-SCHED-042).
///
/// Tracked fields: name, symbol, categories, description, homepage, links,
/// contract_addresses, max_supply, genesis_date.
///
/// This is a pure function — no I/O. Testable without DB or network.
pub fn metadata_has_changed(existing: &LatestMetadata, new: &CoinMeta) -> bool {
    existing.name != new.name
        || existing.symbol != new.symbol
        || existing.categories != new.categories
        || existing.description != new.description
        || existing.homepage != new.homepage
        || existing.links != new.links
        || existing.contract_addresses != new.contract_addresses
        || existing.max_supply != new.max_supply
        || existing.genesis_date != new.genesis_date
}

/// Upsert coin metadata using the revision pattern (REQ-DB-021, REQ-SCHED-042).
///
/// - If no existing revision: insert revision 0.
/// - If unchanged: advance `last_seen_at` on the current revision only.
/// - If changed: insert a new revision (current + 1).
///
// @MX:WARN: [AUTO] upsert_coin_metadata — insert new revision ONLY on value change
// @MX:REASON: REQ-DB-021: advancing last_seen_at must NOT insert a new revision if unchanged.
//             metadata_has_changed() is the gate; bypassing it causes revision churn.
// @MX:SPEC: SPEC-SCHED-001 REQ-SCHED-042; SPEC-DB-001 REQ-DB-021
pub async fn upsert_coin_metadata(pool: &PgPool, meta: &CoinMeta) -> Result<()> {
    // Query for the current highest revision.
    let latest: Option<LatestMetadata> = sqlx::query_as(
        "SELECT revision, name, symbol, categories, description, homepage, links, \
                contract_addresses, max_supply, genesis_date \
         FROM coin_metadata \
         WHERE coin_id = $1 \
         ORDER BY revision DESC \
         LIMIT 1",
    )
    .bind(&meta.coin_id)
    .fetch_optional(pool)
    .await?;

    match latest {
        None => {
            // First time: insert revision 0.
            insert_metadata_revision(pool, meta, 0).await?;
        }
        Some(ref existing) if !metadata_has_changed(existing, meta) => {
            // Unchanged: advance last_seen_at only (no new row).
            sqlx::query(
                "UPDATE coin_metadata \
                 SET last_seen_at = now() \
                 WHERE coin_id = $1 AND revision = $2",
            )
            .bind(&meta.coin_id)
            .bind(existing.revision)
            .execute(pool)
            .await?;
        }
        Some(ref existing) => {
            // Changed: insert next revision.
            insert_metadata_revision(pool, meta, existing.revision + 1).await?;
        }
    }
    Ok(())
}

async fn insert_metadata_revision(pool: &PgPool, meta: &CoinMeta, revision: i32) -> Result<()> {
    sqlx::query(
        "INSERT INTO coin_metadata \
            (coin_id, revision, name, symbol, categories, description, homepage, \
             links, contract_addresses, max_supply, genesis_date) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(&meta.coin_id)
    .bind(revision)
    .bind(&meta.name)
    .bind(&meta.symbol)
    .bind(&meta.categories)
    .bind(&meta.description)
    .bind(&meta.homepage)
    .bind(&meta.links)
    .bind(&meta.contract_addresses)
    .bind(meta.max_supply)
    .bind(meta.genesis_date)
    .execute(pool)
    .await?;
    Ok(())
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn base_meta() -> CoinMeta {
        CoinMeta {
            coin_id: "bitcoin".to_string(),
            name: "Bitcoin".to_string(),
            symbol: "BTC".to_string(),
            categories: Some(vec!["Cryptocurrency".to_string()]),
            description: Some("Peer-to-peer electronic cash".to_string()),
            homepage: Some("https://bitcoin.org".to_string()),
            links: None,
            contract_addresses: None,
            max_supply: Some(dec!(21000000)),
            genesis_date: Some(NaiveDate::from_ymd_opt(2009, 1, 3).unwrap()),
        }
    }

    fn existing_from_meta(meta: &CoinMeta) -> LatestMetadata {
        LatestMetadata {
            revision: 0,
            name: meta.name.clone(),
            symbol: meta.symbol.clone(),
            categories: meta.categories.clone(),
            description: meta.description.clone(),
            homepage: meta.homepage.clone(),
            links: meta.links.clone(),
            contract_addresses: meta.contract_addresses.clone(),
            max_supply: meta.max_supply,
            genesis_date: meta.genesis_date,
        }
    }

    // ── Scenario 10 / REQ-SCHED-042: metadata change detection ───────────────

    #[test]
    fn metadata_unchanged_returns_false() {
        let meta = base_meta();
        let existing = existing_from_meta(&meta);
        assert!(
            !metadata_has_changed(&existing, &meta),
            "identical metadata must not trigger a new revision"
        );
    }

    #[test]
    fn metadata_name_change_detected() {
        let meta = base_meta();
        let mut existing = existing_from_meta(&meta);
        existing.name = "Ethereum".to_string();
        assert!(
            metadata_has_changed(&existing, &meta),
            "name change must be detected"
        );
    }

    #[test]
    fn metadata_symbol_change_detected() {
        let meta = base_meta();
        let mut existing = existing_from_meta(&meta);
        existing.symbol = "ETH".to_string();
        assert!(metadata_has_changed(&existing, &meta));
    }

    #[test]
    fn metadata_categories_change_detected() {
        let meta = base_meta();
        let mut existing = existing_from_meta(&meta);
        existing.categories = None;
        assert!(metadata_has_changed(&existing, &meta));
    }

    #[test]
    fn metadata_max_supply_change_detected() {
        let meta = base_meta();
        let mut existing = existing_from_meta(&meta);
        existing.max_supply = Some(dec!(42000000)); // different from BTC's 21M
        assert!(metadata_has_changed(&existing, &meta));
    }

    #[test]
    fn metadata_genesis_date_change_detected() {
        let meta = base_meta();
        let mut existing = existing_from_meta(&meta);
        existing.genesis_date = None;
        assert!(metadata_has_changed(&existing, &meta));
    }

    #[test]
    fn metadata_null_to_value_detected() {
        let mut meta = base_meta();
        meta.description = None;
        let existing = existing_from_meta(&meta);
        // Now meta has Some(description)
        meta.description = Some("Updated description".to_string());
        assert!(metadata_has_changed(&existing, &meta));
    }

    // ── SQL shape assertions for upsert constants ─────────────────────────────

    #[test]
    fn coin_market_upsert_sql_has_conflict_target() {
        assert!(
            UPSERT_COIN_MARKET_SQL.contains("ON CONFLICT (coin_id, vs_currency, ts) DO UPDATE"),
            "coin_market upsert must use natural key (coin_id, vs_currency, ts)"
        );
    }

    // coin_quotes
    #[test]
    fn coin_quote_upsert_sql_has_correct_conflict_target() {
        assert!(
            UPSERT_COIN_QUOTE_SQL.contains("ON CONFLICT (coin_id, vs_currency, ts) DO UPDATE"),
            "coin_quote upsert must use natural key (coin_id, vs_currency, ts)"
        );
    }

    #[test]
    fn coin_quote_upsert_sql_targets_correct_table() {
        assert!(
            UPSERT_COIN_QUOTE_SQL.contains("INSERT INTO coin_quotes"),
            "upsert must target coin_quotes table"
        );
    }

    #[test]
    fn coin_quote_upsert_sql_no_market_id() {
        assert!(
            !UPSERT_COIN_QUOTE_SQL.contains("market_id"),
            "coin_quotes upsert must not reference market_id (coin-keyed)"
        );
    }

    // coin_candles
    #[test]
    fn coin_candle_upsert_sql_has_correct_conflict_target() {
        assert!(
            UPSERT_COIN_CANDLE_SQL
                .contains("ON CONFLICT (coin_id, vs_currency, interval, ts) DO UPDATE"),
            "coin_candle upsert must use natural key (coin_id, vs_currency, interval, ts)"
        );
    }

    #[test]
    fn coin_candle_upsert_sql_targets_correct_table() {
        assert!(
            UPSERT_COIN_CANDLE_SQL.contains("INSERT INTO coin_candles"),
            "upsert must target coin_candles table"
        );
    }

    #[test]
    fn coin_candle_upsert_sql_no_market_id() {
        assert!(
            !UPSERT_COIN_CANDLE_SQL.contains("market_id"),
            "coin_candles upsert must not reference market_id (coin-keyed)"
        );
    }
}
