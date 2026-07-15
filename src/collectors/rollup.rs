//! Native `1d`/`1w` OHLCV rollup materializer (SPEC-CANDLE-001).
//!
//! Reuses `src/api/candles_agg.rs` unchanged for source selection (`select_source_interval`),
//! bucket alignment (`bucket_start` via `aggregate_candles`), and OHLCV folding
//! (`aggregate_candles` / `fold_volume`). The only post-processing applied here is relabeling
//! each returned row's `source` field to `rollup:<source_interval>` (REQ-CANDLE-003) — no
//! OHLCV/`ts`/`interval` value is recomputed (REQ-CANDLE-004).
//!
//! Two entry points:
//! - [`run_rollup`]: DB-backed, network-free orchestration invoked from the
//!   `("coin","rollup")` collection-queue dispatch arm (REQ-CANDLE-024).
//! - [`materialize_from_source`] / [`reconcile_window`]: pure, hermetically testable core
//!   logic (no SQL, no clock reads) — this is what the reproduction-first unit tests exercise.

use std::collections::HashSet;

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;

use crate::api::candles_agg::{
    aggregate_candles, bucket_start, interval_to_seconds, select_source_interval, IntervalCoverage,
};
use crate::models::quote::CoinCandle;

/// Rollup source-marker prefix (REQ-CANDLE-003), distinct from the ephemeral read-time
/// `aggregated:<label>` marker that `aggregate_candles` stamps in-memory (never persisted).
pub const ROLLUP_SOURCE_PREFIX: &str = "rollup:";

/// Fixed vs_currency for the rollup materializer, matching the hardcoded `"usd"` convention
/// used by the `("coin","candles")` dispatch arm's `MarketQuery.vs_currency`.
pub const ROLLUP_VS_CURRENCY: &str = "usd";

/// Target intervals materialized by this SPEC, paired with their fixed-second duration.
const TARGET_INTERVALS: [(&str, i64); 2] = [("1d", 86_400), ("1w", 604_800)];

/// Week-aligned chunk size for the full-history backfill walk (REQ-CANDLE-011): a bounded
/// multiple of 604800s so no `1d` or `1w` bucket ever straddles a chunk boundary, keeping
/// per-window memory flat regardless of total history length (REQ-CANDLE-012).
const BACKFILL_CHUNK_WEEKS: i64 = 4;
const WEEK_SECS: i64 = 604_800;

// ── Pure core (hermetically testable; no SQL, no clock reads) ────────────────────────────

/// Fold `source` into `target_interval` buckets via `aggregate_candles` (unchanged bucketing
/// math), then relabel each returned row's `source` field to `rollup:<source_interval>`
/// (REQ-CANDLE-001/002/003/004/005).
///
// @MX:ANCHOR: [AUTO] materialize_from_source — rollup entry point reused by backfill,
//             incremental recompute, and the pure unit tests.
// @MX:REASON: fan_in >= 3: backfill_target, incremental_recompute_target, unit tests.
//             The relabel MUST overwrite only `source`; ts/interval/OHLCV values are exactly
//             what `aggregate_candles` produced (REQ-CANDLE-003/004) — never recompute them.
// @MX:SPEC: SPEC-CANDLE-001 REQ-CANDLE-001 REQ-CANDLE-002 REQ-CANDLE-003 REQ-CANDLE-004 REQ-CANDLE-005
pub fn materialize_from_source(
    source: Vec<CoinCandle>,
    target_secs: i64,
    source_secs: i64,
    now: DateTime<Utc>,
    source_interval: &str,
    target_interval: &str,
) -> Vec<CoinCandle> {
    let mut rows = aggregate_candles(
        source,
        target_secs,
        source_secs,
        now,
        source_interval,
        target_interval,
    );
    let label = format!("{ROLLUP_SOURCE_PREFIX}{source_interval}");
    for row in &mut rows {
        row.source = label.clone();
    }
    rows
}

