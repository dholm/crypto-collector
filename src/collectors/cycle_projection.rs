//! Composite halving-cycle projection model (SPEC-CYCLE-001 v0.4.0, REQ-CYCLE-060..064).
//!
//! Replaces the v0.3.0 "cycle-repeat replay" with a research-grounded decomposition,
//! selected by walk-forward backtest (43 origins 2016→2026, log10 RMSE 0.201 vs the
//! replay's 0.394 — see `docs/prediction-research.md` §7):
//!
//! ```text
//! log10 P̂(today + k) = S(today + k)          power-law spine (log-log OLS)
//!                     + C(phase(today + k))   damped, phase-conditioned cycle component
//!                     + g₀ · ρᵏ               continuity / mean-reversion term
//! P10/P90 bands       = P̂ · 10^offset(k)     blended empirical horizon quantiles
//! ```
//!
//! Every term is deterministic and fit only from the stored daily closes (plus, for
//! `bitcoin`/`usd`, a small compiled-in set of historical calibration anchors — §
//! `CALIBRATION_ANCHORS`). No RNG, no external inputs, `Decimal` end-to-end
//! (REQ-CYCLE-024 / REQ-PROV-012).

use chrono::NaiveDate;
use rust_decimal::{Decimal, MathematicalOps};
use rust_decimal_macros::dec;
use std::collections::BTreeMap;

use super::cycle_overlay::{assign_cycle_in, projected_halving_dates, OverlayPoint, CYCLE_DAYS};

// ── Model constants (provenance: docs/prediction-research.md §7.5) ─────────────

/// Bitcoin genesis block date — the power-law spine's time origin.
const GENESIS: (i32, u32, u32) = (2009, 1, 3);

/// Mean-reversion half-life of today's deviation from the model, in days.
/// Ablation-selected (180 vs 255 days); independently corroborated by the
/// BTCautoresearch walk-forward study (research log §4.6/§7.2).
const HALF_LIFE_DAYS: u32 = 180;

/// Shrinkage applied to the cycle-phase component (guards against overfitting the
/// 3–4 observed cycles; ablation-flat optimum 0.5–0.75, research log §7.2).
fn shrink() -> Decimal {
    dec!(0.75)
}

/// Number of phase bins per halving cycle for the cycle component.
const NBINS: usize = 12;

/// Minimum daily observations a cycle needs before its residual curve is used.
const MIN_CYCLE_OBS: usize = 180;

/// Floor for the extrapolated cycle amplitude (log10) — keeps damping from
/// extrapolating the cycle component to zero or below.
fn amp_floor() -> Decimal {
    dec!(0.05)
}

/// Horizon grid step (days) for the empirical band quantiles.
const BAND_GRID_STEP: i64 = 30;

/// Minimum sample pairs a horizon needs for its quantiles to be trusted.
const MIN_BAND_SAMPLES: usize = 100;

/// Assumed length (days) of a cycle whose next halving is unknown (2024-04-20 →
/// estimated 2028-04-20).
const ASSUMED_CYCLE_LEN: i64 = 1461;

/// Compiled-in historical BTC/USD quarterly closes, 2011-08-18 → 2017-08-10
/// (Bitstamp daily closes; public immutable historical facts, same status as the
/// halving-date constants — D6). Used ONLY as regression calibration anchors for the
/// power-law spine when the stored history does not reach back that far: without the
/// early-era leverage points a 2017+ fit degrades from exponent ≈ 5.6 to ≈ 4.5
/// (research log §2). Never emitted as data points.
const CALIBRATION_ANCHORS: [(&str, &str); 25] = [
    ("2011-08-18", "10.90"),
    ("2011-11-17", "2.99"),
    ("2012-02-16", "4.51"),
    ("2012-05-17", "5.02"),
    ("2012-08-16", "13.42"),
    ("2012-11-15", "11.05"),
    ("2013-02-14", "27.40"),
    ("2013-05-16", "113.35"),
    ("2013-08-15", "98.08"),
    ("2013-11-14", "416.95"),
    ("2014-02-13", "604.11"),
    ("2014-05-15", "446.00"),
    ("2014-08-14", "507.99"),
    ("2014-11-13", "420.93"),
    ("2015-02-12", "223.00"),
    ("2015-05-14", "237.13"),
    ("2015-08-13", "264.48"),
    ("2015-11-12", "337.17"),
    ("2016-02-11", "377.98"),
    ("2016-05-12", "455.20"),
    ("2016-08-11", "585.22"),
    ("2016-11-10", "714.47"),
    ("2017-02-09", "986.00"),
    ("2017-05-11", "1828.45"),
    ("2017-08-10", "3413.03"),
];

