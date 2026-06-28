//! Idempotent upsert helpers for all collected data tables (SPEC-SCHED-001 REQ-SCHED-040).
//!
//! Every function uses natural-key conflict targets so re-executing a crashed work unit
//! overwrites identical rows rather than duplicating them (REQ-SCHED-040).
//!
//! # Natural keys
//! - `live_quotes`:            `(market_id, ts)`
//! - `candles`:                `(market_id, interval, ts)`
//! - `coin_market_snapshots`:  `(coin_id, vs_currency, ts)`
//! - `derivatives_quotes`:     `(market_id, ts)`
//! - `coin_metadata`:          `(coin_id, revision)` — revision logic is application-controlled

use anyhow::Result;
use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::PgPool;

use crate::providers::{CoinMarket, CoinMeta, DerivTick, OhlcCandle, SpotQuote};

// ── @MX annotation ────────────────────────────────────────────────────────────
// @MX:NOTE: [AUTO] All upserts use ON CONFLICT DO UPDATE on natural keys (REQ-SCHED-040).
//   Re-executing a crashed work unit overwrites the same rows — no duplicates.
//   live_quotes/candles/coin_market_snapshots/derivatives_quotes are partitioned by ts;
//   ON CONFLICT requires the full PK including the partition key (ts).
// @MX:SPEC: SPEC-SCHED-001 REQ-SCHED-040

// ── live_quotes ───────────────────────────────────────────────────────────────

/// Upsert a live spot quote. Natural key: `(market_id, ts)`.
///
/// On conflict, overwrites price/bid/ask/volume/source (market data changed).
/// `bid_size`, `ask_size`, and `as_of` are not in `SpotQuote`; they are left NULL on insert.
///
// @MX:NOTE: [AUTO] upsert_live_quote — idempotent on (market_id, ts); exact-once persistence
// @MX:SPEC: SPEC-SCHED-001 REQ-SCHED-040
pub const UPSERT_LIVE_QUOTE_SQL: &str = "\
    INSERT INTO live_quotes \
        (market_id, ts, price, bid, ask, volume_24h, vs_currency, source) \
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
    ON CONFLICT (market_id, ts) DO UPDATE SET \
        price      = EXCLUDED.price, \
        bid        = EXCLUDED.bid, \
        ask        = EXCLUDED.ask, \
        volume_24h = EXCLUDED.volume_24h, \
        source     = EXCLUDED.source";

pub async fn upsert_live_quote(pool: &PgPool, q: &SpotQuote) -> Result<(), sqlx::Error> {
    sqlx::query(UPSERT_LIVE_QUOTE_SQL)
        .bind(q.market_id)
        .bind(q.ts)
        .bind(q.price)
        .bind(q.bid)
        .bind(q.ask)
        .bind(q.volume_24h)
        .bind(&q.vs_currency)
        .bind(&q.source)
        .execute(pool)
        .await?;
    Ok(())
}

// ── candles ───────────────────────────────────────────────────────────────────

/// Upsert a single OHLCV candle. Natural key: `(market_id, interval, ts)`.
///
// @MX:NOTE: [AUTO] upsert_candle — idempotent on (market_id, interval, ts); exact-once persistence
// @MX:SPEC: SPEC-SCHED-001 REQ-SCHED-040
pub const UPSERT_CANDLE_SQL: &str = "\
    INSERT INTO candles \
        (market_id, interval, ts, open, high, low, close, volume, vs_currency, source) \
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
    ON CONFLICT (market_id, interval, ts) DO UPDATE SET \
        open       = EXCLUDED.open, \
        high       = EXCLUDED.high, \
        low        = EXCLUDED.low, \
        close      = EXCLUDED.close, \
        volume     = EXCLUDED.volume, \
        source     = EXCLUDED.source";

