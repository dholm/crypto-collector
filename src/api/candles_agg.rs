//! Pure aggregation logic for coin-candle interval fallback (SPEC-API-003).
//!
//! This module is intentionally side-effect-free: no SQL, no async, no clock reads.
//! All arithmetic uses `rust_decimal::Decimal`; no `f64` anywhere (REQ-API-216, REQ-PROV-012).
//! `now` is always injected by the caller so pure logic remains hermetically testable.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use crate::models::quote::CoinCandle;

// ── Interval arithmetic ───────────────────────────────────────────────────────

/// Map an interval string to its fixed-second duration.
///
/// Covers the full stored vocabulary:
/// - Binance: `1m`, `3m`, `5m`, `15m`, `30m`, `1h`, `2h`, `4h`, `6h`, `8h`, `12h`,
///   `1d`, `3d`, `1w` (and the CoinGecko subset `30m`, `4h`, `4d`).
/// - Returns `None` for `1M` (calendar month — non-fixed duration) and any
///   unrecognised string.
///
// @MX:ANCHOR: [AUTO] interval_to_seconds — canonical interval→seconds table; every
//             divisibility check in select_source_interval depends on this mapping.
// @MX:REASON: Non-fixed-duration units (1M) MUST return None so they are never
//             selected as aggregation sources (REQ-API-204). Adding a new stored
//             interval string requires a matching entry here first (REQ-API-203/204).
// @MX:SPEC: SPEC-API-003 REQ-API-203 REQ-API-204
pub fn interval_to_seconds(interval: &str) -> Option<i64> {
    match interval {
        "1m" => Some(60),
        "3m" => Some(180),
        "5m" => Some(300),
        "15m" => Some(900),
        "30m" => Some(1_800),
        "1h" => Some(3_600),
        "2h" => Some(7_200),
        "4h" => Some(14_400),
        "6h" => Some(21_600),
        "8h" => Some(28_800),
        "12h" => Some(43_200),
        "1d" => Some(86_400),
        "3d" => Some(259_200),
        "4d" => Some(345_600),
        "1w" => Some(604_800),
        // "1M" and any unrecognised string — non-fixed duration or unknown.
        _ => None,
    }
}

/// Stored-interval coverage: the `[earliest, latest]` timestamp span actually present
/// for one interval of a `(coin_id, vs_currency)` series.
///
/// The caller supplies this (from a `MIN(ts)/MAX(ts) … GROUP BY interval` probe) so the
/// pure selector can weigh **history depth and staleness**, not just bucket divisibility.
#[derive(Debug, Clone, Copy)]
pub struct IntervalCoverage<'a> {
    pub interval: &'a str,
    pub earliest: DateTime<Utc>,
    pub latest: DateTime<Utc>,
}

/// Select the stored interval that best covers the requested window and evenly divides
/// the target.
///
/// Candidate divisors are the fixed-duration stored intervals with
/// `source_secs < target_secs` and `target_secs % source_secs == 0` (non-fixed-duration
/// strings like `1M` are excluded via `interval_to_seconds` returning `None`).
///
/// Among divisors, the one whose stored span best covers the requested window
/// `[floor, now]` is chosen. Each divisor is scored by the seconds of that window it
/// **cannot** serve — unreached history at the old end plus staleness at the fresh end:
///
/// ```text
/// floor      = window_start, or the deepest available `earliest` when the request is unbounded
/// score(c)   = max(0, c.earliest − floor)  +  max(0, now − c.latest)
/// ```
///
/// The lowest score wins; ties fall back to the **larger** divisor. The tie-break
/// preserves the original fidelity rule: for equally-covering intervals a larger divisor
/// needs fewer source candles per bucket and therefore drops fewer interior buckets on
/// gaps (REQ-API-205 / D1). Returns `None` when no stored interval qualifies.
///
/// Rationale: divisibility alone is blind to coverage. Two stored intervals can both
/// divide `1d` while spanning wildly different date ranges (e.g. a 9-year `5m` backfill
/// vs a 1-month `4h` series); picking the largest divisor silently served the shallow,
/// stale one. Coverage scoring picks the series that actually holds the requested history
/// and reaches the freshest candle, while keeping largest-divisor fidelity when spans tie.
///
// @MX:ANCHOR: [AUTO] select_source_interval — correctness core for aggregation source selection
// @MX:REASON: Divisibility is `target_secs % source_secs == 0`; non-fixed-duration
//             intervals (e.g. `1M`) are excluded via interval_to_seconds returning None.
//             Coverage score (deep-miss + stale-miss) is minimized; ties fall back to the
//             larger divisor (REQ-API-205). fan_in >= 3: list_candles handler + unit tests
//             + acceptance scenarios.
// @MX:SPEC: SPEC-API-003 REQ-API-203 REQ-API-204 REQ-API-205
pub fn select_source_interval<'a>(
    stored: &[IntervalCoverage<'a>],
    target_secs: i64,
    window_start: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<&'a str> {
    // Candidate divisors: fixed-duration intervals strictly smaller than the target that
    // divide it evenly (non-fixed-duration strings excluded via interval_to_seconds → None).
    let divisors: Vec<(i64, &IntervalCoverage)> = stored
        .iter()
        .filter_map(|c| {
            let secs = interval_to_seconds(c.interval)?;
            (secs < target_secs && target_secs % secs == 0).then_some((secs, c))
        })
        .collect();

    // Requested-window floor: an explicit `start` bounds the old end; an unbounded request
    // wants full history, so the floor is the deepest available `earliest` — giving the
    // interval that reaches furthest back a zero deep-miss.
    let floor = window_start.unwrap_or_else(|| {
        divisors
            .iter()
            .map(|(_, c)| c.earliest)
            .min()
            .unwrap_or(now)
    });

    // Score = seconds of the requested window [floor, now] the interval cannot serve:
    // unreached history at the old end (deep-miss) plus staleness at the fresh end
    // (stale-miss). Lowest score wins; ties fall back to the larger divisor for
    // gap-tolerant fidelity (REQ-API-205).
    divisors
        .iter()
        .min_by_key(|(secs, c)| {
            let deep_miss = (c.earliest - floor).num_seconds().max(0);
            let stale_miss = (now - c.latest).num_seconds().max(0);
            (deep_miss + stale_miss, std::cmp::Reverse(*secs))
        })
        .map(|(_, c)| c.interval)
}