/// Days each quarterly anchor represents in the weighted spine fit.
fn anchor_weight() -> Decimal {
    dec!(91)
}

// ── Small numeric helpers (Decimal, `maths` feature) ──────────────────────────

fn genesis() -> NaiveDate {
    let (y, m, d) = GENESIS;
    NaiveDate::from_ymd_opt(y, m, d).expect("valid compiled-in genesis date")
}

/// `10^x` in `Decimal` (via `exp(x · ln 10)`).
fn pow10(x: Decimal) -> Decimal {
    (x * dec!(10).ln()).exp()
}

/// Weighted OLS fit `y = a + b·x` over `(x, y, w)` samples. Returns `None` when the
/// x-variance is zero (degenerate input).
fn weighted_ols(samples: &[(Decimal, Decimal, Decimal)]) -> Option<(Decimal, Decimal)> {
    let w_sum: Decimal = samples.iter().map(|&(_, _, w)| w).sum();
    if w_sum <= Decimal::ZERO {
        return None;
    }
    let mx: Decimal = samples.iter().map(|&(x, _, w)| x * w).sum::<Decimal>() / w_sum;
    let my: Decimal = samples.iter().map(|&(_, y, w)| y * w).sum::<Decimal>() / w_sum;
    let sxx: Decimal = samples
        .iter()
        .map(|&(x, _, w)| w * (x - mx) * (x - mx))
        .sum();
    if sxx <= Decimal::ZERO {
        return None;
    }
    let sxy: Decimal = samples
        .iter()
        .map(|&(x, y, w)| w * (x - mx) * (y - my))
        .sum();
    let b = sxy / sxx;
    Some((my - b * mx, b))
}

/// Empirical quantile with linear interpolation between order statistics.
/// `sorted` must be ascending and non-empty; `q` in `[0, 1]`.
fn quantile(sorted: &[Decimal], q: Decimal) -> Decimal {
    let n = sorted.len();
    if n == 1 {
        return sorted[0];
    }
    let pos = q * Decimal::from(n - 1);
    let idx = pos.floor();
    let frac = pos - idx;
    let i: usize = idx.try_into().unwrap_or(0);
    let i = i.min(n - 1);
    let j = (i + 1).min(n - 1);
    sorted[i] + frac * (sorted[j] - sorted[i])
}

// ── Cycle-phase machinery ──────────────────────────────────────────────────────

/// `(cycle_index, phase ∈ [0, 1))` of a date against the extended halving list
/// (real halvings + one estimated), or `None` before the first halving.
/// `cycle_index` is 0-based into `halvings`.
fn phase_of(date: NaiveDate, halvings: &[NaiveDate]) -> Option<(usize, Decimal)> {
    if date < halvings[0] {
        return None;
    }
    let mut idx = 0usize;
    for (i, &h) in halvings.iter().enumerate() {
        if date >= h {
            idx = i;
        } else {
            break;
        }
    }
    let start = halvings[idx];
    let len = if idx + 1 < halvings.len() {
        (halvings[idx + 1] - start).num_days()
    } else {
        ASSUMED_CYCLE_LEN
    };
    let phase = Decimal::from((date - start).num_days()) / Decimal::from(len);
    Some((idx, phase.min(dec!(0.9999))))
}

