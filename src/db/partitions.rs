//! Runtime partition management for `coin_candles` (SPEC-SCHED-001 10-year range-backfill).
//!
//! `coin_candles` is `RANGE(ts)` partitioned, one partition per calendar month
//! (migrations/0011_remove_markets.sql). The static migration only declares
//! partitions for 2024-01 through 2027-12; a historical backfill reaching further
//! into the past requires creating the covering monthly partition at runtime,
//! before the insert, or the insert fails with "no partition of relation found".

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Datelike, TimeZone, Utc};
use sqlx::PgPool;

/// In-process cache of `(year, month)` pairs already confirmed to have a `coin_candles`
/// partition, so the hot insert path (`upsert_coin_candle`) skips the DDL round-trip
/// after the first successful call for a given month.
fn ensured_months() -> &'static Mutex<HashSet<(i32, u32)>> {
    static CACHE: OnceLock<Mutex<HashSet<(i32, u32)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Ensure a monthly `coin_candles` partition exists for the UTC calendar month
/// containing `ts`, creating it if necessary.
///
/// Parent-level indexes (btree + BRIN, migrations/0011) are inherited automatically
/// by new partitions — no per-partition index DDL is required.
///
// @MX:WARN: [AUTO] ensure_candle_partition — runtime DDL on the candle insert path
// @MX:REASON: Multiple backfill/live-poller workers (or replicas) may race to create
//             the same monthly partition concurrently. Guarded by a transaction-scoped
//             Postgres advisory lock keyed on the partition name
//             (pg_advisory_xact_lock(hashtext(name))) so concurrent callers serialize
//             instead of racing on CREATE TABLE (which can otherwise raise 42P07/23505/
//             "tuple concurrently updated" under concurrent DDL). The DDL runs in its
//             own transaction, separate from the caller's insert transaction, so the
//             lock is held only for the brief DDL check, not across the hot insert path.
// @MX:SPEC: SPEC-SCHED-001 (10-year range-backfill; coin_candles has no partitions before 2024-01)
pub async fn ensure_candle_partition(pool: &PgPool, ts: DateTime<Utc>) -> Result<(), sqlx::Error> {
    let year = ts.year();
    let month = ts.month();

    if ensured_months().lock().unwrap().contains(&(year, month)) {
        return Ok(());
    }

    let (from, to) = month_bounds(year, month);
    let partition_name = partition_name_for(year, month);

    let mut tx = pool.begin().await?;

    // Advisory lock keyed on the partition name: serializes concurrent creators of
    // the same month across replicas/workers without blocking unrelated months.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(&partition_name)
        .execute(&mut *tx)
        .await?;

    let ddl = format!(
        "CREATE TABLE IF NOT EXISTS {partition_name} PARTITION OF coin_candles \
         FOR VALUES FROM ('{}') TO ('{}')",
        from.to_rfc3339(),
        to.to_rfc3339(),
    );
    // `ddl` is built entirely from validated integers (year/month), never external
    // input — audited safe for sqlx 0.9's dynamic-SQL lint.
    sqlx::query(sqlx::AssertSqlSafe(ddl))
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    ensured_months().lock().unwrap().insert((year, month));
    Ok(())
}

/// `coin_candles_YYYY_MM` — matches the static naming in migrations/0011.
fn partition_name_for(year: i32, month: u32) -> String {
    format!("coin_candles_{year:04}_{month:02}")
}

/// UTC month bounds `[from, to)` for the calendar month containing `(year, month)`.
fn month_bounds(year: i32, month: u32) -> (DateTime<Utc>, DateTime<Utc>) {
    let from = Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0).unwrap();
    let to = if month == 12 {
        Utc.with_ymd_and_hms(year + 1, 1, 1, 0, 0, 0).unwrap()
    } else {
        Utc.with_ymd_and_hms(year, month + 1, 1, 0, 0, 0).unwrap()
    };
    (from, to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn partition_name_matches_static_migration_convention() {
        assert_eq!(partition_name_for(2024, 1), "coin_candles_2024_01");
        assert_eq!(partition_name_for(2016, 9), "coin_candles_2016_09");
        assert_eq!(partition_name_for(2027, 12), "coin_candles_2027_12");
    }

    #[test]
    fn month_bounds_covers_calendar_month() {
        let (from, to) = month_bounds(2016, 9);
        assert_eq!(from, Utc.with_ymd_and_hms(2016, 9, 1, 0, 0, 0).unwrap());
        assert_eq!(to, Utc.with_ymd_and_hms(2016, 10, 1, 0, 0, 0).unwrap());
    }

    #[test]
    fn month_bounds_rolls_over_year_at_december() {
        let (from, to) = month_bounds(2016, 12);
        assert_eq!(from, Utc.with_ymd_and_hms(2016, 12, 1, 0, 0, 0).unwrap());
        assert_eq!(to, Utc.with_ymd_and_hms(2017, 1, 1, 0, 0, 0).unwrap());
    }
}
