//! Bitcoin halving-cycle overlay: pure cycle math + recompute driver (SPEC-CYCLE-001).
//!
//! This module is split into two halves:
//! - **Pure transform** (`assign_cycle`, `compute_overlay`): no SQL, no async, fully
//!   unit-testable with in-memory `Decimal` fixtures.
//! - **Recompute driver** (`recompute_cycle_overlay`): reads `coin_candles`, invokes the
//!   pure transform, and replaces the `cycle_overlay_points` table as an idempotent rebuild
//!   (REQ-CYCLE-041/042).

use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::collections::BTreeMap;

// ── Halving-date constants (D6) ────────────────────────────────────────────────

/// Compiled-in halving dates (block-derived, approximate; D6, REQ-CYCLE-010).
///
/// Cycles are numbered by their starting halving: cycle 1 = 2012-11-28 … cycle 4 = 2024-04-20.
/// Cycle 4 is open-ended (no known next halving) — REQ-CYCLE-012.
fn halving_dates() -> [NaiveDate; 4] {
    [
        NaiveDate::from_ymd_opt(2012, 11, 28).expect("valid compiled-in halving date"),
        NaiveDate::from_ymd_opt(2016, 7, 9).expect("valid compiled-in halving date"),
        NaiveDate::from_ymd_opt(2020, 5, 11).expect("valid compiled-in halving date"),
        NaiveDate::from_ymd_opt(2024, 4, 20).expect("valid compiled-in halving date"),
    ]
}

/// The cycle a given date belongs to, plus its day-0 halving date and whole-day offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CycleAssignment {
    pub cycle_number: i32,
    pub halving_date: NaiveDate,
    pub days_since_halving: i64,
}

/// Assign a date to its halving cycle (half-open `[halving, next_halving)`; D6, REQ-CYCLE-010/011).
///
/// Returns `None` for dates before the first known halving (2012-11-28) — the overlay begins
/// at the first known halving and does not model the genesis-to-first-halving era.
///
// @MX:ANCHOR: [AUTO] assign_cycle — cycle-partitioning + days_since_halving; every overlay
//             point depends on this. fan_in >= 3: compute_overlay, recompute driver, unit tests.
// @MX:REASON: Half-open `[halving, next_halving)` boundaries and day-0 = halving-date are the
//             correctness core (REQ-CYCLE-010/011). Cycle 4 has no upper bound (open-ended,
//             REQ-CYCLE-012) — do not add a synthetic "next halving" for it.
// @MX:SPEC: SPEC-CYCLE-001 REQ-CYCLE-010 REQ-CYCLE-011 REQ-CYCLE-012
pub fn assign_cycle(date: NaiveDate) -> Option<CycleAssignment> {
    let dates = halving_dates();
    if date < dates[0] {
        return None;
    }
    // Last index whose halving_date <= date (half-open window membership).
    let mut idx = 0usize;
    for (i, &d) in dates.iter().enumerate() {
        if date >= d {
            idx = i;
        } else {
            break;
        }
    }
    let halving_date = dates[idx];
    let days_since_halving = (date - halving_date).num_days();
    Some(CycleAssignment {
        cycle_number: (idx + 1) as i32,
        halving_date,
        days_since_halving,
    })
}

// ── Pure overlay transform (dual normalization, D7/D8/D9) ────────────────────

/// One computed overlay point, prior to being stamped with `coin_id`/`vs_currency`.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayPoint {
    pub cycle_number: i32,
    pub halving_date: NaiveDate,
    pub days_since_halving: i64,
    pub ts: NaiveDate,
    pub price: Decimal,
    pub norm_halving: Decimal,
    pub norm_cycle_low: Decimal,
    pub halving_baseline_approximate: bool,
    /// `true` when this point is a forward projection (REQ-CYCLE-060), not a real candle.
    /// Always `false` for points produced by `compute_overlay`.
    pub projected: bool,
    /// P10 band (projected points only; REQ-CYCLE-064). `None` on real points.
    pub price_low: Option<Decimal>,
    /// P90 band (projected points only; REQ-CYCLE-064). `None` on real points.
    pub price_high: Option<Decimal>,
}