/// Piecewise-linear interpolation over per-bin values at bin centres
/// `(b + 0.5) / NBINS`. Returns `None` when both flanking bins are absent.
fn interp_bins(curve: &BTreeMap<usize, Decimal>, phase: Decimal) -> Option<Decimal> {
    let x = phase * Decimal::from(NBINS) - dec!(0.5);
    let lo_i = x.floor();
    let lo: i64 = lo_i.try_into().unwrap_or(0);
    let lo = lo.clamp(0, (NBINS - 1) as i64) as usize;
    let hi = ((lo_i + Decimal::ONE)
        .try_into()
        .unwrap_or(0i64)
        .clamp(0, (NBINS - 1) as i64)) as usize;
    match (curve.get(&lo), curve.get(&hi)) {
        (None, None) => None,
        (Some(&v), None) | (None, Some(&v)) => Some(v),
        (Some(&lv), Some(&hv)) => {
            if lo == hi {
                Some(lv)
            } else {
                let t = (x - lo_i).clamp(Decimal::ZERO, Decimal::ONE);
                Some(lv + t * (hv - lv))
            }
        }
    }
}

// ── Fitted model ───────────────────────────────────────────────────────────────

/// The fitted composite model — all state needed to evaluate the centre path and
/// band offsets at any future date.
struct FittedModel {
    /// Power-law spine intercept/slope: `log10 P = a + b · log10(days since genesis)`.
    spine_a: Decimal,
    spine_b: Decimal,
    /// Per-cycle phase-binned mean residual curves (only cycles with enough data).
    curves: BTreeMap<usize, BTreeMap<usize, Decimal>>,
    /// Per-cycle amplitude (max bin mean) of each curve in `curves`.
    amps: BTreeMap<usize, Decimal>,
    /// Extended halving list (real + one estimated) used for phase assignment.
    halvings: Vec<NaiveDate>,
    /// Daily mean-reversion factor `ρ = 0.5^(1 / HALF_LIFE_DAYS)`.
    rho: Decimal,
}

impl FittedModel {
    fn spine(&self, date: NaiveDate) -> Decimal {
        let days = Decimal::from((date - genesis()).num_days());
        self.spine_a + self.spine_b * days.log10()
    }

    /// Damping target: linear extrapolation of per-cycle amplitude over cycle index,
    /// floored — encodes the measured diminishing returns (research log §3.2).
    fn amp_target(&self, cycle_idx: usize) -> Decimal {
        if self.amps.len() < 2 {
            return *self.amps.values().last().unwrap_or(&Decimal::ZERO);
        }
        let pts: Vec<(Decimal, Decimal, Decimal)> = self
            .amps
            .iter()
            .map(|(&i, &a)| (Decimal::from(i as u64), a, Decimal::ONE))
            .collect();
        match weighted_ols(&pts) {
            Some((a0, sl)) => (a0 + sl * Decimal::from(cycle_idx as u64)).max(amp_floor()),
            None => *self.amps.values().last().unwrap_or(&Decimal::ZERO),
        }
    }

    /// The damped, shrunk cycle-phase component `C(date)`.
    fn cycle_component(&self, date: NaiveDate) -> Decimal {
        let Some((cycle_idx, phase)) = phase_of(date, &self.halvings) else {
            return Decimal::ZERO;
        };
        if self.curves.is_empty() {
            return Decimal::ZERO;
        }
        let target = self.amp_target(cycle_idx);
        let mut num = Decimal::ZERO;
        let mut den = Decimal::ZERO;
        for (&c, curve) in &self.curves {
            let amp = self.amps[&c];
            let scale = if amp > amp_floor() {
                target / amp
            } else {
                Decimal::ONE
            };
            if let Some(v) = interp_bins(curve, phase) {
                num += scale * v;
                den += Decimal::ONE;
            }
        }
        if den.is_zero() {
            Decimal::ZERO
        } else {
            shrink() * num / den
        }
    }
}