// ── Bucket alignment ──────────────────────────────────────────────────────────

/// Compute the UTC/epoch-aligned bucket start for `ts` at the given target duration.
///
/// Formula: `epoch_secs - epoch_secs.rem_euclid(target_secs)`.
///
/// A source candle with timestamp `ts` belongs to the half-open window
/// `[bucket_start(ts, target_secs), bucket_start(ts, target_secs) + target_secs)`.
///
// @MX:NOTE: [AUTO] `1w` bucket alignment anchors to epoch-Thursday (1970-01-01 UTC), not ISO Monday.
// @MX:REASON: Uniform epoch truncation is applied identically to all targets (OR-API3-6 resolved).
//             1970-01-01 is a Thursday; epoch-day sources (86400 s) tile epoch-weeks (604800 s)
//             exactly (604800 % 86400 == 0), so the N = 7 completeness count holds without a
//             second alignment rule. This is intentional — do NOT change to ISO Monday-anchored
//             weeks without updating the completeness predicate and the OR-API3-6 decision.
// @MX:SPEC: SPEC-API-003 REQ-API-208
pub fn bucket_start(ts: DateTime<Utc>, target_secs: i64) -> DateTime<Utc> {
    let epoch_secs = ts.timestamp();
    let bucket_epoch = epoch_secs - epoch_secs.rem_euclid(target_secs);
    DateTime::<Utc>::from_timestamp(bucket_epoch, 0)
        .expect("bucket_start always produces a valid UTC epoch second")
}

// ── OHLCV folding ─────────────────────────────────────────────────────────────

/// Fold component volumes with null propagation.
///
/// Returns the `Decimal` sum of all component volumes if every component is
/// `Some` (REQ-API-207a), or `None` if any component is `None` (REQ-API-207b).
/// Never silently coerces a missing component to zero.
///
// @MX:WARN: [AUTO] Volume null-propagation — any None component must yield None, not zero.
// @MX:REASON: The natural regression is using `.unwrap_or(Decimal::ZERO)` which silently drops
//             missing volume data and produces a falsely low total. REQ-API-207b requires that
//             the total be null whenever any component is null. CoinGecko candles carry null
//             volume; mixing them with non-null sources in one bucket must produce a null total.
//             Never use `.unwrap_or_default()` or `.flatten()` on the volume field here.
// @MX:SPEC: SPEC-API-003 REQ-API-207a REQ-API-207b REQ-API-216
fn fold_volume(volumes: &[Option<Decimal>]) -> Option<Decimal> {
    if volumes.iter().all(|v| v.is_some()) {
        Some(volumes.iter().map(|v| v.expect("all are Some")).sum())
    } else {
        None
    }
}