/// Compute the cycle overlay from a coin's daily `(date, close)` series (D7/D8/D9).
///
/// Both `norm_halving` and `norm_cycle_low` are derived from the SAME `close` series
/// passed in — this single-series rule (D7) is what makes the anchor day and the
/// cycle-low day each normalise to exactly `1.0`. No interpolation is performed: a date
/// with no entry in `daily` produces no point (D9, REQ-CYCLE-033). Dates before the first
/// halving are silently omitted (REQ-CYCLE-030/031); cycles with zero input dates produce
/// zero points and this is not an error.
///
/// The in-progress (most recent) cycle's `cycle_low_price` is the minimum over its
/// currently available days — because this function is re-run on every recompute against
/// the current `coin_candles` contents, this naturally reproduces the "running minimum,
/// provisional until closed" behaviour of REQ-CYCLE-034 without any special-casing: a
/// later recompute with more data simply recomputes a (possibly lower) minimum and shifts
/// previously emitted points of that cycle. This is expected, not a regression.
///
// @MX:WARN: [AUTO] compute_overlay — single-series normalization fold (D7/D2).
// @MX:REASON: norm_halving and norm_cycle_low MUST both divide by prices drawn from the
//             same `daily` input series as the numerator. Substituting a different series
//             for either denominator (e.g. a daily `low` instead of `close`) silently
//             breaks the "anchor day = 1.0" / "cycle-low day = 1.0" invariants
//             (REQ-CYCLE-002/020/021/024). Always Decimal, never f64 (REQ-PROV-012).
// @MX:SPEC: SPEC-CYCLE-001 REQ-CYCLE-002 REQ-CYCLE-020 REQ-CYCLE-021 REQ-CYCLE-023
//           REQ-CYCLE-024 REQ-CYCLE-030 REQ-CYCLE-031 REQ-CYCLE-032 REQ-CYCLE-033 REQ-CYCLE-034
pub fn compute_overlay(mut daily: Vec<(NaiveDate, Decimal)>) -> Vec<OverlayPoint> {
    /// `(date, days_since_halving, price)` per available day within a cycle.
    type CycleDayPoints = Vec<(NaiveDate, i64, Decimal)>;

    daily.sort_by_key(|(d, _)| *d);

    // Group into cycles, preserving each cycle's halving_date.
    let mut cycles: BTreeMap<i32, (NaiveDate, CycleDayPoints)> = BTreeMap::new();
    for (date, price) in daily {
        if let Some(a) = assign_cycle(date) {
            let entry = cycles
                .entry(a.cycle_number)
                .or_insert_with(|| (a.halving_date, Vec::new()));
            entry.1.push((date, a.days_since_halving, price));
        }
    }

    let mut result = Vec::new();
    for (cycle_number, (halving_date, mut points)) in cycles {
        points.sort_by_key(|(_, dsh, _)| *dsh);

        // D8: the halving-day anchor is the first available day (smallest days_since_halving).
        // When that first day is not day 0, the true halving-date candle was absent and the
        // baseline is marked approximate (REQ-CYCLE-032). days_since_halving is still measured
        // from the true halving date regardless (assign_cycle already did this).
        let (_, first_dsh, anchor_price) = points[0];
        let approximate = first_dsh != 0;

        // D2/D7: cycle_low_price is the minimum close over the cycle's currently available
        // days. Decimal implements Ord, so `.min()` is exact (no f64 comparison).
        let cycle_low = points
            .iter()
            .map(|&(_, _, p)| p)
            .min()
            .expect("cycle group is always non-empty by construction");

        for (date, dsh, price) in points {
            result.push(OverlayPoint {
                cycle_number,
                halving_date,
                days_since_halving: dsh,
                ts: date,
                price,
                norm_halving: price / anchor_price,
                norm_cycle_low: price / cycle_low,
                halving_baseline_approximate: approximate,
                projected: false,
                price_low: None,
                price_high: None,
            });
        }
    }

    result
}

// ── Forward projection support (REQ-CYCLE-060..064) ───────────────────────────
//
// The projection model itself lives in `super::cycle_projection` (composite model,
// SPEC-CYCLE-001 v0.4.0). This module keeps the cycle-assignment machinery it shares
// with the real-data overlay.

/// One halving cycle, in days: the forward horizon of the projection (unchanged from
/// the superseded cycle-repeat replay, for API stability) and the minimum stored span
/// required before any projection is emitted (REQ-CYCLE-062).
pub const CYCLE_DAYS: i64 = 1458;