/// Fit the composite model from the daily close series.
///
/// `use_btc_anchors` adds the compiled-in pre-2017 quarterly anchors to the spine fit
/// — only anchors strictly *before* the earliest stored day are used, so overlapping
/// history is never double-counted.
fn fit_model(daily: &BTreeMap<NaiveDate, Decimal>, use_btc_anchors: bool) -> Option<FittedModel> {
    let earliest = *daily.keys().next()?;
    let g = genesis();

    // Spine: weighted log-log OLS over daily closes (+ optional anchors).
    let mut samples: Vec<(Decimal, Decimal, Decimal)> = daily
        .iter()
        .map(|(&d, &p)| {
            (
                Decimal::from((d - g).num_days()).log10(),
                p.log10(),
                Decimal::ONE,
            )
        })
        .collect();
    if use_btc_anchors {
        for (ds, ps) in CALIBRATION_ANCHORS {
            let d: NaiveDate = ds.parse().expect("valid compiled-in anchor date");
            if d >= earliest {
                continue;
            }
            let p: Decimal = ps.parse().expect("valid compiled-in anchor price");
            samples.push((
                Decimal::from((d - g).num_days()).log10(),
                p.log10(),
                anchor_weight(),
            ));
        }
    }
    let (spine_a, spine_b) = weighted_ols(&samples)?;

    let halvings = projected_halving_dates();

    // Residuals + per-cycle phase bins.
    let mut bin_sums: BTreeMap<usize, BTreeMap<usize, (Decimal, u64)>> = BTreeMap::new();
    for (&d, &p) in daily {
        let Some((cycle_idx, phase)) = phase_of(d, &halvings) else {
            continue;
        };
        let days = Decimal::from((d - g).num_days());
        let r = p.log10() - (spine_a + spine_b * days.log10());
        let bin: usize = (phase * Decimal::from(NBINS))
            .floor()
            .try_into()
            .unwrap_or(0);
        let e = bin_sums
            .entry(cycle_idx)
            .or_default()
            .entry(bin.min(NBINS - 1))
            .or_insert((Decimal::ZERO, 0));
        e.0 += r;
        e.1 += 1;
    }

    let mut curves = BTreeMap::new();
    let mut amps = BTreeMap::new();
    for (cycle_idx, bins) in bin_sums {
        let n: u64 = bins.values().map(|&(_, c)| c).sum();
        if (n as usize) < MIN_CYCLE_OBS {
            continue;
        }
        let curve: BTreeMap<usize, Decimal> = bins
            .into_iter()
            .map(|(b, (s, c))| (b, s / Decimal::from(c)))
            .collect();
        let amp = curve.values().copied().max().unwrap_or(Decimal::ZERO);
        curves.insert(cycle_idx, curve);
        amps.insert(cycle_idx, amp);
    }

    // ρ = 0.5^(1/half_life).
    let rho = dec!(0.5).powd(Decimal::ONE / Decimal::from(HALF_LIFE_DAYS));

    Some(FittedModel {
        spine_a,
        spine_b,
        curves,
        amps,
        halvings,
        rho,
    })
}

// ── Band offsets (blended empirical horizon quantiles) ─────────────────────────

/// Per-horizon P10/P90 offsets (log10, relative to the centre path).
struct BandGrid {
    /// `(horizon_days, lo_offset, hi_offset)`, ascending in horizon.
    grid: Vec<(i64, Decimal, Decimal)>,
}

impl BandGrid {
    /// Linear interpolation of the offsets at horizon `k`; below the first grid point
    /// the offsets scale linearly from zero at `k = 0`.
    fn offsets_at(&self, k: i64) -> (Decimal, Decimal) {
        if self.grid.is_empty() {
            return (Decimal::ZERO, Decimal::ZERO);
        }
        let (k0, lo0, hi0) = self.grid[0];
        if k <= k0 {
            let t = Decimal::from(k) / Decimal::from(k0);
            return (lo0 * t, hi0 * t);
        }
        for w in self.grid.windows(2) {
            let (ka, loa, hia) = w[0];
            let (kb, lob, hib) = w[1];
            if k <= kb {
                let t = Decimal::from(k - ka) / Decimal::from(kb - ka);
                return (loa + t * (lob - loa), hia + t * (hib - hia));
            }
        }
        let &(_, lo, hi) = self.grid.last().expect("non-empty checked above");
        (lo, hi)
    }
}

