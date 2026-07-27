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
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;

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
    // Emit through the shared const (REQ-OBS-060) — canonical name, no `coin_` prefix.
    metrics::histogram!(crate::metrics::QUOTE_INSERT_DURATION_SECONDS)
        .record(start.elapsed().as_secs_f64());
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

    // Payload built via the shared helper so the batched live-poll NOTIFY (REQ-REFACTOR-042,
    // CandleNotifyPolicy::PerEvent) emits a byte-identical broadcast to this per-row path.
    let payload = candle_notify_payload(candle);

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
    // Emit through the shared const (REQ-OBS-060) — canonical name, no `coin_` prefix.
    metrics::histogram!(crate::metrics::CANDLE_INSERT_DURATION_SECONDS)
        .record(start.elapsed().as_secs_f64());
    result?;
    Ok(())
}

// ── Shared batched candle upsert (SPEC-REFACTOR-001 M4, F-51/F-52) ────────────
//
// Generalized from `rollup::batched_upsert_candles` (REQ-REFACTOR-040) so the candles-dispatch
// hot path (collection_queue, ~2016 rows/refresh) and the backfill page-write path both write
// through one UNNEST-based batch instead of per-row `upsert_coin_candle`. Two policy axes keep
// the paths honest: the D1 conflict policy (native unconditional vs rollup native-wins guard)
// and the F-51 NOTIFY policy (live per-event notify vs backfill silence).

/// Conflict-resolution policy for [`batched_upsert_coin_candles`] (Decision D1,
/// SPEC-REFACTOR-001 REQ-REFACTOR-041). The two policies are DISTINCT and MUST NOT be collapsed
/// into a single unconditional `DO UPDATE` — that conflation is the explicit §G anti-pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandleConflictPolicy {
    /// Native provider writes (live dispatch, backfill): unconditional `DO UPDATE` — a native
    /// row always overwrites on conflict, byte-identical to the per-row [`upsert_coin_candle`]
    /// `ON CONFLICT ... DO UPDATE` (no `rollup:%` guard).
    NativeOverwrite,
    /// Rollup materializer writes: `DO UPDATE ... WHERE coin_candles.source LIKE 'rollup:%'` —
    /// upgrades ONLY a prior rollup row; a colliding native provider row is left byte-identical
    /// (native-wins, SPEC-CANDLE-002 REQ-CANDLE-052).
    RollupGuarded,
}

/// NOTIFY-emission policy for [`batched_upsert_coin_candles`] (SPEC-REFACTOR-001 REQ-REFACTOR-042
/// — INTENDED BEHAVIOR CHANGE (b)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandleNotifyPolicy {
    /// Live-poll path: emit one `pg_notify('coin_candle_updated', payload)` per candle inside the
    /// same transaction as the batch upsert — preserves the per-event WebSocket broadcast
    /// (behavior-preserving vs the per-row [`upsert_coin_candle`]).
    PerEvent,
    /// Backfill path: emit NO NOTIFY so historical rows are not broadcast to WebSocket consumers
    /// (INTENDED CHANGE (b)).
    Silent,
}

/// Shared base UNNEST-based batched candle upsert literal, carrying the unconditional (native)
/// `DO UPDATE` (no `rollup:%` guard, D1). Kept as a macro so BOTH the native and the rollup
/// `&'static str` consts below reuse the exact same base text (`concat!` requires literals, so a
/// const identifier cannot be embedded — the macro is how the base is shared without duplication).
/// sqlx 0.9 requires `&'static str` (the `SqlSafeStr` bound), so the two policies resolve to two
/// static literals rather than a runtime-built `String`.
macro_rules! batched_upsert_coin_candles_base_sql {
    () => {
        "\
    INSERT INTO coin_candles \
        (coin_id, vs_currency, interval, ts, open, high, low, close, volume, source) \
     SELECT * FROM UNNEST( \
        $1::text[], $2::text[], $3::text[], $4::timestamptz[], \
        $5::numeric[], $6::numeric[], $7::numeric[], $8::numeric[], \
        $9::numeric[], $10::text[] \
     ) \
     ON CONFLICT (coin_id, vs_currency, interval, ts) DO UPDATE SET \
        open   = EXCLUDED.open, \
        high   = EXCLUDED.high, \
        low    = EXCLUDED.low, \
        close  = EXCLUDED.close, \
        volume = EXCLUDED.volume, \
        source = EXCLUDED.source"
    };
}