/// Extended halving-date list used ONLY to assign `cycle_number`/`days_since_halving` to
/// PROJECTED points (REQ-CYCLE-063). This is the compiled-in real `halving_dates()` plus one
/// ESTIMATED next halving. It is deliberately kept separate from `halving_dates()` /
/// `assign_cycle` — those remain untouched and cycle 4 stays open-ended for REAL data
/// (REQ-CYCLE-012); this list never bounds real points, only projected ones.
pub(crate) fn projected_halving_dates() -> Vec<NaiveDate> {
    let mut dates: Vec<NaiveDate> = halving_dates().to_vec();
    // ESTIMATE ONLY (block-height projection from the 2024-04-20 halving at ~10min/block); not
    // a confirmed halving date. Used solely to place projected points on the correct side of
    // the eventual real boundary — never affects real-data cycle assignment.
    dates.push(NaiveDate::from_ymd_opt(2028, 4, 20).expect("valid estimated halving date"));
    dates
}

/// Same half-open `[halving, next_halving)` assignment rule as `assign_cycle`, but against an
/// arbitrary (ascending) halving-date list and never returning `None` — projected timestamps
/// are always on/after the first halving by construction.
pub(crate) fn assign_cycle_in(date: NaiveDate, dates: &[NaiveDate]) -> CycleAssignment {
    let mut idx = 0usize;
    for (i, &d) in dates.iter().enumerate() {
        if date >= d {
            idx = i;
        } else {
            break;
        }
    }
    let halving_date = dates[idx];
    CycleAssignment {
        cycle_number: (idx + 1) as i32,
        halving_date,
        days_since_halving: (date - halving_date).num_days(),
    }
}

// ── Recompute driver (REQ-CYCLE-041/042/043) ──────────────────────────────────