/// Build the band grid: at each horizon `k` the per-side offset is the midpoint of
/// (a) the model's own in-sample horizon-`k` error quantiles and (b) the raw
/// residual-change quantiles — (a) alone is optimistic, (b) alone far too wide;
/// the blend backtests closest to nominal coverage (research log §7.4).
fn build_band_grid(model: &FittedModel, daily: &BTreeMap<NaiveDate, Decimal>) -> BandGrid {
    let g = genesis();
    // Precompute residual and cycle component per stored day.
    let mut r: BTreeMap<NaiveDate, Decimal> = BTreeMap::new();
    let mut c: BTreeMap<NaiveDate, Decimal> = BTreeMap::new();
    for (&d, &p) in daily {
        let days = Decimal::from((d - g).num_days());
        r.insert(
            d,
            p.log10() - (model.spine_a + model.spine_b * days.log10()),
        );
        c.insert(d, model.cycle_component(d));
    }

    let q10 = dec!(0.10);
    let q50 = dec!(0.50);
    let q90 = dec!(0.90);

    let mut grid = Vec::new();
    let mut ks: Vec<i64> = (1..=(CYCLE_DAYS / BAND_GRID_STEP))
        .map(|i| i * BAND_GRID_STEP)
        .collect();
    if *ks.last().unwrap_or(&0) != CYCLE_DAYS {
        ks.push(CYCLE_DAYS);
    }
    for k in ks {
        let rho_k = model.rho.powi(k);
        let kd = chrono::Duration::days(k);
        let mut model_err = Vec::new();
        let mut resid_change = Vec::new();
        for (&d, &rd) in &r {
            let d2 = d + kd;
            let Some(&rd2) = r.get(&d2) else { continue };
            let pred = c[&d2] + (rd - c[&d]) * rho_k;
            model_err.push(rd2 - pred);
            resid_change.push(rd2 - rd);
        }
        if model_err.len() < MIN_BAND_SAMPLES {
            continue;
        }
        model_err.sort_unstable();
        resid_change.sort_unstable();
        let me = (
            quantile(&model_err, q10),
            quantile(&model_err, q50),
            quantile(&model_err, q90),
        );
        let rc = (
            quantile(&resid_change, q10),
            quantile(&resid_change, q50),
            quantile(&resid_change, q90),
        );
        let two = dec!(2);
        let lo = ((me.0 - me.1) + (rc.0 - rc.1)) / two;
        let hi = ((me.2 - me.1) + (rc.2 - rc.1)) / two;
        grid.push((k, lo, hi));
    }
    BandGrid { grid }
}

// ── Public entry point ─────────────────────────────────────────────────────────