/// Compute the forward-only window-reconcile (REQ-CANDLE-022): given the previously
/// materialized rows in a bounded recompute window and the freshly emitted set for that same
/// window, return `(upserts, deletes)`.
///
/// - `upserts` = every row `emitted` still produces (re-upserted in place, even if unchanged).
/// - `deletes` = timestamps present in `previously_materialized` but absent from `emitted`
///   (e.g. a forming partial bucket that later closed incomplete and was dropped).
///
/// This bounded reconcile is what makes REQ-CANDLE-005's set-parity hold across recompute
/// runs without a full-history rescan.
pub fn reconcile_window(
    previously_materialized: &[CoinCandle],
    emitted: &[CoinCandle],
) -> (Vec<CoinCandle>, Vec<DateTime<Utc>>) {
    let emitted_ts: HashSet<i64> = emitted.iter().map(|c| c.ts.timestamp()).collect();
    let deletes: Vec<DateTime<Utc>> = previously_materialized
        .iter()
        .filter(|c| !emitted_ts.contains(&c.ts.timestamp()))
        .map(|c| c.ts)
        .collect();
    (emitted.to_vec(), deletes)
}

// ── Batched, partition-safe insert (REQ-CANDLE-043) ───────────────────────────────────────

/// Batched upsert of rollup rows, avoiding the per-row transaction + `pg_notify` overhead of
/// `upsert_coin_candle` (REQ-CANDLE-043). Preserves the identical
/// `(coin_id, vs_currency, interval, ts)` conflict target, so parity and idempotency with the
/// row-at-a-time path are unaffected.
///
// @MX:NOTE: [AUTO] batched_upsert_candles — must not fork candles_agg.rs folding; must
//           preserve volume null-propagation. The batch is a single UNNEST-based INSERT (one
//           round trip, one tx) rather than N single-row upserts — do not revert to a per-row
//           loop for historical backfill sizes (thousands of `1d` + hundreds of `1w` rows per
//           coin). coin_candles is a plain table since migration 0020, so no partition-ensure
//           step is needed for `ts` values outside any static range.
// @MX:SPEC: SPEC-CANDLE-001 REQ-CANDLE-013 REQ-CANDLE-040 REQ-CANDLE-043
pub async fn batched_upsert_candles(
    pool: &PgPool,
    candles: &[CoinCandle],
) -> Result<(), sqlx::Error> {
    if candles.is_empty() {
        return Ok(());
    }

    let coin_ids: Vec<&str> = candles.iter().map(|c| c.coin_id.as_str()).collect();
    let vs_currencies: Vec<&str> = candles.iter().map(|c| c.vs_currency.as_str()).collect();
    let intervals: Vec<&str> = candles.iter().map(|c| c.interval.as_str()).collect();
    let tss: Vec<DateTime<Utc>> = candles.iter().map(|c| c.ts).collect();
    let opens: Vec<rust_decimal::Decimal> = candles.iter().map(|c| c.open).collect();
    let highs: Vec<rust_decimal::Decimal> = candles.iter().map(|c| c.high).collect();
    let lows: Vec<rust_decimal::Decimal> = candles.iter().map(|c| c.low).collect();
    let closes: Vec<rust_decimal::Decimal> = candles.iter().map(|c| c.close).collect();
    let volumes: Vec<Option<rust_decimal::Decimal>> = candles.iter().map(|c| c.volume).collect();
    let sources: Vec<&str> = candles.iter().map(|c| c.source.as_str()).collect();

    sqlx::query(
        "INSERT INTO coin_candles \
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
            source = EXCLUDED.source",
    )
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
    .execute(pool)
    .await?;

    Ok(())
}

// ── DB orchestration ───────────────────────────────────────────────────────────────────────

/// Per-interval coverage probe, shared shape with `candles.rs` / `cycle_overlay.rs`.
async fn coverage_for(
    pool: &PgPool,
    coin_id: &str,
    vs_currency: &str,
) -> Result<Vec<(String, DateTime<Utc>, DateTime<Utc>)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT interval, MIN(ts), MAX(ts) FROM coin_candles \
         WHERE coin_id = $1 AND vs_currency = $2 GROUP BY interval",
    )
    .bind(coin_id)
    .bind(vs_currency)
    .fetch_all(pool)
    .await
}

