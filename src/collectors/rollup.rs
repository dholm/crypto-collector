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
//!
//! Data-integrity hardening (SPEC-CANDLE-002):
//! - **Source-filter invariant (REQ-CANDLE-050/051):** the incremental reconcile reads its
//!   `previously_materialized` set AND issues its per-`ts` DELETE scoped to
//!   `source LIKE 'rollup:%'` only — the same filter `recompute_start`'s `MAX(ts)` query
//!   carries. Native provider rows (any non-`rollup:*` `source`) are never read as
//!   materialized and never deleted. Combined with the native-wins upsert
//!   ([`batched_upsert_candles`], REQ-CANDLE-052) this is a three-layer defense: a derived
//!   materializer can never destroy or overwrite the native rows it derives from.
//! - **Source low-watermark history repair (REQ-CANDLE-054..057):** the reconcile is no
//!   longer strictly forward-only. After the forward recompute it compares the source
//!   `MIN(ts)` against the earliest materialized `rollup:*` bucket ([`backward_repair_window`],
//!   query-derived — no migration); when source history precedes materialization it runs a
//!   bounded, week-aligned backward pass ([`materialize_window_walk`], the same memory-bounded
//!   walk the full-history backfill uses).

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

/// Pure decision core for the F-08 source low-watermark history repair
/// (SPEC-CANDLE-002 REQ-CANDLE-054/055): given the source interval's low-watermark
/// (`source_min_ts` = `MIN(ts)` of the source rows) and the earliest already-materialized
/// `rollup:*` bucket (`earliest_materialized_ts`), return the bounded backward window
/// `[week-aligned(source_min), earliest_materialized)` to repair, or `None` when the source
/// does not precede existing materialization.
///
/// Week-aligning the start (via `bucket_start(_, WEEK_SECS)`) reuses the same chunk boundary
/// the backfill/repair walk uses, so no `1d`/`1w` bucket straddles a chunk edge. The pass is
/// self-terminating for aligned/`1w` cases: once the earliest materialized bucket has moved
/// back to the source low-watermark bucket, the next call returns `None`; any residual overlap
/// re-materializes identical `rollup:*` buckets idempotently (native-wins protects any native
/// row). This is the AC-CANDLE-055 pure-test target — DB-free by construction.
pub fn backward_repair_window(
    source_min_ts: DateTime<Utc>,
    earliest_materialized_ts: DateTime<Utc>,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let start = bucket_start(source_min_ts, WEEK_SECS);
    if start < earliest_materialized_ts {
        Some((start, earliest_materialized_ts))
    } else {
        None
    }
}

// ── Batched, partition-safe insert (REQ-CANDLE-043) ───────────────────────────────────────

/// Batched upsert of rollup rows, avoiding the per-row transaction + `pg_notify` overhead of
/// `upsert_coin_candle` (REQ-CANDLE-043). Preserves the identical
/// `(coin_id, vs_currency, interval, ts)` conflict target, so parity and idempotency with the
/// row-at-a-time path are unaffected.
///
// @MX:ANCHOR: [AUTO] batched_upsert_candles native-wins collision contract — the
//             `ON CONFLICT ... DO UPDATE ... WHERE coin_candles.source LIKE 'rollup:%'` guard
//             upgrades ONLY a prior rollup row; a colliding native provider row is left
//             byte-identical (the WHERE is false → the conflict is a no-op). Every write path
//             (forward recompute, full backfill, backward repair) routes through here, so this
//             is the single enforcement point of Decision D1.
// @MX:REASON: fan_in >= 3 (backfill/repair walk, incremental recompute, DB tests) AND a
//             data-integrity invariant: a derived materializer MUST NOT overwrite genuine
//             provider data. Removing the WHERE re-opens the F-07 native-row-destruction path.
// @MX:SPEC: SPEC-CANDLE-002 REQ-CANDLE-052
// @MX:NOTE: [AUTO] batched_upsert_candles — must not fork candles_agg.rs folding; must
//           preserve volume null-propagation. The batch is a single UNNEST-based INSERT (one
//           round trip, one tx) rather than N single-row upserts — do not revert to a per-row
//           loop for historical backfill sizes (thousands of `1d` + hundreds of `1w` rows per
//           coin). coin_candles is a plain table since migration 0020, so no partition-ensure
//           step is needed for `ts` values outside any static range.
// @MX:SPEC: SPEC-CANDLE-001 REQ-CANDLE-013 REQ-CANDLE-040 REQ-CANDLE-043 SPEC-CANDLE-002 REQ-CANDLE-052
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
            source = EXCLUDED.source \
         WHERE coin_candles.source LIKE 'rollup:%'",
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