/// Project the composite model `CYCLE_DAYS` forward from the latest real daily close.
///
/// Same contract as the superseded `project_cycle_repeat`: `daily` is the dense
/// `(date, close)` series `compute_overlay` was built from; `real_points` is that
/// function's output (used to fold real anchors/lows into projected cycles per
/// REQ-CYCLE-061). Returns zero points (not an error) when the stored span is shorter
/// than `CYCLE_DAYS` (REQ-CYCLE-062 preserved verbatim).
///
/// `use_btc_anchors` must be `true` only for `bitcoin`/`usd` — the compiled-in
/// calibration anchors are BTC/USD closes.
///
// @MX:ANCHOR: [AUTO] project_composite — forward-projection boundary consumed by the
//             recompute driver; replaces project_cycle_repeat (SPEC-CYCLE-001 v0.4.0).
//             The read route's ordering/cursor contract requires projected points to
//             carry cycle assignments from the extended halving list, as before.
// @MX:REASON: The centre path MUST remain continuous at the join (g₀ decay term — the
//             v0.2.0 discontinuity bug must not return) and `price` MUST be the P50
//             path with `price_low`/`price_high` the P10/P90 bands, never swapped.
//             Decimal throughout — never f64 (REQ-PROV-012). Model constants are
//             backtest-selected; do not tune them without re-running the walk-forward
//             backtest (tests/backtest_projection.rs + docs/prediction-research.md).
// @MX:SPEC: SPEC-CYCLE-001 REQ-CYCLE-060 REQ-CYCLE-061 REQ-CYCLE-062 REQ-CYCLE-063 REQ-CYCLE-064
pub fn project_composite(
    daily: &[(NaiveDate, Decimal)],
    real_points: &[OverlayPoint],
    use_btc_anchors: bool,
) -> Vec<OverlayPoint> {
    if daily.is_empty() {
        return vec![];
    }
    let series: BTreeMap<NaiveDate, Decimal> = daily.iter().copied().collect();
    let today = *series.keys().next_back().expect("checked non-empty above");
    let earliest = *series.keys().next().expect("checked non-empty above");
    let current_price = series[&today];

    // REQ-CYCLE-062: fewer than CYCLE_DAYS days of history → zero points, not an error.
    if (today - earliest).num_days() < CYCLE_DAYS {
        return vec![];
    }

    let Some(model) = fit_model(&series, use_btc_anchors) else {
        return vec![];
    };
    let bands = build_band_grid(&model, &series);

    // Continuity: g₀ anchors the path at today's real price.
    let g = genesis();
    let r_today = current_price.log10()
        - (model.spine_a + model.spine_b * Decimal::from((today - g).num_days()).log10());
    let g0 = r_today - model.cycle_component(today);

    struct Raw {
        ts: NaiveDate,
        p50: Decimal,
        p10: Decimal,
        p90: Decimal,
        cycle_number: i32,
        halving_date: NaiveDate,
        days_since_halving: i64,
    }

    let mut rho_k = Decimal::ONE;
    let mut raw: Vec<Raw> = Vec::with_capacity(CYCLE_DAYS as usize);
    for k in 1..=CYCLE_DAYS {
        rho_k *= model.rho;
        let ts = today + chrono::Duration::days(k);
        let center = model.spine(ts) + model.cycle_component(ts) + g0 * rho_k;
        let (lo_off, hi_off) = bands.offsets_at(k);
        let a = assign_cycle_in(ts, &model.halvings);
        raw.push(Raw {
            ts,
            p50: pow10(center),
            p10: pow10(center + lo_off),
            p90: pow10(center + hi_off),
            cycle_number: a.cycle_number,
            halving_date: a.halving_date,
            days_since_halving: a.days_since_halving,
        });
    }

    // Per-cycle normalization: fold real anchors/lows exactly as the previous
    // projection did (REQ-CYCLE-061/063 unchanged).
    let mut by_cycle: BTreeMap<i32, Vec<&Raw>> = BTreeMap::new();
    for p in &raw {
        by_cycle.entry(p.cycle_number).or_default().push(p);
    }

    let mut result = Vec::with_capacity(raw.len());
    for (cycle_number, points) in &by_cycle {
        let real_cycle_point = real_points.iter().find(|p| p.cycle_number == *cycle_number);
        let cycle_anchor_price = match real_cycle_point {
            Some(rp) => rp.price / rp.norm_halving,
            None => {
                points
                    .iter()
                    .min_by_key(|p| p.days_since_halving)
                    .expect("cycle group is non-empty by construction")
                    .p50
            }
        };
        let projected_low = points
            .iter()
            .map(|p| p.p50)
            .min()
            .expect("cycle group is non-empty by construction");
        let real_low = real_points
            .iter()
            .filter(|p| p.cycle_number == *cycle_number)
            .map(|p| p.price)
            .min();
        let cycle_low = match real_low {
            Some(rl) => rl.min(projected_low),
            None => projected_low,
        };

        for p in points {
            result.push(OverlayPoint {
                cycle_number: *cycle_number,
                halving_date: p.halving_date,
                days_since_halving: p.days_since_halving,
                ts: p.ts,
                price: p.p50,
                norm_halving: p.p50 / cycle_anchor_price,
                norm_cycle_low: p.p50 / cycle_low,
                halving_baseline_approximate: true,
                projected: true,
                price_low: Some(p.p10),
                price_high: Some(p.p90),
            });
        }
    }

    result
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collectors::cycle_overlay::compute_overlay;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    /// Dense synthetic series of `days` consecutive dates from `start`.
    fn synthetic(
        start: NaiveDate,
        days: i64,
        f: impl Fn(i64) -> Decimal,
    ) -> Vec<(NaiveDate, Decimal)> {
        (0..days)
            .map(|i| (start + chrono::Duration::days(i), f(i)))
            .collect()
    }

    // REQ-CYCLE-062: insufficient history → zero points, not an error.
    #[test]
    fn insufficient_history_yields_zero_points() {
        let daily = synthetic(d(2024, 1, 1), CYCLE_DAYS - 1, |i| {
            dec!(100) + Decimal::from(i)
        });
        assert!(project_composite(&daily, &[], false).is_empty());
        assert!(project_composite(&[], &[], false).is_empty());
    }

    // Continuity at the join (the v0.2.0 discontinuity bug must not return): the first
    // projected day must be within a small multiplicative step of today's real price.
    #[test]
    fn first_projected_point_is_continuous_with_current_price() {
        let daily = synthetic(d(2020, 1, 1), CYCLE_DAYS + 10, |i| {
            dec!(20000) + Decimal::from(i) * dec!(10)
        });
        let current = daily.last().unwrap().1;
        let projected = project_composite(&daily, &[], false);
        assert_eq!(projected.len() as i64, CYCLE_DAYS);
        let first = &projected[0];
        assert!(
            first.price > current * dec!(0.9) && first.price < current * dec!(1.1),
            "first projected point must stay close to current price, got {} vs {}",
            first.price,
            current
        );
    }

    // Bands: present on every projected point, ordered P10 <= P50 <= P90, and widening
    // with horizon.
    #[test]
    fn bands_are_ordered_and_widen_with_horizon() {
        let daily = synthetic(d(2019, 1, 1), CYCLE_DAYS + 400, |i| {
            // Gentle noise-free growth: bands come from the model's own error history.
            dec!(5000) + Decimal::from(i) * dec!(12)
        });
        let projected = project_composite(&daily, &[], false);
        assert_eq!(projected.len() as i64, CYCLE_DAYS);
        for p in &projected {
            let lo = p.price_low.expect("projected points carry P10");
            let hi = p.price_high.expect("projected points carry P90");
            assert!(lo <= p.price && p.price <= hi, "band ordering at {}", p.ts);
        }
        let w = |p: &OverlayPoint| p.price_high.unwrap() / p.price_low.unwrap();
        let early = w(&projected[29]);
        let late = w(&projected[(CYCLE_DAYS - 1) as usize]);
        assert!(
            late >= early,
            "band width must not shrink with horizon: {early} -> {late}"
        );
    }

    // Cycle assignment: projected points crossing the estimated 2028-04-20 halving get
    // cycle 5 (REQ-CYCLE-063 semantics preserved from the replay implementation).
    #[test]
    fn projected_points_cross_estimated_halving_into_cycle_5() {
        let today = d(2027, 6, 1);
        let start = today - chrono::Duration::days(CYCLE_DAYS + 10);
        let daily = synthetic(start, CYCLE_DAYS + 11, |i| dec!(1000) + Decimal::from(i));
        let projected = project_composite(&daily, &[], false);
        let est = d(2028, 4, 20);
        assert!(projected.iter().any(|p| p.ts < est && p.cycle_number == 4));
        assert!(projected.iter().any(|p| p.ts >= est && p.cycle_number == 5));
    }

    // Norm folding: a cycle with real points reuses the real halving anchor, so a
    // projected point's norm_halving is its p50 over the real anchor price.
    #[test]
    fn projected_norms_fold_real_cycle_anchor() {
        let start = d(2021, 1, 1);
        let daily = synthetic(start, CYCLE_DAYS + 600, |i| {
            dec!(30000) + Decimal::from(i) * dec!(5)
        });
        let real = compute_overlay(daily.clone());
        let projected = project_composite(&daily, &real, false);
        assert!(!projected.is_empty());
        // Find a projected point sharing a cycle with real points.
        let shared = projected
            .iter()
            .find(|p| real.iter().any(|r| r.cycle_number == p.cycle_number));
        if let Some(p) = shared {
            let rp = real
                .iter()
                .find(|r| r.cycle_number == p.cycle_number)
                .unwrap();
            let anchor = rp.price / rp.norm_halving;
            assert_eq!(p.norm_halving, p.price / anchor);
        }
        assert!(projected.iter().all(|p| p.projected));
        assert!(projected.iter().all(|p| p.halving_baseline_approximate));
    }

    // Decimal-typed invariant (REQ-CYCLE-024): bands and price are Decimal.
    #[test]
    fn projected_fields_are_decimal_typed() {
        let daily = synthetic(d(2020, 1, 1), CYCLE_DAYS + 5, |i| {
            dec!(20000) + Decimal::from(i) * dec!(5)
        });
        let projected = project_composite(&daily, &[], false);
        assert!(!projected.is_empty());
        let _p: Decimal = projected[0].price;
        let _lo: Decimal = projected[0].price_low.unwrap();
        let _hi: Decimal = projected[0].price_high.unwrap();
    }

    // Helper sanity: pow10/quantile/weighted_ols behave as expected.
    #[test]
    fn numeric_helpers_are_sane() {
        // pow10(log10(x)) ≈ x
        let x = dec!(12345.678);
        let round = pow10(x.log10());
        assert!(
            (round - x).abs() / x < dec!(0.0001),
            "pow10/log10 round-trip: {round}"
        );

        let sorted = vec![dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)];
        assert_eq!(quantile(&sorted, dec!(0.5)), dec!(3));
        assert_eq!(quantile(&sorted, dec!(0)), dec!(1));
        assert_eq!(quantile(&sorted, dec!(1)), dec!(5));

        // y = 2 + 3x exactly recovered.
        let pts: Vec<(Decimal, Decimal, Decimal)> = (0..10)
            .map(|i| {
                let x = Decimal::from(i);
                (x, dec!(2) + dec!(3) * x, Decimal::ONE)
            })
            .collect();
        let (a, b) = weighted_ols(&pts).unwrap();
        assert!((a - dec!(2)).abs() < dec!(0.0000001));
        assert!((b - dec!(3)).abs() < dec!(0.0000001));
    }

    // Anchors: with a short 2017+-style history, the anchored spine slope must land
    // near the full-history exponent (~5.5–5.7), not the degraded ~4.5 (research §2).
    #[test]
    fn btc_anchors_recover_full_history_spine_slope() {
        // Synthetic "2017+" history following the true power law with b = 5.6.
        let start = d(2017, 8, 18);
        let g = genesis();
        let daily: Vec<(NaiveDate, Decimal)> = (0..(CYCLE_DAYS + 800))
            .map(|i| {
                let date = start + chrono::Duration::days(i);
                let days = Decimal::from((date - g).num_days());
                (date, pow10(dec!(-16.2) + dec!(5.6) * days.log10()))
            })
            .collect();
        let series: BTreeMap<NaiveDate, Decimal> = daily.iter().copied().collect();
        let with_anchors = fit_model(&series, true).unwrap();
        // The synthetic series follows b = 5.6 exactly; anchors are real (noisy) history,
        // so the anchored fit should stay in the 5.0–6.0 neighbourhood.
        assert!(
            with_anchors.spine_b > dec!(5.0) && with_anchors.spine_b < dec!(6.0),
            "anchored spine slope {} out of expected range",
            with_anchors.spine_b
        );
    }
}