/// Full-history backfill (REQ-CANDLE-010/011/012/013): walk `[earliest .. now]` in
/// week-aligned windows, loading only each window's source rows before folding, so the
/// per-window row count stays bounded regardless of total history length.
#[allow(clippy::too_many_arguments)]
async fn backfill_target(
    pool: &PgPool,
    coin_id: &str,
    vs_currency: &str,
    target_interval: &str,
    target_secs: i64,
    source_interval: &str,
    source_secs: i64,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let earliest: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT MIN(ts) FROM coin_candles WHERE coin_id = $1 AND vs_currency = $2 AND interval = $3",
    )
    .bind(coin_id)
    .bind(vs_currency)
    .bind(source_interval)
    .fetch_one(pool)
    .await?;

    let Some(earliest) = earliest else {
        return Ok(());
    };

    let chunk_secs = WEEK_SECS * BACKFILL_CHUNK_WEEKS;
    let mut window_start = bucket_start(earliest, WEEK_SECS);
    let now_epoch = now.timestamp();

    while window_start.timestamp() <= now_epoch {
        let window_end = window_start + Duration::seconds(chunk_secs);

        let source_rows: Vec<CoinCandle> = sqlx::query_as(
            "SELECT coin_id, vs_currency, interval, ts, open, high, low, close, volume, source \
             FROM coin_candles \
             WHERE coin_id = $1 AND vs_currency = $2 AND interval = $3 \
               AND ts >= $4 AND ts < $5 \
             ORDER BY ts ASC",
        )
        .bind(coin_id)
        .bind(vs_currency)
        .bind(source_interval)
        .bind(window_start)
        .bind(window_end)
        .fetch_all(pool)
        .await?;

        if !source_rows.is_empty() {
            let rows = materialize_from_source(
                source_rows,
                target_secs,
                source_secs,
                now,
                source_interval,
                target_interval,
            );
            batched_upsert_candles(pool, &rows).await?;
        }

        window_start = window_end;
    }

    Ok(())
}

/// Forward-only incremental recompute (REQ-CANDLE-020/021/022/023): reload source only from
/// the max-materialized bucket forward, re-upsert every bucket `aggregate_candles` emits for
/// the window, and delete any previously-materialized bucket the reconcile no longer emits.
/// First run for a coin/interval with no materialized rows falls back to a full backfill
/// (REQ-CANDLE-010).
#[allow(clippy::too_many_arguments)]
async fn incremental_recompute_target(
    pool: &PgPool,
    coin_id: &str,
    vs_currency: &str,
    target_interval: &str,
    target_secs: i64,
    source_interval: &str,
    source_secs: i64,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let max_bucket: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT MAX(ts) FROM coin_candles \
         WHERE coin_id = $1 AND vs_currency = $2 AND interval = $3 AND source LIKE 'rollup:%'",
    )
    .bind(coin_id)
    .bind(vs_currency)
    .bind(target_interval)
    .fetch_one(pool)
    .await?;

    let Some(recompute_start) = max_bucket else {
        return backfill_target(
            pool,
            coin_id,
            vs_currency,
            target_interval,
            target_secs,
            source_interval,
            source_secs,
            now,
        )
        .await;
    };

    let previously_materialized: Vec<CoinCandle> = sqlx::query_as(
        "SELECT coin_id, vs_currency, interval, ts, open, high, low, close, volume, source \
         FROM coin_candles \
         WHERE coin_id = $1 AND vs_currency = $2 AND interval = $3 AND ts >= $4",
    )
    .bind(coin_id)
    .bind(vs_currency)
    .bind(target_interval)
    .bind(recompute_start)
    .fetch_all(pool)
    .await?;

    let source_rows: Vec<CoinCandle> = sqlx::query_as(
        "SELECT coin_id, vs_currency, interval, ts, open, high, low, close, volume, source \
         FROM coin_candles \
         WHERE coin_id = $1 AND vs_currency = $2 AND interval = $3 AND ts >= $4",
    )
    .bind(coin_id)
    .bind(vs_currency)
    .bind(source_interval)
    .bind(recompute_start)
    .fetch_all(pool)
    .await?;

    let emitted = materialize_from_source(
        source_rows,
        target_secs,
        source_secs,
        now,
        source_interval,
        target_interval,
    );

    let (upserts, deletes) = reconcile_window(&previously_materialized, &emitted);

    if !upserts.is_empty() {
        batched_upsert_candles(pool, &upserts).await?;
    }

    for ts in deletes {
        sqlx::query(
            "DELETE FROM coin_candles \
             WHERE coin_id = $1 AND vs_currency = $2 AND interval = $3 AND ts = $4",
        )
        .bind(coin_id)
        .bind(vs_currency)
        .bind(target_interval)
        .bind(ts)
        .execute(pool)
        .await?;
    }

    Ok(())
}