/// Native path (dispatch, backfill): unconditional `DO UPDATE`. Byte-identical DO-UPDATE column
/// set to [`UPSERT_COIN_CANDLE_SQL`] — no `rollup:%` guard (D1).
pub const BATCHED_UPSERT_COIN_CANDLES_NATIVE_SQL: &str = batched_upsert_coin_candles_base_sql!();

/// Native-wins guard appended for [`CandleConflictPolicy::RollupGuarded`] ONLY (D1). A derived
/// rollup writer upgrades only a prior rollup row; a colliding native row is left byte-identical.
pub const ROLLUP_NATIVE_WINS_GUARD: &str = " WHERE coin_candles.source LIKE 'rollup:%'";

/// Rollup path: the native SQL PLUS the native-wins guard. Assembled as `concat!` of the shared
/// base + the guard, so it is provably `native + guard` (D1) — verified by
/// `batched_rollup_sql_is_native_plus_guard`.
pub const BATCHED_UPSERT_COIN_CANDLES_ROLLUP_SQL: &str = concat!(
    batched_upsert_coin_candles_base_sql!(),
    " WHERE coin_candles.source LIKE 'rollup:%'"
);

/// Select the batched-upsert SQL for `policy` (pure; makes the D1 two-policy split unit-testable
/// without a DB). Returns `&'static str` (sqlx `SqlSafeStr`): the native path is the base literal;
/// the rollup path is base + guard.
pub fn batched_candle_upsert_sql(policy: CandleConflictPolicy) -> &'static str {
    match policy {
        CandleConflictPolicy::NativeOverwrite => BATCHED_UPSERT_COIN_CANDLES_NATIVE_SQL,
        CandleConflictPolicy::RollupGuarded => BATCHED_UPSERT_COIN_CANDLES_ROLLUP_SQL,
    }
}