/// Aggregate source candles into target-interval OHLCV buckets.
///
/// **Bucket classification is by wall clock (`now`), never by page position.**
///
/// - **Forming bucket** (`bucket_start <= now < bucket_start + target_secs`): emitted even
///   when it has fewer than `N = target_secs / source_secs` source candles — this is the
///   live in-progress candle (REQ-API-210).
/// - **Closed bucket** (`bucket_start + target_secs <= now`): emitted only when complete
///   (exactly `N` distinct source candles present); dropped when any expected source candle
///   is missing (REQ-API-209). Nothing is fabricated or interpolated (REQ-API-211).
///
/// Output: `Vec<CoinCandle>` ordered `ts DESC`, `ts = bucket_start`,
/// `source = "aggregated:<source_interval_label>"`, `interval = target_interval`.
/// All OHLCV arithmetic is in `Decimal` (REQ-API-216).
///
// @MX:WARN: [AUTO] Wall-clock forming-bucket classifier — cursor-independence invariant.
// @MX:REASON: The forming bucket MUST be identified by `now`, NOT by the newest row in the
//             `source` slice. Under keyset pagination, a `cursor` or `end` bound makes the
//             page-newest bucket an old closed bucket that must be dropped (REQ-API-209).
//             Classifying by "newest row in input" would incorrectly promote a closed
//             incomplete bucket to forming status (OR-API3-3 RESOLVED, REQ-API-209/210).
//             `now` is injected by the caller; this function never reads the system clock.
// @MX:SPEC: SPEC-API-003 REQ-API-208 REQ-API-209 REQ-API-210 REQ-API-211 REQ-API-212
pub fn aggregate_candles(
    mut source: Vec<CoinCandle>,
    target_secs: i64,
    source_secs: i64,
    now: DateTime<Utc>,
    source_interval_label: &str,
    target_interval: &str,
) -> Vec<CoinCandle> {
    if source.is_empty() {
        return vec![];
    }

    // Sort ascending by ts so that BTreeMap insertion order matches ts-ASC,
    // making indices[0] the earliest (open) and indices[last] the latest (close).
    source.sort_by_key(|c| c.ts);

    // N = number of expected source candles in one complete closed bucket (precondition P1:
    // source ts are epoch-aligned to their own interval, so exactly N fit per target window).
    let n: usize = (target_secs / source_secs)
        .try_into()
        .expect("bucket size (target/source) fits in usize");

    let coin_id = source[0].coin_id.clone();
    let vs_currency = source[0].vs_currency.clone();
    let agg_source = format!("aggregated:{source_interval_label}");
    let now_epoch = now.timestamp();

    // Group source candles by epoch-aligned bucket_start (BTreeMap → ASC key order).
    // Values are indices into the sorted `source` slice (insertion-order ≡ ts-ASC).
    let mut bucket_map: std::collections::BTreeMap<i64, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (idx, c) in source.iter().enumerate() {
        let bs = bucket_start(c.ts, target_secs).timestamp();
        bucket_map.entry(bs).or_default().push(idx);
    }

    let mut result = Vec::with_capacity(bucket_map.len());

    for (bs_epoch, indices) in bucket_map {
        let bucket_end_epoch = bs_epoch + target_secs;
        // Wall-clock classification (must use `now`, not "newest row in input").
        let is_closed = bucket_end_epoch <= now_epoch;

        // REQ-API-209: drop closed buckets missing any of their N expected source candles.
        if is_closed && indices.len() < n {
            continue;
        }

        // Fold: indices are in ts-ASC insertion order because source was pre-sorted.
        let first = &source[indices[0]];
        let last = &source[*indices.last().expect("non-empty bucket")];

        let high = indices
            .iter()
            .map(|&i| source[i].high)
            .max()
            .expect("non-empty bucket");
        let low = indices
            .iter()
            .map(|&i| source[i].low)
            .min()
            .expect("non-empty bucket");
        let volumes: Vec<Option<Decimal>> = indices.iter().map(|&i| source[i].volume).collect();
        let volume = fold_volume(&volumes);

        let ts = DateTime::<Utc>::from_timestamp(bs_epoch, 0)
            .expect("bucket start is always a valid UTC timestamp");

        result.push(CoinCandle {
            coin_id: coin_id.clone(),
            vs_currency: vs_currency.clone(),
            interval: target_interval.to_string(),
            ts,
            open: first.open,
            high,
            low,
            close: last.close,
            volume,
            source: agg_source.clone(),
        });
    }

    // REQ-API-214: return ts DESC to match the endpoint's ORDER BY ts DESC contract.
    result.sort_by_key(|c| std::cmp::Reverse(c.ts));
    result
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rust_decimal_macros::dec;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn ts_epoch(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(secs, 0).unwrap()
    }

    // Equal-coverage helper for divisibility-focused selector tests: every interval shares
    // one span [epoch 0, epoch 1_000_000], so coverage is held constant and the test isolates
    // the divisibility rule + largest-divisor tie-break. Paired with `now = ts_epoch(1_000_000)`
    // so `stale_miss` is zero for all candidates.
    fn cov(interval: &str) -> IntervalCoverage<'_> {
        IntervalCoverage {
            interval,
            earliest: ts_epoch(0),
            latest: ts_epoch(1_000_000),
        }
    }

    const SEL_NOW: i64 = 1_000_000;

    fn make_candle(
        ts: DateTime<Utc>,
        open: Decimal,
        high: Decimal,
        low: Decimal,
        close: Decimal,
    ) -> CoinCandle {
        CoinCandle {
            coin_id: "bitcoin".into(),
            vs_currency: "usd".into(),
            interval: "1h".into(),
            ts,
            open,
            high,
            low,
            close,
            volume: Some(dec!(100)),
            source: "binance".into(),
        }
    }

    fn make_candle_null_vol(
        ts: DateTime<Utc>,
        open: Decimal,
        high: Decimal,
        low: Decimal,
        close: Decimal,
    ) -> CoinCandle {
        CoinCandle {
            coin_id: "dogecoin".into(),
            vs_currency: "usd".into(),
            interval: "30m".into(),
            ts,
            open,
            high,
            low,
            close,
            volume: None,
            source: "coingecko".into(),
        }
    }

    // ── T-001: interval_to_seconds ─────────────────────────────────────────────
    // Scenario: every vocab string maps to the exact seconds (REQ-API-203/204).

    #[test]
    fn interval_seconds_1m() {
        assert_eq!(interval_to_seconds("1m"), Some(60));
    }

    #[test]
    fn interval_seconds_3m() {
        assert_eq!(interval_to_seconds("3m"), Some(180));
    }

    #[test]
    fn interval_seconds_5m() {
        assert_eq!(interval_to_seconds("5m"), Some(300));
    }

    #[test]
    fn interval_seconds_15m() {
        assert_eq!(interval_to_seconds("15m"), Some(900));
    }

    #[test]
    fn interval_seconds_30m() {
        assert_eq!(interval_to_seconds("30m"), Some(1_800));
    }

    #[test]
    fn interval_seconds_1h() {
        assert_eq!(interval_to_seconds("1h"), Some(3_600));
    }

    #[test]
    fn interval_seconds_2h() {
        assert_eq!(interval_to_seconds("2h"), Some(7_200));
    }

    #[test]
    fn interval_seconds_4h() {
        assert_eq!(interval_to_seconds("4h"), Some(14_400));
    }

    #[test]
    fn interval_seconds_6h() {
        assert_eq!(interval_to_seconds("6h"), Some(21_600));
    }

    #[test]
    fn interval_seconds_8h() {
        assert_eq!(interval_to_seconds("8h"), Some(28_800));
    }

    #[test]
    fn interval_seconds_12h() {
        assert_eq!(interval_to_seconds("12h"), Some(43_200));
    }

    #[test]
    fn interval_seconds_1d() {
        assert_eq!(interval_to_seconds("1d"), Some(86_400));
    }

    #[test]
    fn interval_seconds_3d() {
        assert_eq!(interval_to_seconds("3d"), Some(259_200));
    }

    #[test]
    fn interval_seconds_4d() {
        assert_eq!(interval_to_seconds("4d"), Some(345_600));
    }

    #[test]
    fn interval_seconds_1w() {
        assert_eq!(interval_to_seconds("1w"), Some(604_800));
    }

    // REQ-API-204: 1M is not a fixed-second duration — must return None.
    #[test]
    fn interval_seconds_1m_calendar_month_is_none() {
        assert_eq!(interval_to_seconds("1M"), None);
    }

    // REQ-API-204: unrecognised strings return None.
    #[test]
    fn interval_seconds_unknown_strings_are_none() {
        assert_eq!(interval_to_seconds(""), None);
        assert_eq!(interval_to_seconds("2d"), None);
        assert_eq!(interval_to_seconds("10h"), None);
        assert_eq!(interval_to_seconds("1hour"), None);
        assert_eq!(interval_to_seconds("monthly"), None);
    }

    // Verify the full table in one pass (REQ-API-203 worked examples from spec.md).
    #[test]
    fn interval_seconds_full_table_matches_spec() {
        let table = [
            ("1m", 60i64),
            ("3m", 180),
            ("5m", 300),
            ("15m", 900),
            ("30m", 1_800),
            ("1h", 3_600),
            ("2h", 7_200),
            ("4h", 14_400),
            ("6h", 21_600),
            ("8h", 28_800),
            ("12h", 43_200),
            ("1d", 86_400),
            ("3d", 259_200),
            ("4d", 345_600),
            ("1w", 604_800),
        ];
        for (iv, expected) in table {
            assert_eq!(
                interval_to_seconds(iv),
                Some(expected),
                "interval_to_seconds({iv:?}) must be {expected}"
            );
        }
    }

    // ── T-002: select_source_interval ──────────────────────────────────────────
    // With coverage held equal (via `cov` + `SEL_NOW`), these isolate the divisibility rule
    // and the largest-divisor tie-break — the behaviour preserved from the pre-coverage rule.

    #[test]
    fn select_4h_from_mixed_picks_1h() {
        // Target 4h = 14400s. Largest divisor among {3600,900,300,60} is 3600 (1h).
        let stored = ["1h", "15m", "5m", "1m"].map(cov);
        let result = select_source_interval(&stored, 14_400, None, ts_epoch(SEL_NOW));
        assert_eq!(result, Some("1h"));
    }

    // Scenario 3 (REQ-API-205): equal coverage → largest divisor. 1d over 30m/4h/4d.
    #[test]
    fn select_1d_picks_largest_divisor_4h_scenario_3() {
        // Target 1d = 86400. Divisors among stored: 30m(1800, 86400%1800==0 ✓),
        // 4h(14400, 86400%14400==0 ✓), 4d(345600 > 86400 → excluded).
        // Coverage equal → tie-break to the largest divisor: 14400 → "4h".
        let stored = ["30m", "4h", "4d"].map(cov);
        let result = select_source_interval(&stored, 86_400, None, ts_epoch(SEL_NOW));
        assert_eq!(
            result,
            Some("4h"),
            "equal coverage → largest divisor of 1d must be 4h"
        );
    }

    // Scenario 4 (REQ-API-204/205): 1h←30m on a CoinGecko coin storing 30m/4h/4d.
    #[test]
    fn select_1h_from_30m_scenario_4() {
        // Target 1h = 3600. 4h(14400 > 3600 → excluded), 4d(345600 → excluded),
        // 30m(1800, 3600%1800==0 ✓) → "30m".
        let stored = ["30m", "4h", "4d"].map(cov);
        let result = select_source_interval(&stored, 3_600, None, ts_epoch(SEL_NOW));
        assert_eq!(result, Some("30m"));
    }

    // Scenario 9/10 (REQ-API-203): 1h over {4h,4d} → None (3600 % 14400 != 0).
    #[test]
    fn select_none_when_no_divisor_scenario_9() {
        let stored = ["4h", "4d"].map(cov);
        let result = select_source_interval(&stored, 3_600, None, ts_epoch(SEL_NOW));
        assert_eq!(result, None, "neither 4h nor 4d divides 1h");
    }

    // Scenario 10 (REQ-API-203): 1w←30m (604800 % 1800 == 0 → valid).
    #[test]
    fn select_1w_from_30m_scenario_10() {
        let stored = ["30m"].map(cov);
        let result = select_source_interval(&stored, 604_800, None, ts_epoch(SEL_NOW));
        assert_eq!(result, Some("30m"));
    }

    // REQ-API-204: 1M is excluded even when present.
    #[test]
    fn select_excludes_calendar_month_interval() {
        // 1M has no fixed second count → interval_to_seconds returns None → excluded.
        // "30m" remains as the only valid divisor of 1d (86400 % 1800 == 0).
        let stored = ["1M", "30m"].map(cov);
        let result = select_source_interval(&stored, 86_400, None, ts_epoch(SEL_NOW));
        assert_eq!(
            result,
            Some("30m"),
            "1M must be excluded; 30m is the valid divisor"
        );
    }

    // Empty stored set.
    #[test]
    fn select_none_on_empty_stored() {
        let stored: [IntervalCoverage; 0] = [];
        assert_eq!(
            select_source_interval(&stored, 14_400, None, ts_epoch(SEL_NOW)),
            None
        );
    }

    // Source with same secs as target must not be selected (target itself is native path).
    #[test]
    fn select_none_when_source_equals_target() {
        // "4h" stored, target is also 4h — same interval, not a divisor (secs < target required).
        let stored = ["4h"].map(cov);
        let result = select_source_interval(&stored, 14_400, None, ts_epoch(SEL_NOW));
        assert_eq!(result, None, "source == target must not be selected");
    }

    // ── T-002b: coverage-aware selection (this bug) ────────────────────────────
    // Divisibility alone is blind to how much history each interval holds. When a deep,
    // fine series and a shallow, stale coarse series both divide the target, the deep one
    // must win so the derived series spans full history and reaches the freshest candle.

    fn dt(y: i32, mo: u32, d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, 0, 0, 0).unwrap()
    }

    // REGRESSION (reported bug): BTC stores a 9-year 5m backfill plus a 1-month, 4-day-stale
    // 4h series. A 1d request must aggregate from 5m (deep + fresh), NOT the shallow 4h that
    // the old largest-divisor rule selected.
    #[test]
    fn select_1d_prefers_deep_5m_over_shallow_stale_4h() {
        let now = dt(2026, 7, 5);
        let stored = [
            IntervalCoverage {
                interval: "5m",
                earliest: dt(2017, 8, 17),
                latest: now,
            },
            IntervalCoverage {
                interval: "4h",
                earliest: dt(2026, 6, 3),
                latest: dt(2026, 7, 1), // 4 days stale
            },
            IntervalCoverage {
                interval: "1m",
                earliest: dt(2026, 6, 30),
                latest: now,
            },
        ];
        let result = select_source_interval(&stored, 86_400, None, now);
        assert_eq!(
            result,
            Some("5m"),
            "1d must aggregate from the deep 5m series, not the shallow 4h"
        );
    }

    // Same defect for 1w — the coarsest divisor was equally blind to depth.
    #[test]
    fn select_1w_prefers_deep_5m_over_shallow_stale_4h() {
        let now = dt(2026, 7, 5);
        let stored = [
            IntervalCoverage {
                interval: "5m",
                earliest: dt(2017, 8, 17),
                latest: now,
            },
            IntervalCoverage {
                interval: "4h",
                earliest: dt(2026, 6, 3),
                latest: dt(2026, 7, 1),
            },
        ];
        let result = select_source_interval(&stored, 604_800, None, now);
        assert_eq!(
            result,
            Some("5m"),
            "1w must aggregate from the deep 5m series, not the shallow 4h"
        );
    }

    // Fidelity preserved: when a coarser divisor covers the window just as well and is
    // equally fresh, it wins the tie-break (fewer source candles per bucket → fewer
    // gap-dropped buckets). No needless downgrade to the finest interval.
    #[test]
    fn select_1d_prefers_larger_divisor_when_coverage_equal() {
        let now = dt(2026, 7, 5);
        let earliest = dt(2026, 6, 1);
        let stored = [
            IntervalCoverage {
                interval: "5m",
                earliest,
                latest: now,
            },
            IntervalCoverage {
                interval: "4h",
                earliest,
                latest: now,
            },
        ];
        let result = select_source_interval(&stored, 86_400, None, now);
        assert_eq!(
            result,
            Some("4h"),
            "equal coverage → larger divisor for gap-tolerant fidelity"
        );
    }

    // Staleness term: a fresh fine interval beats a stale coarse one even when both reach
    // equally far back — this kills the "4-days-stale tail" symptom.
    #[test]
    fn select_1d_prefers_fresh_source_over_stale_coarse() {
        let now = dt(2026, 7, 5);
        let earliest = dt(2026, 6, 1);
        let stored = [
            IntervalCoverage {
                interval: "5m",
                earliest,
                latest: now, // fresh
            },
            IntervalCoverage {
                interval: "4h",
                earliest,
                latest: dt(2026, 7, 1), // 4 days stale
            },
        ];
        let result = select_source_interval(&stored, 86_400, None, now);
        assert_eq!(
            result,
            Some("5m"),
            "stale 4h must lose to fresh 5m via the staleness term"
        );
    }

    // Bounded request: an explicit `start` sets the floor. A coarse interval that fully
    // covers [start, now] wins on fidelity even though a finer one reaches deeper history
    // the caller did not ask for.
    #[test]
    fn select_1d_bounded_window_prefers_covering_larger_divisor() {
        let now = dt(2026, 7, 5);
        let start = dt(2026, 6, 10);
        let stored = [
            IntervalCoverage {
                interval: "5m",
                earliest: dt(2017, 8, 17),
                latest: now,
            },
            IntervalCoverage {
                interval: "4h",
                earliest: dt(2026, 6, 3), // covers `start`
                latest: now,              // fresh in this scenario
            },
        ];
        let result = select_source_interval(&stored, 86_400, Some(start), now);
        assert_eq!(
            result,
            Some("4h"),
            "both cover [start, now] → larger divisor 4h wins on fidelity"
        );
    }

    // ── T-003: bucket_start ────────────────────────────────────────────────────

    // 4h bucket alignment (REQ-API-208).
    #[test]
    fn bucket_start_4h_alignment() {
        // ts = epoch + 14400 + 3600 (1h into the second 4h bucket).
        // bucket_start should be 14400.
        let ts = ts_epoch(14_400 + 3_600);
        let bs = bucket_start(ts, 14_400);
        assert_eq!(bs.timestamp(), 14_400);
    }

    // 1d bucket alignment.
    #[test]
    fn bucket_start_1d_alignment() {
        // Any ts within 2026-06-01 UTC should bucket to 2026-06-01 00:00:00 UTC.
        let ts = Utc.with_ymd_and_hms(2026, 6, 1, 13, 30, 0).unwrap();
        let bs = bucket_start(ts, 86_400);
        let expected = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        assert_eq!(bs, expected);
    }

    // 1w anchors to epoch-Thursday, not ISO Monday (OR-API3-6 RESOLVED).
    #[test]
    fn bucket_start_1w_anchors_to_epoch_thursday() {
        // Epoch 0 = 1970-01-01 = Thursday. bucket_start(epoch_0 + offset, 1w) should be
        // an epoch-Thursday boundary for any offset within the first week.
        let ts = ts_epoch(100_000); // 100000s into 1970-01-01 (Thursday)
        let bs = bucket_start(ts, 604_800);
        // Epoch 0 is a Thursday; the week starts at 0.
        assert_eq!(
            bs.timestamp(),
            0,
            "first epoch-week bucket must start at epoch 0 (Thursday)"
        );
        // Verify it is a Thursday.
        use chrono::Datelike;
        assert_eq!(
            bs.weekday(),
            chrono::Weekday::Thu,
            "1w bucket start must be a Thursday"
        );
    }

    // 1w: a ts in the second week lands in the correct bucket.
    #[test]
    fn bucket_start_1w_second_week() {
        // ts = 604800 + 10000 (in the second epoch-week).
        let ts = ts_epoch(604_800 + 10_000);
        let bs = bucket_start(ts, 604_800);
        assert_eq!(bs.timestamp(), 604_800);
        use chrono::Datelike;
        assert_eq!(bs.weekday(), chrono::Weekday::Thu);
    }

    // Boundary: ts exactly at bucket_start belongs to that bucket (not the previous one).
    #[test]
    fn bucket_start_boundary_exact_start_is_inclusive() {
        let ts = ts_epoch(14_400); // exactly at the start of the second 4h bucket
        let bs = bucket_start(ts, 14_400);
        assert_eq!(
            bs.timestamp(),
            14_400,
            "ts == bucket_start is in that bucket"
        );
    }

    // Boundary: ts at bucket_start + target - 1s is in the SAME bucket (half-open window end is exclusive).
    #[test]
    fn bucket_start_boundary_one_second_before_end_is_same_bucket() {
        let ts = ts_epoch(14_400 + 14_400 - 1); // one second before second bucket closes
        let bs = bucket_start(ts, 14_400);
        assert_eq!(
            bs.timestamp(),
            14_400,
            "ts one second before end is still in the same bucket"
        );
    }

    // Boundary: ts at bucket_start + target is in the NEXT bucket.
    #[test]
    fn bucket_start_boundary_next_bucket() {
        let ts = ts_epoch(14_400 + 14_400); // exact start of third bucket
        let bs = bucket_start(ts, 14_400);
        assert_eq!(
            bs.timestamp(),
            28_800,
            "ts at next boundary belongs to next bucket"
        );
    }

    // ── T-004: OHLC fold (tested via aggregate_candles) ────────────────────────
    // Scenario 2 (REQ-API-206/208/212): 4h aggregated from 4×1h candles.

    #[test]
    fn aggregate_candles_scenario_2_ohlc_fold() {
        // Four 1h source candles in one epoch-aligned 4h bucket [0, 14400).
        // open=first.open=100, high=max=130, low=min=95, close=last.close=120.
        let now = ts_epoch(100_000_000); // far future → all buckets closed

        let source = vec![
            {
                let mut c = make_candle(ts_epoch(0), dec!(100), dec!(110), dec!(95), dec!(108));
                c.interval = "1h".into();
                c
            },
            {
                let mut c =
                    make_candle(ts_epoch(3_600), dec!(108), dec!(120), dec!(100), dec!(115));
                c.interval = "1h".into();
                c
            },
            {
                let mut c =
                    make_candle(ts_epoch(7_200), dec!(115), dec!(130), dec!(105), dec!(125));
                c.interval = "1h".into();
                c
            },
            {
                let mut c =
                    make_candle(ts_epoch(10_800), dec!(125), dec!(128), dec!(110), dec!(120));
                c.interval = "1h".into();
                c
            },
        ];

        let agg = aggregate_candles(source, 14_400, 3_600, now, "1h", "4h");

        assert_eq!(agg.len(), 1, "one complete 4h bucket expected");
        let candle = &agg[0];
        assert_eq!(candle.open, dec!(100), "open = first candle's open");
        assert_eq!(candle.high, dec!(130), "high = max across bucket");
        assert_eq!(candle.low, dec!(95), "low = min across bucket");
        assert_eq!(candle.close, dec!(120), "close = last candle's close");
        assert_eq!(candle.ts.timestamp(), 0, "ts == bucket_start");
        assert_eq!(candle.source, "aggregated:1h", "REQ-API-212");
        assert_eq!(candle.interval, "4h", "output interval = target");
    }

    // ── T-005: volume fold ─────────────────────────────────────────────────────

    // Scenario 5 (REQ-API-207a): all volumes present → sum.
    #[test]
    fn fold_volume_all_some_sums_correctly() {
        let vols = vec![Some(dec!(500)), Some(dec!(700)), Some(dec!(300))];
        assert_eq!(fold_volume(&vols), Some(dec!(1500)));
    }

    // Scenario 6 (REQ-API-207b): one None component → None (not partial sum).
    #[test]
    fn fold_volume_one_none_yields_none() {
        let vols = vec![Some(dec!(500)), None, Some(dec!(700))];
        assert_eq!(
            fold_volume(&vols),
            None,
            "any null component must produce null total"
        );
    }

    // All None → None.
    #[test]
    fn fold_volume_all_none_is_none() {
        let vols = vec![None, None, None];
        assert_eq!(fold_volume(&vols), None);
    }

    // Scenario 6 via aggregate_candles: null volume propagation.
    #[test]
    fn aggregate_candles_volume_null_propagation_scenario_6() {
        // Target 1h = 3600s from source 30m = 1800s (N=2).
        // One 30m source candle has null volume.
        let now = ts_epoch(100_000_000);

        let c0 = {
            let mut c = make_candle(ts_epoch(0), dec!(10), dec!(12), dec!(9), dec!(11));
            c.volume = Some(dec!(400));
            c.interval = "30m".into();
            c
        };
        let c1 = make_candle_null_vol(ts_epoch(1_800), dec!(11), dec!(13), dec!(10), dec!(12));

        let agg = aggregate_candles(vec![c0, c1], 3_600, 1_800, now, "30m", "1h");

        assert_eq!(agg.len(), 1);
        assert!(
            agg[0].volume.is_none(),
            "REQ-API-207b: any null component yields null total"
        );
    }

    // Scenario 5 via aggregate_candles: all volumes sum correctly.
    #[test]
    fn aggregate_candles_volume_sum_scenario_5() {
        let now = ts_epoch(100_000_000);

        // 4h from 4×1h. Volumes: 300, 500, 400, 300. Sum = 1500.
        let mut source = vec![
            make_candle(ts_epoch(0), dec!(1), dec!(2), dec!(1), dec!(2)),
            make_candle(ts_epoch(3_600), dec!(2), dec!(3), dec!(2), dec!(3)),
            make_candle(ts_epoch(7_200), dec!(3), dec!(4), dec!(3), dec!(4)),
            make_candle(ts_epoch(10_800), dec!(4), dec!(5), dec!(4), dec!(5)),
        ];
        source[0].volume = Some(dec!(300));
        source[1].volume = Some(dec!(500));
        source[2].volume = Some(dec!(400));
        source[3].volume = Some(dec!(300));

        let agg = aggregate_candles(source, 14_400, 3_600, now, "1h", "4h");

        assert_eq!(agg.len(), 1);
        assert_eq!(
            agg[0].volume,
            Some(dec!(1500)),
            "REQ-API-207a: all present → sum = 1500"
        );
    }

    // ── T-006: aggregate_candles — partial/gap bucket policy ──────────────────

    // Scenario 7 (REQ-API-209/211): closed interior gap bucket is dropped; neighbours present.
    #[test]
    fn aggregate_candles_closed_gap_bucket_dropped_scenario_7() {
        // 4h from 1h (N=4). Three buckets:
        //   B0 [0, 14400)   — 4 candles → complete → kept
        //   B1 [14400,28800) — 3 candles → incomplete → DROPPED
        //   B2 [28800,43200) — 4 candles → complete → kept
        // now is far future → all closed.
        let now = ts_epoch(100_000_000);

        let mut source: Vec<CoinCandle> = vec![];
        // B0 (complete)
        for h in [0i64, 3_600, 7_200, 10_800] {
            let mut c = make_candle(ts_epoch(h), dec!(100), dec!(110), dec!(90), dec!(105));
            c.interval = "1h".into();
            source.push(c);
        }
        // B1 (incomplete — missing the 4th candle at ts=25200)
        for h in [14_400i64, 18_000, 21_600] {
            let mut c = make_candle(ts_epoch(h), dec!(100), dec!(110), dec!(90), dec!(105));
            c.interval = "1h".into();
            source.push(c);
        }
        // B2 (complete)
        for h in [28_800i64, 32_400, 36_000, 39_600] {
            let mut c = make_candle(ts_epoch(h), dec!(100), dec!(110), dec!(90), dec!(105));
            c.interval = "1h".into();
            source.push(c);
        }

        let agg = aggregate_candles(source, 14_400, 3_600, now, "1h", "4h");

        // Only B0 and B2 (ts DESC → B2 first).
        let tses: Vec<i64> = agg.iter().map(|c| c.ts.timestamp()).collect();
        assert_eq!(
            tses,
            vec![28_800, 0],
            "B1 (incomplete closed) must be dropped; B0 and B2 kept"
        );
        // Confirm B1's timestamp is absent.
        assert!(
            agg.iter().all(|c| c.ts.timestamp() != 14_400),
            "B1 ts=14400 must not appear in result"
        );
    }

    // Scenario 8 (REQ-API-210): forming bucket emitted even if incomplete.
    #[test]
    fn aggregate_candles_forming_bucket_emitted_incomplete_scenario_8() {
        // Target 4h from 1h (N=4). One bucket [0, 14400) with only 2 of 4 candles.
        // now = epoch 7200 → inside [0, 14400) → forming → must be emitted.
        let now = ts_epoch(7_200); // 2h into the forming window

        let source = vec![
            {
                let mut c = make_candle(ts_epoch(0), dec!(50), dec!(60), dec!(45), dec!(55));
                c.interval = "1h".into();
                c
            },
            {
                let mut c = make_candle(ts_epoch(3_600), dec!(55), dec!(65), dec!(50), dec!(62));
                c.interval = "1h".into();
                c
            },
        ];

        let agg = aggregate_candles(source, 14_400, 3_600, now, "1h", "4h");

        assert_eq!(
            agg.len(),
            1,
            "forming bucket must be emitted even when incomplete"
        );
        let c = &agg[0];
        assert_eq!(c.ts.timestamp(), 0, "ts = bucket_start");
        assert_eq!(c.open, dec!(50), "open = first candle open");
        assert_eq!(c.close, dec!(62), "close = last available candle close");
        assert_eq!(c.high, dec!(65), "high = max over available candles");
        assert_eq!(c.low, dec!(45), "low = min over available candles");
        assert_eq!(c.source, "aggregated:1h");
    }

    // Scenario 8b (REQ-API-209/210): closed incomplete bucket dropped even when newest in source.
    // This verifies the wall-clock invariant: "newest in input" ≠ "forming bucket".
    #[test]
    fn aggregate_candles_closed_incomplete_dropped_even_if_newest_scenario_8b() {
        // Target 4h from 1h (N=4).
        // B_old [0, 14400)   — 4 candles, closed, complete → kept
        // B_new [28800,43200) — 3 candles, closed, incomplete → DROPPED (newest in input)
        // now = far future → both closed.
        let now = ts_epoch(100_000_000);

        let mut source: Vec<CoinCandle> = vec![];
        // B_old (complete, older)
        for h in [0i64, 3_600, 7_200, 10_800] {
            let mut c = make_candle(ts_epoch(h), dec!(10), dec!(12), dec!(9), dec!(11));
            c.interval = "1h".into();
            source.push(c);
        }
        // B_new (incomplete, newer)
        for h in [28_800i64, 32_400, 36_000] {
            let mut c = make_candle(ts_epoch(h), dec!(20), dec!(22), dec!(19), dec!(21));
            c.interval = "1h".into();
            source.push(c);
        }

        let agg = aggregate_candles(source, 14_400, 3_600, now, "1h", "4h");

        let tses: Vec<i64> = agg.iter().map(|c| c.ts.timestamp()).collect();
        assert_eq!(
            tses,
            vec![0],
            "closed-incomplete bucket (ts=28800) must be dropped even if newest"
        );
        assert!(
            agg.iter().all(|c| c.ts.timestamp() != 28_800),
            "B_new ts=28800 must not appear — it is closed and incomplete"
        );
    }

    // REQ-API-212: source label is set to aggregated:<source_interval_label>.
    #[test]
    fn aggregate_candles_source_label_is_aggregated_prefix() {
        let now = ts_epoch(100_000_000);
        let source: Vec<CoinCandle> = (0..4)
            .map(|i| {
                let mut c = make_candle(ts_epoch(i * 3_600), dec!(1), dec!(2), dec!(1), dec!(2));
                c.interval = "1h".into();
                c
            })
            .collect();

        let agg = aggregate_candles(source, 14_400, 3_600, now, "1h", "4h");

        assert!(!agg.is_empty());
        for c in &agg {
            assert_eq!(
                c.source, "aggregated:1h",
                "REQ-API-212: label must be aggregated:1h"
            );
            assert_eq!(
                c.interval, "4h",
                "output interval must be the target interval"
            );
        }
    }

    // Output is ts DESC.
    #[test]
    fn aggregate_candles_output_ordered_ts_desc() {
        let now = ts_epoch(100_000_000);
        let mut source: Vec<CoinCandle> = vec![];
        // Two complete 4h buckets.
        for h in [0i64, 3_600, 7_200, 10_800, 14_400, 18_000, 21_600, 25_200] {
            let mut c = make_candle(ts_epoch(h), dec!(1), dec!(2), dec!(1), dec!(2));
            c.interval = "1h".into();
            source.push(c);
        }
        let agg = aggregate_candles(source, 14_400, 3_600, now, "1h", "4h");
        assert_eq!(agg.len(), 2);
        assert!(agg[0].ts > agg[1].ts, "output must be ordered ts DESC");
    }

    // Empty source returns empty.
    #[test]
    fn aggregate_candles_empty_source_returns_empty() {
        let now = ts_epoch(100_000_000);
        let agg = aggregate_candles(vec![], 14_400, 3_600, now, "1h", "4h");
        assert!(agg.is_empty());
    }
}