/// Rollup materializer entry point (REQ-CANDLE-001/024): network-free, DB-only. Invoked from
/// the `("coin","rollup")` collection-queue dispatch arm, mirroring the `("coin",
/// "cycle_overlay")` precedent — no provider, no pacer.
pub async fn run_rollup(
    pool: &PgPool,
    coin_id: &str,
    vs_currency: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    // Per-coin source coverage is independent of the target interval, so probe it once and
    // reuse it across every TARGET_INTERVALS iteration rather than re-running the (full-history,
    // partition-unprunable) coverage query per target.
    let coverage_rows = coverage_for(pool, coin_id, vs_currency).await?;
    let coverage: Vec<IntervalCoverage> = coverage_rows
        .iter()
        .map(|(iv, earliest, latest)| IntervalCoverage {
            interval: iv.as_str(),
            earliest: *earliest,
            latest: *latest,
        })
        .collect();

    for (target_interval, target_secs) in TARGET_INTERVALS {
        // REQ-CANDLE-001: same selector the read path uses, called with window_start=None
        // (materialize the full-history canonical series).
        let Some(source_interval) = select_source_interval(&coverage, target_secs, None, now)
        else {
            // No divisible source interval — zero materialized rows; read-time aggregation
            // fallback remains in place (REQ-CANDLE-031).
            continue;
        };
        let source_interval = source_interval.to_string();
        let source_secs = interval_to_seconds(&source_interval)
            .expect("select_source_interval only returns intervals known to interval_to_seconds");

        incremental_recompute_target(
            pool,
            coin_id,
            vs_currency,
            target_interval,
            target_secs,
            &source_interval,
            source_secs,
            now,
        )
        .await?;
    }

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;
    use rust_decimal_macros::dec;

    fn ts_epoch(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(secs, 0).unwrap()
    }

    fn make_5m(
        ts: DateTime<Utc>,
        open: rust_decimal::Decimal,
        high: rust_decimal::Decimal,
        low: rust_decimal::Decimal,
        close: rust_decimal::Decimal,
        volume: Option<rust_decimal::Decimal>,
    ) -> CoinCandle {
        CoinCandle {
            coin_id: "bitcoin".into(),
            vs_currency: "usd".into(),
            interval: "5m".into(),
            ts,
            open,
            high,
            low,
            close,
            volume,
            source: "binance".into(),
        }
    }

    // ── Reproduction test 1 (pure): rollup unit test ───────────────────────────────────────
    // Given known 5m candles spanning several days (incl. one day with a NULL-volume source
    // candle), the materializer must produce exact 1d OHLC + the rollup:5m marker, and 1w
    // buckets must be epoch-Thursday-anchored.

    #[test]
    fn materialize_from_source_1d_ohlc_and_rollup_marker() {
        // Day 0 = epoch [0, 86400): two 5m candles.
        // Day 1 = epoch [86400, 172800): two 5m candles, one with NULL volume.
        let now = ts_epoch(1_000_000_000); // far future -> all buckets closed
        let n = 86_400 / 300; // 288 expected 5m candles per complete 1d bucket

        // Build a complete day-0 bucket (288 candles) so it is not dropped as incomplete.
        let mut source: Vec<CoinCandle> = Vec::new();
        for i in 0..n {
            let ts = ts_epoch(i * 300);
            source.push(make_5m(
                ts,
                dec!(100),
                dec!(105),
                dec!(95),
                dec!(102),
                Some(dec!(10)),
            ));
        }
        // Override open (first) / close (last) / high / low for day 0 to known values.
        source[0].open = dec!(100);
        source[0].high = dec!(101);
        source[0].low = dec!(99);
        let last_idx = source.len() - 1;
        source[last_idx].close = dec!(110);
        source[last_idx].high = dec!(112); // new max
        source[last_idx].low = dec!(90); // new min

        // Build a complete day-1 bucket (288 candles) with one NULL-volume candle.
        let day1_start = 86_400i64;
        for i in 0..n {
            let ts = ts_epoch(day1_start + i * 300);
            let vol = if i == 5 { None } else { Some(dec!(20)) };
            source.push(make_5m(ts, dec!(200), dec!(205), dec!(195), dec!(202), vol));
        }

        let agg = materialize_from_source(source, 86_400, 300, now, "5m", "1d");

        assert_eq!(agg.len(), 2, "two complete 1d buckets expected");

        // ts DESC: day1 first, day0 second.
        let day1 = &agg[0];
        let day0 = &agg[1];

        assert_eq!(day0.ts.timestamp(), 0, "day 0 bucket_start = epoch 0");
        assert_eq!(day0.open, dec!(100), "day0 open = first-in-bucket");
        assert_eq!(day0.close, dec!(110), "day0 close = last-in-bucket");
        assert_eq!(day0.high, dec!(112), "day0 high = max across bucket");
        assert_eq!(day0.low, dec!(90), "day0 low = min across bucket");
        assert_eq!(
            day0.volume,
            Some(dec!(10) * rust_decimal::Decimal::from(n)),
            "day0 volume = sum of all present component volumes"
        );
        assert_eq!(
            day0.source, "rollup:5m",
            "REQ-CANDLE-003: rollup marker must be rollup:<source_interval>, never aggregated:"
        );
        assert_eq!(day0.interval, "1d");

        assert_eq!(day1.ts.timestamp(), 86_400, "day 1 bucket_start");
        assert!(
            day1.volume.is_none(),
            "REQ-CANDLE-004: any NULL-volume component must null-propagate the bucket total"
        );
        assert_eq!(day1.source, "rollup:5m");
    }

    // 1w buckets must be epoch-Thursday-anchored (REQ-CANDLE-002), not ISO Monday.
    #[test]
    fn materialize_from_source_1w_epoch_thursday_anchored() {
        let now = ts_epoch(1_000_000_000);
        let n = 604_800 / 300; // complete 1w bucket needs 2016 5m candles

        let mut source: Vec<CoinCandle> = Vec::new();
        for i in 0..n {
            let ts = ts_epoch(i * 300);
            source.push(make_5m(
                ts,
                dec!(1),
                dec!(2),
                dec!(1),
                dec!(2),
                Some(dec!(1)),
            ));
        }

        let agg = materialize_from_source(source, 604_800, 300, now, "5m", "1w");

        assert_eq!(agg.len(), 1, "one complete 1w bucket expected");
        assert_eq!(agg[0].ts.timestamp(), 0, "epoch 0 is the first 1w bucket");
        assert_eq!(
            agg[0].ts.weekday(),
            chrono::Weekday::Thu,
            "1w bucket must be epoch-Thursday-anchored, not ISO Monday"
        );
        assert_eq!(agg[0].source, "rollup:5m");
    }

    // Forming bucket policy carries through unchanged (REQ-CANDLE-005): emitted even partial.
    #[test]
    fn materialize_from_source_forming_bucket_emitted_partial() {
        let now = ts_epoch(3_600); // 1h into the forming 1d bucket
        let source = vec![make_5m(
            ts_epoch(0),
            dec!(50),
            dec!(55),
            dec!(45),
            dec!(52),
            Some(dec!(5)),
        )];

        let agg = materialize_from_source(source, 86_400, 300, now, "5m", "1d");

        assert_eq!(
            agg.len(),
            1,
            "forming bucket must be emitted even if partial"
        );
        assert_eq!(agg[0].source, "rollup:5m");
    }

    // ── Reproduction test 2 (pure): incremental-update / window-reconcile ──────────────────
    // Adding a new 5m candle inside an existing forming day updates only that day's 1d bucket;
    // other days are never touched because the recompute window only includes source rows
    // from the forming day forward (no full-history rescan by construction).

    #[test]
    fn incremental_recompute_updates_only_the_forming_day() {
        let day0_start = 0i64;
        let day1_start = 86_400i64;

        // Previously materialized state: day0 (closed, complete, from a prior run) is NOT
        // included in the recompute window at all — proving no full rescan is needed to
        // preserve it. Day1 has a stale partial forming-bucket snapshot from a prior run
        // (only 1 of N candles).
        let day1_old_partial = CoinCandle {
            source: "rollup:5m".into(),
            ts: ts_epoch(day1_start),
            ..make_5m(
                ts_epoch(day1_start),
                dec!(200),
                dec!(200),
                dec!(200),
                dec!(200),
                Some(dec!(1)),
            )
        };
        let previously_materialized = vec![day1_old_partial.clone()];

        // Forward-only recompute reloads source from day1 forward only (the recompute
        // window) — day0's source candles are never fetched, so day0 cannot be touched.
        let now = ts_epoch(day1_start + 600); // still forming day1 (10 min in)
        let recompute_source = vec![
            make_5m(
                ts_epoch(day1_start),
                dec!(200),
                dec!(205),
                dec!(195),
                dec!(202),
                Some(dec!(10)),
            ),
            make_5m(
                ts_epoch(day1_start + 300),
                dec!(202),
                dec!(210),
                dec!(198),
                dec!(208),
                Some(dec!(12)),
            ),
        ];

        let emitted = materialize_from_source(recompute_source, 86_400, 300, now, "5m", "1d");
        let (upserts, deletes) = reconcile_window(&previously_materialized, &emitted);

        assert_eq!(upserts.len(), 1, "only the forming day1 bucket is emitted");
        assert_eq!(upserts[0].ts.timestamp(), day1_start);
        assert_eq!(
            upserts[0].close,
            dec!(208),
            "close reflects the newly added 5m candle"
        );
        assert!(
            deletes.is_empty(),
            "the forming bucket is still emitted (partial) -> no delete"
        );
        assert!(
            upserts.iter().all(|c| c.ts.timestamp() != day0_start),
            "day0 must never appear in the recompute output — proves no full rescan"
        );
    }

    // REQ-CANDLE-022: a forming bucket that later closes incomplete must be deleted by the
    // bounded window-reconcile on the next recompute (set-parity across runs).
    #[test]
    fn reconcile_window_deletes_forming_bucket_that_closed_incomplete() {
        let bucket_ts = ts_epoch(0);
        let previously_materialized = vec![CoinCandle {
            source: "rollup:5m".into(),
            ..make_5m(bucket_ts, dec!(1), dec!(2), dec!(1), dec!(2), Some(dec!(1)))
        }];

        // Bucket is now closed (now far past bucket_end) but incomplete (missing candles) ->
        // aggregate_candles drops it -> emitted is empty for this bucket.
        let emitted: Vec<CoinCandle> = vec![];

        let (upserts, deletes) = reconcile_window(&previously_materialized, &emitted);

        assert!(upserts.is_empty());
        assert_eq!(
            deletes,
            vec![bucket_ts],
            "REQ-CANDLE-022: non-emitted previously-materialized bucket must be deleted"
        );
    }

    #[test]
    fn reconcile_window_no_changes_when_previously_materialized_matches_emitted() {
        let bucket_ts = ts_epoch(0);
        let row = CoinCandle {
            source: "rollup:5m".into(),
            ..make_5m(bucket_ts, dec!(1), dec!(2), dec!(1), dec!(2), Some(dec!(1)))
        };
        let previously_materialized = vec![row.clone()];
        let emitted = vec![row];

        let (upserts, deletes) = reconcile_window(&previously_materialized, &emitted);
        assert_eq!(upserts.len(), 1);
        assert!(deletes.is_empty());
    }
}