/// The `coin_candle_updated` NOTIFY payload for one candle — the single source of truth shared by
/// [`upsert_coin_candle`] and the batched [`CandleNotifyPolicy::PerEvent`] path, so the live
/// broadcast payload stays byte-identical across the per-row and batched write paths.
fn candle_notify_payload(candle: &CoinCandle) -> String {
    serde_json::json!({
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
    .to_string()
}

/// Batched UNNEST upsert of `candles`, parameterized by the D1 `conflict` policy and the F-51
/// `notify` policy. An N-row batch is row-for-row equivalent to N single `upsert_coin_candle`
/// calls of the same policy (same `(coin_id, vs_currency, interval, ts)` conflict target, same
/// DO-UPDATE column set). The whole page is one statement in one transaction; when
/// `notify == PerEvent`, one `pg_notify` per candle is emitted inside that same transaction.
///
// @MX:ANCHOR: [AUTO] batched_upsert_coin_candles — shared candle batch write for the candles
//             dispatch, backfill, and rollup paths. Two invariants ride here: (1) D1 two-policy
//             split — `NativeOverwrite` is an UNCONDITIONAL DO UPDATE; `RollupGuarded` appends the
//             `WHERE coin_candles.source LIKE 'rollup:%'` native-wins guard — the two MUST NOT be
//             collapsed into one unconditional upsert (that reopens the F-07 native-row clobber).
//             (2) F-51 NOTIFY policy — `PerEvent` (live) emits one pg_notify per candle;
//             `Silent` (backfill) emits none, so historical rows never flood the WebSocket.
// @MX:REASON: fan_in >= 3 (collection_queue candles dispatch, backfill page writes,
//             rollup::batched_upsert_candles) AND a data-integrity + broadcast-policy invariant:
//             a wrong conflict policy silently destroys native rows; a wrong notify policy floods
//             every WebSocket consumer with historical backfill rows.
// @MX:SPEC: SPEC-REFACTOR-001 REQ-REFACTOR-040 REQ-REFACTOR-041 REQ-REFACTOR-042
pub async fn batched_upsert_coin_candles(
    pool: &PgPool,
    candles: &[CoinCandle],
    conflict: CandleConflictPolicy,
    notify: CandleNotifyPolicy,
) -> Result<(), sqlx::Error> {
    if candles.is_empty() {
        return Ok(());
    }

    let coin_ids: Vec<&str> = candles.iter().map(|c| c.coin_id.as_str()).collect();
    let vs_currencies: Vec<&str> = candles.iter().map(|c| c.vs_currency.as_str()).collect();
    let intervals: Vec<&str> = candles.iter().map(|c| c.interval.as_str()).collect();
    let tss: Vec<DateTime<Utc>> = candles.iter().map(|c| c.ts).collect();
    let opens: Vec<Decimal> = candles.iter().map(|c| c.open).collect();
    let highs: Vec<Decimal> = candles.iter().map(|c| c.high).collect();
    let lows: Vec<Decimal> = candles.iter().map(|c| c.low).collect();
    let closes: Vec<Decimal> = candles.iter().map(|c| c.close).collect();
    let volumes: Vec<Option<Decimal>> = candles.iter().map(|c| c.volume).collect();
    let sources: Vec<&str> = candles.iter().map(|c| c.source.as_str()).collect();

    let sql = batched_candle_upsert_sql(conflict);

    let mut tx = pool.begin().await?;

    sqlx::query(sql)
        .bind(&coin_ids)
        .bind(&vs_currencies)
        .bind(&intervals)
        .bind(&tss)
        .bind(&opens)
        .bind(&highs)
        .bind(&lows)
        .bind(&closes)
        .bind(&volumes)
        .bind(&sources)
        .execute(&mut *tx)
        .await?;

    // REQ-REFACTOR-042 (INTENDED CHANGE b): live path notifies per event; backfill stays Silent.
    if matches!(notify, CandleNotifyPolicy::PerEvent) {
        for candle in candles {
            let payload = candle_notify_payload(candle);
            sqlx::query("SELECT pg_notify('coin_candle_updated', $1)")
                .bind(&payload)
                .execute(&mut *tx)
                .await?;
        }
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

    // ── SPEC-REFACTOR-001 M4: shared batched candle upsert (pure / non-DB) ────────

    // AC-REFACTOR-041a (pure portion): the two conflict policies are DISTINCT (D1). The native
    // path is an UNCONDITIONAL DO UPDATE; the rollup path appends the `rollup:%` native-wins
    // guard. A single collapsed unconditional upsert for both is the §G anti-pattern.
    #[test]
    fn batched_native_sql_is_unconditional_no_rollup_guard() {
        let sql = batched_candle_upsert_sql(CandleConflictPolicy::NativeOverwrite);
        assert!(
            !sql.contains("rollup:%"),
            "native path must NOT carry the rollup native-wins guard (unconditional DO UPDATE)"
        );
        assert!(
            !sql.contains("WHERE coin_candles.source LIKE"),
            "native path must have no ON CONFLICT WHERE clause"
        );
    }

    #[test]
    fn batched_rollup_sql_has_native_wins_guard() {
        let sql = batched_candle_upsert_sql(CandleConflictPolicy::RollupGuarded);
        assert!(
            sql.contains("WHERE coin_candles.source LIKE 'rollup:%'"),
            "rollup path must retain the native-wins guard (D1, REQ-CANDLE-052)"
        );
    }

    // D1 relationship: the rollup SQL is EXACTLY the native SQL plus the guard suffix — proving the
    // two policies share one base and differ only by the native-wins guard (never conflated).
    #[test]
    fn batched_rollup_sql_is_native_plus_guard() {
        assert_eq!(
            BATCHED_UPSERT_COIN_CANDLES_ROLLUP_SQL,
            format!("{BATCHED_UPSERT_COIN_CANDLES_NATIVE_SQL}{ROLLUP_NATIVE_WINS_GUARD}"),
            "rollup SQL must be native SQL + native-wins guard (D1)"
        );
    }

    #[test]
    fn batched_both_policies_share_conflict_target() {
        for policy in [
            CandleConflictPolicy::NativeOverwrite,
            CandleConflictPolicy::RollupGuarded,
        ] {
            let sql = batched_candle_upsert_sql(policy);
            assert!(
                sql.contains("ON CONFLICT (coin_id, vs_currency, interval, ts) DO UPDATE"),
                "both policies must preserve the (coin_id, vs_currency, interval, ts) conflict target"
            );
            assert!(
                sql.contains("INSERT INTO coin_candles"),
                "both policies target coin_candles"
            );
            assert!(
                sql.contains("UNNEST("),
                "both policies use a single UNNEST-based INSERT (batched)"
            );
        }
    }

    // The native batched DO-UPDATE column set must match the per-row `upsert_coin_candle`
    // (REQ-REFACTOR-041 row-for-row parity of the update semantics).
    #[test]
    fn batched_native_do_update_columns_match_per_row() {
        let batched = batched_candle_upsert_sql(CandleConflictPolicy::NativeOverwrite);
        for col in ["open", "high", "low", "close", "volume", "source"] {
            let needle = format!("= EXCLUDED.{col}");
            assert!(
                batched.contains(&needle),
                "batched native SQL must set {col} = EXCLUDED.{col}"
            );
            assert!(
                UPSERT_COIN_CANDLE_SQL.contains(&needle),
                "per-row SQL must set {col} = EXCLUDED.{col} (parity of DO UPDATE semantics)"
            );
        }
    }

    // NOTIFY-flag routing: the two policies are distinct values. PerEvent drives per-candle
    // pg_notify; Silent suppresses it (REQ-REFACTOR-042, INTENDED CHANGE b). The runtime routing
    // is DB-gated (db_live_path_emits_one_notify_backfill_emits_zero); here we pin the enum shape.
    #[test]
    fn notify_policy_variants_are_distinct() {
        assert_ne!(CandleNotifyPolicy::PerEvent, CandleNotifyPolicy::Silent);
        assert_eq!(CandleNotifyPolicy::PerEvent, CandleNotifyPolicy::PerEvent);
    }

    // The batched PerEvent NOTIFY payload is byte-identical to the per-row `upsert_coin_candle`
    // payload (shared `candle_notify_payload` SSOT) so the live WebSocket broadcast is unchanged.
    #[test]
    fn candle_notify_payload_shape_and_null_volume() {
        let ts = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let candle = CoinCandle {
            coin_id: "bitcoin".into(),
            vs_currency: "usd".into(),
            interval: "1d".into(),
            ts,
            open: dec!(100.5),
            high: dec!(110),
            low: dec!(99),
            close: dec!(105.25),
            volume: Some(dec!(1234.5)),
            source: "binance".into(),
        };
        let payload = candle_notify_payload(&candle);
        let v: serde_json::Value = serde_json::from_str(&payload).expect("valid JSON payload");
        assert_eq!(v["coin_id"], "bitcoin");
        assert_eq!(v["vs_currency"], "usd");
        assert_eq!(v["interval"], "1d");
        assert_eq!(v["ts"], ts.to_rfc3339());
        assert_eq!(v["open"], "100.5");
        assert_eq!(v["close"], "105.25");
        assert_eq!(v["volume"], "1234.5");
        assert_eq!(v["source"], "binance");

        // NULL volume must serialize as JSON null (not the string "null").
        let mut null_vol = candle.clone();
        null_vol.volume = None;
        let p2 = candle_notify_payload(&null_vol);
        let v2: serde_json::Value = serde_json::from_str(&p2).unwrap();
        assert!(
            v2["volume"].is_null(),
            "None volume must serialize to JSON null"
        );
    }

    // ── SPEC-REFACTOR-001 M4: DB-gated integration tests ─────────────────────────
    // These require a live PostgreSQL and MUST run with `--test-threads=1` (shared coin_candles
    // table, throwaway coin_ids + explicit cleanup). Run:
    //   DATABASE_URL=postgres://... cargo test -p crypto-collector -- --ignored --test-threads=1
    // (per CLAUDE.md § Integration Tests). Each seeds the parent tracked_coins row first (FK
    // coin_candles_coin_id_fkey1) and tears down child-before-parent.

    use sqlx::postgres::PgListener;

    async fn db_pool() -> PgPool {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required for DB-gated tests");
        crate::db::connect(&url).await.expect("connect + migrate")
    }

    async fn seed_tracked_coin(pool: &PgPool, coin: &str) {
        sqlx::query(
            "INSERT INTO tracked_coins (coin_id, symbol, name, status) \
             VALUES ($1, 'TST', 'Test', 'active') \
             ON CONFLICT (coin_id) DO NOTHING",
        )
        .bind(coin)
        .execute(pool)
        .await
        .expect("seed tracked_coins parent");
    }

    async fn cleanup_coin(pool: &PgPool, coin: &str) {
        sqlx::query("DELETE FROM coin_candles WHERE coin_id = $1")
            .bind(coin)
            .execute(pool)
            .await
            .expect("cleanup coin_candles");
        sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
            .bind(coin)
            .execute(pool)
            .await
            .expect("cleanup tracked_coins");
    }

    #[allow(clippy::too_many_arguments)]
    fn candle(
        coin: &str,
        interval: &str,
        ts_secs: i64,
        open: Decimal,
        high: Decimal,
        low: Decimal,
        close: Decimal,
        volume: Option<Decimal>,
        source: &str,
    ) -> CoinCandle {
        CoinCandle {
            coin_id: coin.to_string(),
            vs_currency: "usd".to_string(),
            interval: interval.to_string(),
            ts: DateTime::<Utc>::from_timestamp(ts_secs, 0).unwrap(),
            open,
            high,
            low,
            close,
            volume,
            source: source.to_string(),
        }
    }

    /// Uniform-OHLC convenience candle (`o=h=l=c=price`).
    fn uniform(coin: &str, ts_secs: i64, price: Decimal, source: &str) -> CoinCandle {
        candle(
            coin,
            "1d",
            ts_secs,
            price,
            price,
            price,
            price,
            Some(dec!(1)),
            source,
        )
    }

    async fn fetch_candle(
        pool: &PgPool,
        coin: &str,
        interval: &str,
        ts_secs: i64,
    ) -> Option<(String, Decimal)> {
        sqlx::query_as::<_, (String, Decimal)>(
            "SELECT source, close FROM coin_candles \
             WHERE coin_id = $1 AND vs_currency = 'usd' AND interval = $2 AND ts = to_timestamp($3)",
        )
        .bind(coin)
        .bind(interval)
        .bind(ts_secs)
        .fetch_optional(pool)
        .await
        .expect("fetch candle")
    }

    type CandleRow = (
        String,
        DateTime<Utc>,
        Decimal,
        Decimal,
        Decimal,
        Decimal,
        Option<Decimal>,
        String,
    );

    async fn fetch_all(pool: &PgPool, coin: &str) -> Vec<CandleRow> {
        sqlx::query_as::<_, CandleRow>(
            "SELECT interval, ts, open, high, low, close, volume, source \
             FROM coin_candles WHERE coin_id = $1 AND vs_currency = 'usd' \
             ORDER BY interval, ts",
        )
        .bind(coin)
        .fetch_all(pool)
        .await
        .expect("fetch all candles")
    }

    async fn recv_within(listener: &mut PgListener, ms: u64) -> Option<String> {
        match tokio::time::timeout(std::time::Duration::from_millis(ms), listener.recv()).await {
            Ok(Ok(n)) => Some(n.payload().to_string()),
            // timeout OR listener error → treat as "no notification available"
            _ => None,
        }
    }

    // AC-REFACTOR-041a (parity): an N-row batched upsert produces rows identical to N single
    // `upsert_coin_candle` calls of the same inputs (includes a NULL-volume row).
    #[tokio::test]
    #[ignore]
    async fn db_batched_upsert_parity_with_single_upserts() {
        let pool = db_pool().await;
        let coin_a = "test-refactor001-parity-batch";
        let coin_b = "test-refactor001-parity-single";
        cleanup_coin(&pool, coin_a).await;
        cleanup_coin(&pool, coin_b).await;
        seed_tracked_coin(&pool, coin_a).await;
        seed_tracked_coin(&pool, coin_b).await;

        // Distinct OHLC per row + one NULL-volume row to exercise the array NULL path.
        let build = |coin: &str| -> Vec<CoinCandle> {
            vec![
                candle(
                    coin,
                    "1d",
                    0,
                    dec!(100),
                    dec!(110),
                    dec!(95),
                    dec!(105),
                    Some(dec!(10)),
                    "binance",
                ),
                candle(
                    coin,
                    "1d",
                    86_400,
                    dec!(105),
                    dec!(120),
                    dec!(104),
                    dec!(118),
                    None,
                    "binance",
                ),
                candle(
                    coin,
                    "5m",
                    300,
                    dec!(50),
                    dec!(51),
                    dec!(49),
                    dec!(50.5),
                    Some(dec!(3.5)),
                    "coinbase",
                ),
            ]
        };

        // Path A: one batched UNNEST upsert (native, silent).
        batched_upsert_coin_candles(
            &pool,
            &build(coin_a),
            CandleConflictPolicy::NativeOverwrite,
            CandleNotifyPolicy::Silent,
        )
        .await
        .expect("batched upsert");

        // Path B: N single upserts.
        for c in build(coin_b) {
            upsert_coin_candle(&pool, &c).await.expect("single upsert");
        }

        let rows_a = fetch_all(&pool, coin_a).await;
        let rows_b = fetch_all(&pool, coin_b).await;
        assert_eq!(
            rows_a, rows_b,
            "N-row batched upsert must equal N single upsert_coin_candle calls (row-for-row parity)"
        );
        assert_eq!(rows_a.len(), 3, "all three rows persisted");

        cleanup_coin(&pool, coin_a).await;
        cleanup_coin(&pool, coin_b).await;
    }

    // AC-REFACTOR-041a (native path): a native-write batch unconditionally overwrites a colliding
    // rollup row (the native path has NO rollup:% guard).
    #[tokio::test]
    #[ignore]
    async fn db_native_batch_overwrites_colliding_rollup_row() {
        let pool = db_pool().await;
        let coin = "test-refactor001-native-wins";
        cleanup_coin(&pool, coin).await;
        seed_tracked_coin(&pool, coin).await;

        batched_upsert_coin_candles(
            &pool,
            &[uniform(coin, 0, dec!(1), "rollup:5m")],
            CandleConflictPolicy::RollupGuarded,
            CandleNotifyPolicy::Silent,
        )
        .await
        .expect("seed rollup row");

        batched_upsert_coin_candles(
            &pool,
            &[uniform(coin, 0, dec!(999), "binance")],
            CandleConflictPolicy::NativeOverwrite,
            CandleNotifyPolicy::Silent,
        )
        .await
        .expect("native overwrite");

        let (src, close) = fetch_candle(&pool, coin, "1d", 0)
            .await
            .expect("row present");
        assert_eq!(
            src, "binance",
            "native batch must overwrite the colliding rollup row (unconditional DO UPDATE)"
        );
        assert_eq!(close, dec!(999));

        cleanup_coin(&pool, coin).await;
    }

    // AC-REFACTOR-041a (rollup path, D1): a rollup-write batch must NOT overwrite a native row —
    // the `WHERE coin_candles.source LIKE 'rollup:%'` guard makes the conflict a no-op.
    #[tokio::test]
    #[ignore]
    async fn db_rollup_batch_does_not_overwrite_native_row() {
        let pool = db_pool().await;
        let coin = "test-refactor001-rollup-guard";
        cleanup_coin(&pool, coin).await;
        seed_tracked_coin(&pool, coin).await;

        batched_upsert_coin_candles(
            &pool,
            &[uniform(coin, 0, dec!(500), "coingecko")],
            CandleConflictPolicy::NativeOverwrite,
            CandleNotifyPolicy::Silent,
        )
        .await
        .expect("seed native row");

        batched_upsert_coin_candles(
            &pool,
            &[uniform(coin, 0, dec!(111), "rollup:5m")],
            CandleConflictPolicy::RollupGuarded,
            CandleNotifyPolicy::Silent,
        )
        .await
        .expect("rollup no-op on native collision");

        let (src, close) = fetch_candle(&pool, coin, "1d", 0)
            .await
            .expect("row present");
        assert_eq!(
            src, "coingecko",
            "rollup batch must NOT overwrite a native row (native-wins guard, D1)"
        );
        assert_eq!(close, dec!(500));

        cleanup_coin(&pool, coin).await;
    }

    // AC-REFACTOR-042a (INTENDED CHANGE b): live-poll path emits exactly ONE NOTIFY for a single
    // candle; the backfill (Silent) path emits ZERO NOTIFYs — asserted via LISTEN.
    #[tokio::test]
    #[ignore]
    async fn db_live_path_emits_one_notify_backfill_emits_zero() {
        let pool = db_pool().await;
        let coin = "test-refactor001-notify";
        cleanup_coin(&pool, coin).await;
        seed_tracked_coin(&pool, coin).await;

        let mut listener = PgListener::connect_with(&pool)
            .await
            .expect("listener connect");
        listener
            .listen("coin_candle_updated")
            .await
            .expect("LISTEN coin_candle_updated");

        // Live path: exactly one candle → exactly one NOTIFY (per-event).
        batched_upsert_coin_candles(
            &pool,
            &[uniform(coin, 0, dec!(100), "binance")],
            CandleConflictPolicy::NativeOverwrite,
            CandleNotifyPolicy::PerEvent,
        )
        .await
        .expect("live upsert");

        assert!(
            recv_within(&mut listener, 1_000).await.is_some(),
            "live-poll candle upsert must emit exactly ONE NOTIFY"
        );
        assert!(
            recv_within(&mut listener, 300).await.is_none(),
            "a single-candle live upsert must emit NO second NOTIFY"
        );

        // Backfill path: a page of candles → ZERO NOTIFYs (INTENDED CHANGE b).
        let page: Vec<CoinCandle> = (1..4)
            .map(|i| uniform(coin, i * 86_400, dec!(200), "bitstamp"))
            .collect();
        batched_upsert_coin_candles(
            &pool,
            &page,
            CandleConflictPolicy::NativeOverwrite,
            CandleNotifyPolicy::Silent,
        )
        .await
        .expect("backfill upsert");

        assert!(
            recv_within(&mut listener, 500).await.is_none(),
            "backfill page write must emit ZERO NOTIFYs (historical rows must not broadcast)"
        );

        cleanup_coin(&pool, coin).await;
    }
}