/// Walk `[window_start, ceiling]` in week-aligned `BACKFILL_CHUNK_WEEKS`-wide chunks,
/// loading only each window's source rows before folding + upserting — the shared
/// memory-bounded walk (REQ-CANDLE-011/012) reused by BOTH the full-history backfill and the
/// F-08 backward-repair pass (SPEC-CANDLE-002 REQ-CANDLE-056). `window_start` MUST already be
/// week-aligned (`bucket_start(_, WEEK_SECS)`), so no `1d`/`1w` bucket straddles a chunk edge.
///
// @MX:WARN: [AUTO] materialize_window_walk — memory-bounded: each iteration loads exactly one
//           week-aligned chunk of source rows, never the full source series. The backward-repair
//           range can span years, so widening the window or fetching the whole range at once
//           would OOM-kill the pod.
// @MX:REASON: OOM prevention — a coin's full candle history is ~1M rows; loading it into the
//             256 Mi pod OOM-kills it. The chunked walk is the invariant that keeps per-window
//             memory flat regardless of how deep the repaired history reaches.
// @MX:SPEC: SPEC-CANDLE-001 REQ-CANDLE-011 REQ-CANDLE-012 SPEC-CANDLE-002 REQ-CANDLE-055 REQ-CANDLE-056
#[allow(clippy::too_many_arguments)]
async fn materialize_window_walk(
    pool: &PgPool,
    coin_id: &str,
    vs_currency: &str,
    target_interval: &str,
    target_secs: i64,
    source_interval: &str,
    source_secs: i64,
    now: DateTime<Utc>,
    mut window_start: DateTime<Utc>,
    ceiling: DateTime<Utc>,
) -> anyhow::Result<()> {
    let chunk_secs = WEEK_SECS * BACKFILL_CHUNK_WEEKS;
    let ceiling_epoch = ceiling.timestamp();

    while window_start.timestamp() <= ceiling_epoch {
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

/// Full-history backfill (REQ-CANDLE-010/011/012/013): walk `[earliest .. now]` in
/// week-aligned windows via [`materialize_window_walk`], loading only each window's source
/// rows before folding, so the per-window row count stays bounded regardless of total history
/// length.
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

    materialize_window_walk(
        pool,
        coin_id,
        vs_currency,
        target_interval,
        target_secs,
        source_interval,
        source_secs,
        now,
        bucket_start(earliest, WEEK_SECS),
        now,
    )
    .await
}

/// Incremental recompute (REQ-CANDLE-020/021/022/023): reload source only from the
/// max-materialized bucket forward, re-upsert every bucket `aggregate_candles` emits for the
/// window, and delete any previously-materialized bucket the reconcile no longer emits. First
/// run for a coin/interval with no materialized rows falls back to a full backfill
/// (REQ-CANDLE-010). After the forward pass it runs the F-08 backward repair when source
/// history precedes the earliest materialized bucket (SPEC-CANDLE-002 REQ-CANDLE-054/055).
///
// @MX:ANCHOR: [AUTO] incremental_recompute_target source-filter invariant — the reconcile's
//             `previously_materialized` read AND its per-`ts` DELETE are BOTH scoped to
//             `source LIKE 'rollup:%'`, the same filter `recompute_start`'s `MAX(ts)` query
//             carries. Native provider rows are never read as materialized (so never enter the
//             reconcile's delete set) and never deleted. `reconcile_window` stays pure — the
//             domain narrowing happens at these SQL boundaries.
// @MX:REASON: data-loss prevention — without both filters a native `1d` row in the recompute
//             window is treated as a stale rollup bucket and destroyed (the F-07 defect). This
//             is a fan_in / invariant contract: every rollup reconcile passes through here.
// @MX:SPEC: SPEC-CANDLE-002 REQ-CANDLE-050 REQ-CANDLE-051 REQ-CANDLE-053 REQ-CANDLE-054 REQ-CANDLE-055
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

    // REQ-CANDLE-050: scope the previously-materialized read to rollup rows only — the same
    // `source LIKE 'rollup:%'` filter the `MAX(ts)` query above carries. A native provider row
    // in `[recompute_start, now]` must NOT be seen as materialized (else the reconcile would
    // delete it as a non-emitted bucket). `reconcile_window` stays pure; this SQL narrows its
    // input domain to rollup-owned rows.
    let previously_materialized: Vec<CoinCandle> = sqlx::query_as(
        "SELECT coin_id, vs_currency, interval, ts, open, high, low, close, volume, source \
         FROM coin_candles \
         WHERE coin_id = $1 AND vs_currency = $2 AND interval = $3 AND ts >= $4 \
           AND source LIKE 'rollup:%'",
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
        // REQ-CANDLE-051: belt-and-suspenders — even though `previously_materialized` is now
        // rollup-only (so `deletes` can only carry rollup `ts`), scope the DELETE to
        // `source LIKE 'rollup:%'` so a native row sharing a `ts` can never be removed.
        sqlx::query(
            "DELETE FROM coin_candles \
             WHERE coin_id = $1 AND vs_currency = $2 AND interval = $3 AND ts = $4 \
               AND source LIKE 'rollup:%'",
        )
        .bind(coin_id)
        .bind(vs_currency)
        .bind(target_interval)
        .bind(ts)
        .execute(pool)
        .await?;
    }

    // ── F-08 source low-watermark history repair (SPEC-CANDLE-002 REQ-CANDLE-054..057) ──
    // The forward recompute above only walks `[recompute_start, now]`, so source rows that
    // arrived BEHIND the earliest materialized bucket (a deep backfill completing after the
    // rollup already ran) are never materialized. Query the source low-watermark and the
    // earliest materialized `rollup:*` bucket (both cheap indexed `MIN(ts)` lookups — no new
    // migration/column, Decision D2); when the source precedes materialization, walk the gap
    // via the same memory-bounded week-aligned pass the backfill uses.
    let source_min: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT MIN(ts) FROM coin_candles \
         WHERE coin_id = $1 AND vs_currency = $2 AND interval = $3",
    )
    .bind(coin_id)
    .bind(vs_currency)
    .bind(source_interval)
    .fetch_one(pool)
    .await?;

    let earliest_materialized: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT MIN(ts) FROM coin_candles \
         WHERE coin_id = $1 AND vs_currency = $2 AND interval = $3 AND source LIKE 'rollup:%'",
    )
    .bind(coin_id)
    .bind(vs_currency)
    .bind(target_interval)
    .fetch_one(pool)
    .await?;

    if let (Some(source_min), Some(earliest_materialized)) = (source_min, earliest_materialized) {
        if let Some((repair_start, repair_end)) =
            backward_repair_window(source_min, earliest_materialized)
        {
            // Accept the idempotent overshoot (plan §3 option a): reuse the bounded walk with
            // `ceiling = earliest_materialized`. The final chunk may re-fold buckets at/after
            // the exclusive end, but those are already `rollup:*` rows so re-materializing them
            // is idempotent, and native-wins protects any native row regardless.
            materialize_window_walk(
                pool,
                coin_id,
                vs_currency,
                target_interval,
                target_secs,
                source_interval,
                source_secs,
                now,
                repair_start,
                repair_end,
            )
            .await?;
        }
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
    let coverage_rows = crate::db::interval_coverage(pool, coin_id, vs_currency).await?;
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

    // ── F-07 characterization (SPEC-CANDLE-002, pure): reconcile over a rollup-only slice ──
    // REQ-CANDLE-050/053. The source filter is applied at the SQL SELECT that feeds
    // `reconcile_window`, so the pure core is unchanged and DB-free. Confirm it still deletes a
    // dropped rollup bucket and keeps parity within the rollup-owned subset when its input
    // slice contains only `rollup:*` rows (a native row would never reach it post-filter).
    #[test]
    fn reconcile_window_over_rollup_only_slice_deletes_dropped_rollup_bucket() {
        let kept = CoinCandle {
            source: "rollup:5m".into(),
            ..make_5m(
                ts_epoch(0),
                dec!(1),
                dec!(2),
                dec!(1),
                dec!(2),
                Some(dec!(1)),
            )
        };
        let dropped = CoinCandle {
            source: "rollup:5m".into(),
            ..make_5m(
                ts_epoch(86_400),
                dec!(1),
                dec!(2),
                dec!(1),
                dec!(2),
                Some(dec!(1)),
            )
        };
        let previously_materialized = vec![kept.clone(), dropped];
        // `dropped` closed incomplete → no longer emitted; `kept` still emitted.
        let emitted = vec![kept];

        let (upserts, deletes) = reconcile_window(&previously_materialized, &emitted);
        assert_eq!(
            upserts.len(),
            1,
            "emitted set unchanged by the rollup-only narrowing"
        );
        assert_eq!(
            deletes,
            vec![ts_epoch(86_400)],
            "REQ-CANDLE-053: dropped rollup bucket still deleted — parity within the rollup subset"
        );
    }

    // ── F-08 backward-repair pure decision core (SPEC-CANDLE-002, pure) ─────────────────────
    // AC-CANDLE-055 pure portion. Returns Some (week-aligned start) when the source
    // low-watermark precedes the earliest materialized bucket; None otherwise.
    #[test]
    fn backward_repair_window_some_when_source_precedes_earliest() {
        let source_min = ts_epoch(10 * 86_400);
        let earliest_materialized = ts_epoch(20 * 86_400);

        let (start, end) = backward_repair_window(source_min, earliest_materialized)
            .expect("source precedes earliest materialized → Some");

        assert_eq!(
            start,
            bucket_start(source_min, WEEK_SECS),
            "start must be week-aligned to source_min's bucket"
        );
        assert!(
            start <= source_min,
            "week-aligned start is never after source_min"
        );
        assert_eq!(
            end, earliest_materialized,
            "end is the earliest materialized bucket (exclusive walk ceiling)"
        );
        assert!(start < end);
    }

    #[test]
    fn backward_repair_window_none_when_watermark_not_before_earliest() {
        // earliest materialized already at the source low-watermark's week bucket (epoch 0
        // Thursday): week-aligned(source_min) == 0 is NOT < 0 → None (self-terminating).
        let earliest_materialized = ts_epoch(0);
        let source_min_same_week = ts_epoch(3 * 86_400); // < WEEK_SECS → same bucket as epoch 0
        assert!(
            backward_repair_window(source_min_same_week, earliest_materialized).is_none(),
            "no source before the earliest materialized bucket → None"
        );

        // Source strictly AHEAD of materialization (materialized history is older) → None.
        let earliest2 = ts_epoch(5 * 86_400);
        let source2 = ts_epoch(40 * 86_400);
        assert!(backward_repair_window(source2, earliest2).is_none());
    }

    // ── DB-gated integration tests (SPEC-CANDLE-002) ───────────────────────────────────────
    // These MUST run with `--test-threads=1`: like the rest of this repo's DB-gated suite they
    // share the live `coin_candles` table (seeded with throwaway coin_ids + explicit cleanup),
    // and concurrent runs would interleave rows. Run:
    //   DATABASE_URL=postgres://... cargo test -p crypto-collector -- --ignored --test-threads=1
    // (per CLAUDE.md § Integration Tests).

    const DAY: i64 = 86_400;

    async fn db_pool() -> PgPool {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required for DB-gated tests");
        crate::db::connect(&url).await.expect("connect + migrate")
    }

    async fn cleanup_coin(pool: &PgPool, coin: &str) {
        sqlx::query("DELETE FROM coin_candles WHERE coin_id = $1")
            .bind(coin)
            .execute(pool)
            .await
            .expect("cleanup coin_candles");
    }

    /// Seed a complete day of uniform 5m source candles `[day_start, day_start+DAY)` at `price`
    /// (288 rows) so the day's `1d` bucket is complete and emitted.
    async fn seed_full_5m_day(
        pool: &PgPool,
        coin: &str,
        day_start: i64,
        price: rust_decimal::Decimal,
    ) {
        sqlx::query(
            "INSERT INTO coin_candles \
                (coin_id, vs_currency, interval, ts, open, high, low, close, volume, source) \
             SELECT $1, 'usd', '5m', to_timestamp(gs), $2, $2, $2, $2, 1, 'binance' \
             FROM generate_series($3::bigint, $3::bigint + 86400 - 300, 300) AS gs \
             ON CONFLICT (coin_id, vs_currency, interval, ts) DO NOTHING",
        )
        .bind(coin)
        .bind(price)
        .bind(day_start)
        .execute(pool)
        .await
        .expect("seed full 5m day");
    }

    /// Insert (or reset) a single `1d` row at `ts` with the given `source` and uniform OHLC.
    async fn insert_1d(
        pool: &PgPool,
        coin: &str,
        ts: i64,
        price: rust_decimal::Decimal,
        source: &str,
    ) {
        sqlx::query(
            "INSERT INTO coin_candles \
                (coin_id, vs_currency, interval, ts, open, high, low, close, volume, source) \
             VALUES ($1, 'usd', '1d', to_timestamp($2), $3, $3, $3, $3, 42, $4) \
             ON CONFLICT (coin_id, vs_currency, interval, ts) \
             DO UPDATE SET source = EXCLUDED.source, close = EXCLUDED.close, \
                           open = EXCLUDED.open, high = EXCLUDED.high, low = EXCLUDED.low",
        )
        .bind(coin)
        .bind(ts)
        .bind(price)
        .bind(source)
        .execute(pool)
        .await
        .expect("insert 1d");
    }

    async fn fetch_1d(
        pool: &PgPool,
        coin: &str,
        ts: i64,
    ) -> Option<(String, rust_decimal::Decimal)> {
        sqlx::query_as::<_, (String, rust_decimal::Decimal)>(
            "SELECT source, close FROM coin_candles \
             WHERE coin_id = $1 AND vs_currency = 'usd' AND interval = '1d' AND ts = to_timestamp($2)",
        )
        .bind(coin)
        .bind(ts)
        .fetch_optional(pool)
        .await
        .expect("fetch 1d")
    }

    async fn count_1d(pool: &PgPool, coin: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM coin_candles \
             WHERE coin_id = $1 AND vs_currency = 'usd' AND interval = '1d'",
        )
        .bind(coin)
        .fetch_one(pool)
        .await
        .expect("count 1d")
    }

    async fn min_rollup_1d_ts(pool: &PgPool, coin: &str) -> DateTime<Utc> {
        sqlx::query_scalar::<_, DateTime<Utc>>(
            "SELECT MIN(ts) FROM coin_candles \
             WHERE coin_id = $1 AND vs_currency = 'usd' AND interval = '1d' AND source LIKE 'rollup:%'",
        )
        .bind(coin)
        .fetch_one(pool)
        .await
        .expect("min rollup 1d ts")
    }

    fn far_future() -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_000_000_000, 0).unwrap()
    }

    // AC-CANDLE-050 (REQ-CANDLE-050/051/053): a recompute window holding BOTH a native `1d` row
    // and rollup rows leaves the native row byte-identical while the rollup subset reconciles.
    #[tokio::test]
    #[ignore]
    async fn db_mixed_source_preserves_native_and_reconciles_rollup() {
        let pool = db_pool().await;
        let coin = "test-candle002-mixed";
        cleanup_coin(&pool, coin).await;

        let day0 = 0i64; // epoch Thursday → week-aligned, so no spurious backward repair fires
        let day1 = DAY;

        seed_full_5m_day(&pool, coin, day0, dec!(100)).await;
        seed_full_5m_day(&pool, coin, day1, dec!(200)).await;
        // Pre-existing rollup 1d row at day0 forces the incremental/reconcile path (max_bucket
        // Some) rather than the first-run backfill path. Its close (999) must be reconciled away.
        insert_1d(&pool, coin, day0, dec!(999), "rollup:5m").await;
        // Native provider 1d row at day1 — an emitted rollup bucket targets this exact ts.
        insert_1d(&pool, coin, day1, dec!(12345), "bitstamp").await;

        incremental_recompute_target(&pool, coin, "usd", "1d", DAY, "5m", 300, far_future())
            .await
            .expect("reconcile cycle");

        let native = fetch_1d(&pool, coin, day1)
            .await
            .expect("native day1 row present");
        assert_eq!(
            native.0, "bitstamp",
            "native source preserved (not relabeled to rollup)"
        );
        assert_eq!(
            native.1,
            dec!(12345),
            "native close preserved (never overwritten)"
        );

        let rollup = fetch_1d(&pool, coin, day0)
            .await
            .expect("rollup day0 row present");
        assert_eq!(rollup.0, "rollup:5m");
        assert_eq!(
            rollup.1,
            dec!(100),
            "rollup day0 reconciled to the folded close"
        );

        assert_eq!(
            count_1d(&pool, coin).await,
            2,
            "exactly two 1d rows — native day1 neither deleted nor duplicated"
        );

        cleanup_coin(&pool, coin).await;
    }

    // AC-CANDLE-052 (REQ-CANDLE-052): an emitted rollup bucket colliding with a native row at the
    // same PK is a no-op — native wins.
    #[tokio::test]
    #[ignore]
    async fn db_collision_native_wins() {
        let pool = db_pool().await;
        let coin = "test-candle002-collision";
        cleanup_coin(&pool, coin).await;

        let day0 = 0i64;
        seed_full_5m_day(&pool, coin, day0, dec!(100)).await; // emits a 1d bucket at day0
        insert_1d(&pool, coin, day0, dec!(54321), "coingecko").await; // native at that exact ts

        incremental_recompute_target(&pool, coin, "usd", "1d", DAY, "5m", 300, far_future())
            .await
            .expect("materialize");

        let row = fetch_1d(&pool, coin, day0).await.expect("day0 row present");
        assert_eq!(row.0, "coingecko", "native source preserved on collision");
        assert_eq!(
            row.1,
            dec!(54321),
            "native OHLCV not overwritten by the rollup value"
        );
        assert_eq!(
            count_1d(&pool, coin).await,
            1,
            "no duplicate rollup row created"
        );

        cleanup_coin(&pool, coin).await;
    }

    // AC-CANDLE-055 (REQ-CANDLE-054/055/056/057): source arriving behind the earliest materialized
    // bucket triggers a bounded backward pass that extends the series; a re-run is idempotent.
    #[tokio::test]
    #[ignore]
    async fn db_history_repair_backward_pass_is_idempotent() {
        let pool = db_pool().await;
        let coin = "test-candle002-repair";
        cleanup_coin(&pool, coin).await;

        // Initial source: two adjacent complete days deep in history.
        let late_a = 20 * DAY;
        let late_b = 21 * DAY;
        seed_full_5m_day(&pool, coin, late_a, dec!(300)).await;
        seed_full_5m_day(&pool, coin, late_b, dec!(310)).await;

        // First run: no rollup rows → backfill materializes late_a/late_b (no backward repair yet).
        incremental_recompute_target(&pool, coin, "usd", "1d", DAY, "5m", 300, far_future())
            .await
            .expect("initial materialize");
        assert!(
            fetch_1d(&pool, coin, late_a).await.is_some(),
            "late_a materialized"
        );
        let earliest_before = min_rollup_1d_ts(&pool, coin).await;

        // Deep backfill completes AFTER the rollup: insert source BEHIND the earliest bucket.
        let early_a = 4 * DAY;
        let early_b = 5 * DAY;
        seed_full_5m_day(&pool, coin, early_a, dec!(50)).await;
        seed_full_5m_day(&pool, coin, early_b, dec!(60)).await;

        // Second run: forward recompute (no change) + backward repair materializes early history.
        incremental_recompute_target(&pool, coin, "usd", "1d", DAY, "5m", 300, far_future())
            .await
            .expect("history-repair run");
        assert!(
            fetch_1d(&pool, coin, early_a).await.is_some(),
            "early_a materialized by the bounded backward pass"
        );
        let earliest_after = min_rollup_1d_ts(&pool, coin).await;
        assert!(
            earliest_after < earliest_before,
            "earliest materialized bucket moved back to cover the backfilled history"
        );

        // Third run: idempotent / self-terminating — no further change to the row set.
        let count_before_rerun = count_1d(&pool, coin).await;
        incremental_recompute_target(&pool, coin, "usd", "1d", DAY, "5m", 300, far_future())
            .await
            .expect("idempotent re-run");
        assert_eq!(
            count_1d(&pool, coin).await,
            count_before_rerun,
            "re-running the rollup produced no further change (idempotent)"
        );

        cleanup_coin(&pool, coin).await;
    }
}