/// Recompute the overlay for `(coin_id, vs_currency)` from `coin_candles` and replace the
/// stored `cycle_overlay_points` rows (idempotent full rebuild, REQ-CYCLE-041/042).
///
/// Reads native `1d` candles first; when none exist, falls back to deriving a `1d` series
/// by reusing the SPEC-API-003 read-time aggregation over the largest stored divisor
/// interval (OR-CYCLE-4). A coin with no `1d`-derivable history simply yields zero points —
/// this is not an error (REQ-CYCLE-030/031).
///
// @MX:NOTE: [AUTO] recompute_cycle_overlay — full idempotent derived rebuild from coin_candles.
//           In-progress-cycle points are provisional and MAY change between ticks (REQ-CYCLE-034).
//           Safe under multiple replicas: callers must serialise via the collection_queue
//           lease / SKIP LOCKED discipline (REQ-CYCLE-042); this function itself performs one
//           DELETE + re-INSERT transaction per call.
// @MX:SPEC: SPEC-CYCLE-001 REQ-CYCLE-041 REQ-CYCLE-042 REQ-CYCLE-043
pub async fn recompute_cycle_overlay(
    pool: &PgPool,
    coin_id: &str,
    vs_currency: &str,
) -> Result<()> {
    let native_rows: Vec<(NaiveDate, Decimal)> = sqlx::query_as(
        "SELECT ts::date, close FROM coin_candles \
         WHERE coin_id = $1 AND vs_currency = $2 AND interval = '1d'",
    )
    .bind(coin_id)
    .bind(vs_currency)
    .fetch_all(pool)
    .await?;

    let daily = if native_rows.is_empty() {
        aggregate_daily_from_finer(pool, coin_id, vs_currency).await?
    } else {
        native_rows
    };

    let mut points = compute_overlay(daily.clone());
    // Composite projection (SPEC-CYCLE-001 v0.4.0). The compiled-in calibration anchors
    // are BTC/USD historical closes — enable them only for that exact pair.
    let use_btc_anchors = coin_id == "bitcoin" && vs_currency == "usd";
    let projected = super::cycle_projection::project_composite(&daily, &points, use_btc_anchors);
    points.extend(projected);

    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM cycle_overlay_points WHERE coin_id = $1 AND vs_currency = $2")
        .bind(coin_id)
        .bind(vs_currency)
        .execute(&mut *tx)
        .await?;

    for p in &points {
        sqlx::query(
            "INSERT INTO cycle_overlay_points \
                (coin_id, vs_currency, cycle_number, halving_date, days_since_halving, \
                 ts, price, norm_halving, norm_cycle_low, halving_baseline_approximate, \
                 projected, price_low, price_high) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(coin_id)
        .bind(vs_currency)
        .bind(p.cycle_number)
        .bind(p.halving_date)
        .bind(p.days_since_halving as i32)
        .bind(p.ts)
        .bind(p.price)
        .bind(p.norm_halving)
        .bind(p.norm_cycle_low)
        .bind(p.halving_baseline_approximate)
        .bind(p.projected)
        .bind(p.price_low)
        .bind(p.price_high)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

/// Select the 1d-divisor interval with the widest historical coverage as the daily source.
///
/// Unlike SPEC-API-003's `select_source_interval` (which picks the *coarsest* divisor to
/// minimise aggregation work), the overlay needs the *deepest* history: a coin may store a
/// coarse interval (e.g. `4h`) that only covers recent live data alongside a finer interval
/// (e.g. `5m`) backfilled across years. Picking the coarsest would silently truncate the
/// overlay to the recent window. We therefore rank candidates by coverage span (widest
/// first), breaking ties toward the coarser interval for aggregation efficiency.
///
/// `candidates` are `(interval, coverage_secs)` pairs where `coverage_secs = max(ts) - min(ts)`.
///
// @MX:NOTE: [AUTO] overlay daily source = widest-coverage 1d-divisor, NOT the coarsest.
// @MX:REASON: heterogeneous per-interval coverage (finer=backfilled deep history, coarser=recent
//             live only) means "coarsest divisor" can drop years of data. Widest span reaches
//             back furthest; coarser breaks ties only among equally-covering intervals.
// @MX:SPEC: SPEC-CYCLE-001 REQ-CYCLE-041 OR-CYCLE-4
fn select_widest_source_interval<'a>(
    candidates: &[(&'a str, i64)],
    target_secs: i64,
) -> Option<&'a str> {
    use crate::api::candles_agg::interval_to_seconds;
    candidates
        .iter()
        .filter_map(|&(name, coverage_secs)| {
            let secs = interval_to_seconds(name)?;
            // Must be a strictly-finer interval that tiles 1d evenly (same divisor rule).
            if secs < target_secs && target_secs % secs == 0 {
                Some((coverage_secs, secs, name))
            } else {
                None
            }
        })
        // Widest coverage first; tie-break toward the coarser interval (larger secs).
        .max_by_key(|&(coverage_secs, secs, _)| (coverage_secs, secs))
        .map(|(_, _, name)| name)
}

/// Derive a `1d` `(date, close)` series from the widest-coverage stored divisor interval,
/// reusing the SPEC-API-003 read-time aggregation (OR-CYCLE-4).
async fn aggregate_daily_from_finer(
    pool: &PgPool,
    coin_id: &str,
    vs_currency: &str,
) -> Result<Vec<(NaiveDate, Decimal)>> {
    use crate::api::candles_agg::interval_to_seconds;

    // Per-interval coverage span, so we can prefer the interval that reaches furthest back
    // rather than the coarsest one (which may only hold recent live data).
    let stored: Vec<(String, DateTime<Utc>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT interval, min(ts), max(ts) FROM coin_candles \
         WHERE coin_id = $1 AND vs_currency = $2 GROUP BY interval",
    )
    .bind(coin_id)
    .bind(vs_currency)
    .fetch_all(pool)
    .await?;

    let target_secs = interval_to_seconds("1d").expect("1d always has a known second count");
    let candidates: Vec<(&str, i64)> = stored
        .iter()
        .map(|(interval, min_ts, max_ts)| (interval.as_str(), (*max_ts - *min_ts).num_seconds()))
        .collect();

    let Some(source_interval) = select_widest_source_interval(&candidates, target_secs) else {
        // OR-CYCLE-4 / REQ-CYCLE-030/031: no derivable 1d source → zero points, not an error.
        return Ok(vec![]);
    };
    let source_interval = source_interval.to_string();

    // Aggregate to a daily `(date, close)` series IN SQL. A multi-year finer series (e.g. 5m
    // over 9 years ≈ 1M rows) must never be materialised in the app — doing so OOM-kills the
    // 256Mi pod. `DISTINCT ON (day) ... ORDER BY day, ts DESC` returns one row per UTC day
    // whose `close` is the last intraday candle's close — the same "last value in bucket"
    // daily-close semantics as SPEC-API-003 aggregation, at ~one row per day instead of ~1M.
    //
    // @MX:WARN: [AUTO] SQL-side daily aggregation — do NOT revert to fetch_all of the finer
    //           series; a deep backfill makes that OOM the pod (256Mi limit).
    // @MX:REASON: aggregate_daily_from_finer previously loaded every source candle; with the
    //             widest-coverage interval that is the full multi-year 5m history.
    // @MX:SPEC: SPEC-CYCLE-001 REQ-CYCLE-041 OR-CYCLE-4
    let daily: Vec<(NaiveDate, Decimal)> = sqlx::query_as(
        "SELECT DISTINCT ON ((ts AT TIME ZONE 'UTC')::date) \
                (ts AT TIME ZONE 'UTC')::date, close \
         FROM coin_candles \
         WHERE coin_id = $1 AND vs_currency = $2 AND interval = $3 \
         ORDER BY (ts AT TIME ZONE 'UTC')::date, ts DESC",
    )
    .bind(coin_id)
    .bind(vs_currency)
    .bind(&source_interval)
    .fetch_all(pool)
    .await?;

    Ok(daily)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    // ── assign_cycle ───────────────────────────────────────────────────────────

    // Scenario 2 (REQ-CYCLE-010/011): cycle partitioning + day-0 = halving date.
    #[test]
    fn assign_cycle_halving_date_is_day_zero_of_its_own_cycle() {
        let a = assign_cycle(d(2020, 5, 11)).unwrap();
        assert_eq!(a.cycle_number, 3);
        assert_eq!(a.halving_date, d(2020, 5, 11));
        assert_eq!(a.days_since_halving, 0);
    }

    #[test]
    fn assign_cycle_last_day_before_next_halving() {
        let a = assign_cycle(d(2024, 4, 19)).unwrap();
        assert_eq!(a.cycle_number, 3);
        assert_eq!(a.days_since_halving, 1439);
    }

    #[test]
    fn assign_cycle_half_open_boundary_next_halving_belongs_to_next_cycle() {
        let a = assign_cycle(d(2024, 4, 20)).unwrap();
        assert_eq!(
            a.cycle_number, 4,
            "the next halving date belongs to the new cycle"
        );
        assert_eq!(a.days_since_halving, 0);
    }

    #[test]
    fn assign_cycle_cycle_1_and_2_boundaries() {
        assert_eq!(assign_cycle(d(2012, 11, 28)).unwrap().cycle_number, 1);
        assert_eq!(assign_cycle(d(2016, 7, 8)).unwrap().cycle_number, 1);
        assert_eq!(assign_cycle(d(2016, 7, 9)).unwrap().cycle_number, 2);
        assert_eq!(assign_cycle(d(2020, 5, 10)).unwrap().cycle_number, 2);
    }

    #[test]
    fn assign_cycle_open_ended_cycle_4_has_no_upper_bound() {
        // REQ-CYCLE-012: cycle 4 extends indefinitely (no known next halving yet).
        let far_future = assign_cycle(d(2030, 1, 1)).unwrap();
        assert_eq!(far_future.cycle_number, 4);
    }

    #[test]
    fn assign_cycle_before_first_halving_is_none() {
        assert!(assign_cycle(d(2012, 11, 27)).is_none());
        assert!(assign_cycle(d(2009, 1, 3)).is_none());
    }

    // ── compute_overlay: dual normalization exact-1.0 invariants ──────────────

    // Scenario 3 (REQ-CYCLE-002/020/022): halving-day anchor normalises to exactly 1.0.
    #[test]
    fn halving_anchor_normalizes_to_exactly_one() {
        let daily = vec![(d(2020, 5, 11), dec!(8600)), (d(2021, 1, 1), dec!(17200))];
        let points = compute_overlay(daily);
        let anchor = points.iter().find(|p| p.days_since_halving == 0).unwrap();
        assert_eq!(anchor.norm_halving, dec!(1));
        let later = points.iter().find(|p| p.ts == d(2021, 1, 1)).unwrap();
        assert_eq!(later.norm_halving, dec!(2));
        // Both baselines are always present (REQ-CYCLE-022).
        assert!(points.iter().all(|p| p.norm_cycle_low > dec!(0)));
    }

    // Scenario 4 (REQ-CYCLE-021): cycle-low day normalises to exactly 1.0.
    #[test]
    fn cycle_low_normalizes_to_exactly_one() {
        let daily = vec![
            (d(2020, 5, 11), dec!(8600)),
            (d(2020, 11, 27), dec!(4000)), // cycle low
            (d(2021, 6, 1), dec!(12000)),
        ];
        let points = compute_overlay(daily);
        let low_point = points.iter().find(|p| p.ts == d(2020, 11, 27)).unwrap();
        assert_eq!(low_point.norm_cycle_low, dec!(1));
        let high_point = points.iter().find(|p| p.ts == d(2021, 6, 1)).unwrap();
        assert_eq!(high_point.norm_cycle_low, dec!(3));
    }

    // Scenario 5 (REQ-CYCLE-023): cycle-low series plotted against days_since_halving,
    // NOT re-based to days-since-low.
    #[test]
    fn cycle_low_x_axis_is_days_since_halving_not_days_since_low() {
        let halving = d(2020, 5, 11);
        let low_day = halving + chrono::Duration::days(200);
        let daily = vec![(halving, dec!(9000)), (low_day, dec!(4000))];
        let points = compute_overlay(daily);
        let low_point = points.iter().find(|p| p.norm_cycle_low == dec!(1)).unwrap();
        assert_eq!(
            low_point.days_since_halving, 200,
            "cycle-low point's X-axis must remain days_since_halving, not reset to 0"
        );
    }

    // Scenario 9 (REQ-CYCLE-032): missing halving-day anchor forward-searches.
    #[test]
    fn missing_halving_day_anchor_forward_searches_first_available() {
        let daily = vec![
            (d(2020, 5, 23), dec!(9700)), // first available: days_since_halving == 12
            (d(2021, 1, 1), dec!(19400)),
        ];
        let points = compute_overlay(daily);
        let anchor = points.iter().find(|p| p.ts == d(2020, 5, 23)).unwrap();
        assert_eq!(anchor.norm_halving, dec!(1));
        assert_eq!(anchor.days_since_halving, 12);
        assert!(
            anchor.halving_baseline_approximate,
            "REQ-CYCLE-032: forward-searched anchor must be flagged approximate"
        );
    }

    #[test]
    fn exact_halving_day_present_is_not_approximate() {
        let daily = vec![(d(2020, 5, 11), dec!(8600))];
        let points = compute_overlay(daily);
        assert!(!points[0].halving_baseline_approximate);
    }

    // Scenario 10 (REQ-CYCLE-033): no interpolation — sparse sequence under gaps.
    #[test]
    fn gaps_produce_no_interpolated_points() {
        let halving = d(2020, 5, 11);
        let daily = vec![
            (halving + chrono::Duration::days(99), dec!(10000)),
            (halving + chrono::Duration::days(105), dec!(10500)),
        ];
        let points = compute_overlay(daily);
        let dshs: Vec<i64> = points.iter().map(|p| p.days_since_halving).collect();
        assert_eq!(dshs, vec![99, 105], "no point for days 100..104 (gap)");
    }

    // Scenario 7 (REQ-CYCLE-030/031): absent early cycles produce zero points, no error.
    #[test]
    fn absent_early_history_omits_those_cycles_without_error() {
        // Only 2020+ data — cycles 1 and 2 have zero input dates.
        let daily = vec![(d(2020, 5, 11), dec!(8600)), (d(2024, 4, 20), dec!(65000))];
        let points = compute_overlay(daily);
        assert!(points
            .iter()
            .all(|p| p.cycle_number == 3 || p.cycle_number == 4));
        assert!(points.iter().any(|p| p.cycle_number == 3));
        assert!(points.iter().any(|p| p.cycle_number == 4));
    }

    #[test]
    fn zero_candles_yields_zero_points_without_error() {
        let points = compute_overlay(vec![]);
        assert!(points.is_empty());
    }

    // Scenario 8 (REQ-CYCLE-030): partial cycle uses available days only.
    #[test]
    fn partial_cycle_starts_at_first_available_day() {
        let daily = vec![(d(2021, 1, 1), dec!(30000))]; // ~235 days after 2020-05-11 halving
        let points = compute_overlay(daily);
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].cycle_number, 3);
        assert_eq!(points[0].days_since_halving, 235);
    }

    // Scenario 11 / REQ-CYCLE-034: running-min update for the in-progress cycle across
    // successive recomputes (simulated here as two separate compute_overlay calls over
    // growing input, since the function is pure and re-run on every recompute).
    #[test]
    fn in_progress_cycle_low_shifts_as_new_low_arrives_between_recomputes() {
        let halving = d(2024, 4, 20);
        let day1 = halving + chrono::Duration::days(10);
        let day2 = halving + chrono::Duration::days(20);

        // First recompute: only day1 available, running-min so far = 50000.
        let points_1 = compute_overlay(vec![(halving, dec!(60000)), (day1, dec!(50000))]);
        let d1_point = points_1.iter().find(|p| p.ts == day1).unwrap();
        assert_eq!(d1_point.norm_cycle_low, dec!(1));

        // Second recompute: day2 arrives with a new, lower close.
        let points_2 = compute_overlay(vec![
            (halving, dec!(60000)),
            (day1, dec!(50000)),
            (day2, dec!(40000)),
        ]);
        let d1_point_2 = points_2.iter().find(|p| p.ts == day1).unwrap();
        let d2_point_2 = points_2.iter().find(|p| p.ts == day2).unwrap();
        assert_eq!(
            d2_point_2.norm_cycle_low,
            dec!(1),
            "the new low must normalise to 1.0"
        );
        assert_eq!(
            d1_point_2.norm_cycle_low,
            dec!(50000) / dec!(40000),
            "REQ-CYCLE-034: an earlier point's norm_cycle_low may shift on recompute; not an error"
        );
    }

    // Edge case: single-day cycle is both anchor and low.
    #[test]
    fn single_day_cycle_is_both_anchor_and_low() {
        let points = compute_overlay(vec![(d(2020, 5, 11), dec!(8600))]);
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].norm_halving, dec!(1));
        assert_eq!(points[0].norm_cycle_low, dec!(1));
        assert_eq!(points[0].days_since_halving, 0);
        assert!(!points[0].halving_baseline_approximate);
    }

    // REQ-CYCLE-024: no f64 anywhere — enforced structurally (Decimal-typed fields);
    // this test just documents the invariant by checking the field types compile as Decimal.
    #[test]
    fn overlay_point_fields_are_decimal_typed() {
        let points = compute_overlay(vec![(d(2020, 5, 11), dec!(8600))]);
        let _price: Decimal = points[0].price;
        let _nh: Decimal = points[0].norm_halving;
        let _ncl: Decimal = points[0].norm_cycle_low;
    }

    // ── select_widest_source_interval: overlay daily-source selection ─────────────

    const DAY: i64 = 86_400;

    // Reproduction (OR-CYCLE-4 regression): a coarse interval (4h) holding only recent live
    // data must NOT be preferred over a fine interval (5m) that spans years of backfill.
    // The old SPEC-API-003 `select_source_interval` (coarsest divisor) would pick 4h and
    // truncate the overlay to ~1 month; the overlay-specific selector must pick 5m.
    #[test]
    fn widest_source_prefers_deep_5m_over_recent_4h() {
        let candidates = [
            ("4h", 28 * DAY),   // ~1 month recent
            ("5m", 3244 * DAY), // ~9 years backfilled
            ("1m", 2 * DAY),    // 2 days recent
        ];
        assert_eq!(select_widest_source_interval(&candidates, DAY), Some("5m"));
        // Contrast: with coverage held equal, the API selector tie-breaks to the coarser 4h.
        // (The API selector is now coverage-aware too; fed real spans it also prefers 5m — the
        // overlay keeps its own selector for its distinct coverage_secs/tie-break contract.)
        use crate::api::candles_agg::IntervalCoverage;
        let anchor = chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap();
        let equal_cov: Vec<IntervalCoverage> = candidates
            .iter()
            .map(|&(n, _)| IntervalCoverage {
                interval: n,
                earliest: anchor,
                latest: anchor,
            })
            .collect();
        assert_eq!(
            crate::api::candles_agg::select_source_interval(&equal_cov, DAY, None, anchor),
            Some("4h"),
            "with coverage held equal, the API selector tie-breaks to the coarser 4h"
        );
    }

    #[test]
    fn widest_source_breaks_ties_toward_coarser_interval() {
        // Equal coverage → prefer the coarser interval (fewer rows to aggregate).
        let candidates = [("5m", 100 * DAY), ("1h", 100 * DAY)];
        assert_eq!(select_widest_source_interval(&candidates, DAY), Some("1h"));
    }

    #[test]
    fn widest_source_ignores_non_divisor_and_too_coarse_intervals() {
        // `1d` is not strictly finer than target; a non-tiling interval is excluded.
        let candidates = [("1d", 999 * DAY), ("4h", 10 * DAY)];
        assert_eq!(select_widest_source_interval(&candidates, DAY), Some("4h"));
        assert_eq!(
            select_widest_source_interval(&[], DAY),
            None,
            "no candidates → no source"
        );
    }
}