pub async fn upsert_candle(pool: &PgPool, c: &OhlcCandle) -> Result<(), sqlx::Error> {
    sqlx::query(UPSERT_CANDLE_SQL)
        .bind(c.market_id)
        .bind(&c.interval)
        .bind(c.ts)
        .bind(c.open)
        .bind(c.high)
        .bind(c.low)
        .bind(c.close)
        .bind(c.volume)
        .bind(&c.vs_currency)
        .bind(&c.source)
        .execute(pool)
        .await?;
    Ok(())
}

/// Upsert a batch of OHLCV candles in a single transaction.
pub async fn upsert_candles(pool: &PgPool, candles: &[OhlcCandle]) -> Result<(), sqlx::Error> {
    if candles.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for c in candles {
        sqlx::query(UPSERT_CANDLE_SQL)
            .bind(c.market_id)
            .bind(&c.interval)
            .bind(c.ts)
            .bind(c.open)
            .bind(c.high)
            .bind(c.low)
            .bind(c.close)
            .bind(c.volume)
            .bind(&c.vs_currency)
            .bind(&c.source)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
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

// ── derivatives_quotes ────────────────────────────────────────────────────────

/// Upsert a derivatives tick. Natural key: `(market_id, ts)`.
///
// @MX:NOTE: [AUTO] upsert_derivatives — idempotent on (market_id, ts)
// @MX:SPEC: SPEC-SCHED-001 REQ-SCHED-040
pub const UPSERT_DERIVATIVES_SQL: &str = "\
    INSERT INTO derivatives_quotes \
        (market_id, ts, funding_rate, open_interest, open_interest_usd, \
         mark_price, index_price, basis, volume_24h, contract_type, venue, source) \
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
    ON CONFLICT (market_id, ts) DO UPDATE SET \
        funding_rate      = EXCLUDED.funding_rate, \
        open_interest     = EXCLUDED.open_interest, \
        open_interest_usd = EXCLUDED.open_interest_usd, \
        mark_price        = EXCLUDED.mark_price, \
        index_price       = EXCLUDED.index_price, \
        basis             = EXCLUDED.basis, \
        volume_24h        = EXCLUDED.volume_24h, \
        contract_type     = EXCLUDED.contract_type, \
        venue             = EXCLUDED.venue, \
        source            = EXCLUDED.source";

pub async fn upsert_derivatives_quote(pool: &PgPool, d: &DerivTick) -> Result<(), sqlx::Error> {
    sqlx::query(UPSERT_DERIVATIVES_SQL)
        .bind(d.market_id)
        .bind(d.ts)
        .bind(d.funding_rate)
        .bind(d.open_interest)
        .bind(d.open_interest_usd)
        .bind(d.mark_price)
        .bind(d.index_price)
        .bind(d.basis)
        .bind(d.volume_24h)
        .bind(&d.contract_type)
        .bind(&d.venue)
        .bind(&d.source)
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
    fn live_quote_upsert_sql_has_conflict_target() {
        assert!(
            UPSERT_LIVE_QUOTE_SQL.contains("ON CONFLICT (market_id, ts) DO UPDATE"),
            "live_quote upsert must use natural key (market_id, ts)"
        );
    }

    #[test]
    fn candle_upsert_sql_has_interval_in_conflict_target() {
        assert!(
            UPSERT_CANDLE_SQL.contains("ON CONFLICT (market_id, interval, ts) DO UPDATE"),
            "candle upsert must use natural key (market_id, interval, ts)"
        );
    }

    #[test]
    fn coin_market_upsert_sql_has_conflict_target() {
        assert!(
            UPSERT_COIN_MARKET_SQL.contains("ON CONFLICT (coin_id, vs_currency, ts) DO UPDATE"),
            "coin_market upsert must use natural key (coin_id, vs_currency, ts)"
        );
    }

    #[test]
    fn derivatives_upsert_sql_has_conflict_target() {
        assert!(
            UPSERT_DERIVATIVES_SQL.contains("ON CONFLICT (market_id, ts) DO UPDATE"),
            "derivatives upsert must use natural key (market_id, ts)"
        );
    }
}
