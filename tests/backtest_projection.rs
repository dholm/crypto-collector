//! Walk-forward backtest of the composite cycle projection (SPEC-CYCLE-001 v0.4.0).
//!
//! Replays history from `tests/fixtures/btc_daily_close.csv` (BTC/USD daily closes,
//! 2011-08-18 → 2026-07-07; Bitstamp public API pre-2017 merged with the collector's own
//! `coin_candles` 1d closes — see docs/prediction-research.md §2) and verifies that the
//! composite model beats the superseded v0.3.0 "cycle-repeat replay" out-of-sample.
//!
//! No DB, no network — deterministic on the committed fixture. Methodology and full
//! results: docs/prediction-research.md §6–7.

use chrono::NaiveDate;
use crypto_collector::collectors::cycle_overlay::CYCLE_DAYS;
use crypto_collector::collectors::cycle_projection::project_composite;
use rust_decimal::Decimal;
use std::collections::BTreeMap;

fn load_fixture() -> Vec<(NaiveDate, Decimal)> {
    let csv = include_str!("fixtures/btc_daily_close.csv");
    csv.lines()
        .skip(1) // header
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let (d, p) = l.split_once(',').expect("date,close");
            (
                d.parse().expect("valid fixture date"),
                p.parse().expect("valid fixture close"),
            )
        })
        .collect()
}

/// The superseded v0.3.0 replay, ported verbatim as the frozen baseline:
/// `projected[today + k] = current * P[today - 1458 + k] / P[today - 1458]` with
/// last-observation-carried-forward across gaps.
fn replay_baseline(daily: &[(NaiveDate, Decimal)]) -> Option<BTreeMap<i64, Decimal>> {
    let series: BTreeMap<NaiveDate, Decimal> = daily.iter().copied().collect();
    let today = *series.keys().next_back()?;
    let earliest = *series.keys().next()?;
    let current = series[&today];
    let base_date = today - chrono::Duration::days(CYCLE_DAYS);
    if base_date < earliest {
        return None;
    }
    let locf = |d: NaiveDate| series.range(..=d).next_back().map(|(_, &p)| p);
    let base = locf(base_date)?;
    let mut out = BTreeMap::new();
    for k in 1..=CYCLE_DAYS {
        if let Some(rp) = locf(base_date + chrono::Duration::days(k)) {
            out.insert(k, current * rp / base);
        }
    }
    Some(out)
}

/// log10 via f64 — test-metric arithmetic only (production code stays Decimal).
fn log10(d: Decimal) -> f64 {
    let f: f64 = d.try_into().expect("fixture price fits f64");
    f.log10()
}

/// Nearest fixture date within ±3 days (the fixture has ~30 small gaps).
fn actual_near(series: &BTreeMap<NaiveDate, Decimal>, d: NaiveDate) -> Option<Decimal> {
    for off in [0i64, 1, -1, 2, -2, 3, -3] {
        if let Some(&p) = series.get(&(d + chrono::Duration::days(off))) {
            return Some(p);
        }
    }
    None
}

const HORIZONS: [i64; 4] = [90, 365, 730, 1458];

/// Walk-forward comparison: yearly origins 2016→2025, fits strictly on data ≤ origin.
/// Asserts the composite model's mean log10 RMSE beats the replay by a wide margin
/// (the Python prototype measured 0.20 vs 0.39; the 0.85 factor here is slack, not
/// the expectation) and that its P10–P90 bands cover most realised outcomes.
#[test]
fn composite_beats_replay_walk_forward() {
    let full = load_fixture();
    let series: BTreeMap<NaiveDate, Decimal> = full.iter().copied().collect();

    let origins: Vec<NaiveDate> = (2016..=2025)
        .map(|y| {
            let mut d = NaiveDate::from_ymd_opt(y, 1, 1).unwrap();
            while !series.contains_key(&d) {
                d -= chrono::Duration::days(1);
            }
            d
        })
        .collect();

    let mut sq_err: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
    let mut band_total = 0u32;
    let mut band_inside = 0u32;

    for &t0 in &origins {
        let train: Vec<(NaiveDate, Decimal)> =
            full.iter().copied().filter(|&(d, _)| d <= t0).collect();

        let replay = replay_baseline(&train).expect("2016+ origins have full replay history");
        let composite = project_composite(&train, &[], true);
        assert!(
            !composite.is_empty(),
            "composite must project from origin {t0}"
        );
        let comp_by_k: BTreeMap<i64, &_> = composite
            .iter()
            .map(|p| ((p.ts - t0).num_days(), p))
            .collect();

        for k in HORIZONS {
            let target = t0 + chrono::Duration::days(k);
            let Some(actual) = actual_near(&series, target) else {
                continue;
            };
            let actual_log = log10(actual);
            if let Some(rp) = replay.get(&k) {
                sq_err
                    .entry("replay")
                    .or_default()
                    .push((log10(*rp) - actual_log).powi(2));
            }
            if let Some(cp) = comp_by_k.get(&k) {
                sq_err
                    .entry("composite")
                    .or_default()
                    .push((log10(cp.price) - actual_log).powi(2));
                let lo = cp.price_low.expect("projected points carry P10");
                let hi = cp.price_high.expect("projected points carry P90");
                band_total += 1;
                if actual >= lo && actual <= hi {
                    band_inside += 1;
                }
            }
        }
    }

    let rmse = |name: &str| -> f64 {
        let v = &sq_err[name];
        (v.iter().sum::<f64>() / v.len() as f64).sqrt()
    };
    let replay_rmse = rmse("replay");
    let composite_rmse = rmse("composite");
    let coverage = f64::from(band_inside) / f64::from(band_total);

    println!(
        "walk-forward log10 RMSE: replay {replay_rmse:.4} vs composite {composite_rmse:.4}; \
         P10-P90 coverage {coverage:.2} ({band_inside}/{band_total})"
    );

    assert!(
        composite_rmse < replay_rmse * 0.85,
        "composite ({composite_rmse:.4}) must beat replay ({replay_rmse:.4}) by a clear margin"
    );
    assert!(
        composite_rmse < 0.32,
        "composite log10 RMSE ({composite_rmse:.4}) above sanity bound"
    );
    assert!(
        coverage >= 0.60,
        "P10-P90 coverage {coverage:.2} below sanity bound (target 0.80)"
    );
}

/// The projection must stay continuous at the join on real data (regression guard for
/// the v0.2.0 discontinuity bug) and be sane in magnitude: within the first projected
/// month the P50 path must stay within ±25% of the last real close.
#[test]
fn composite_join_is_continuous_on_real_data() {
    let full = load_fixture();
    let current = full.last().unwrap().1;
    let projected = project_composite(&full, &[], true);
    assert_eq!(projected.len() as i64, CYCLE_DAYS);
    let first_month = &projected[..30];
    for p in first_month {
        let ratio = p.price / current;
        assert!(
            ratio > Decimal::from(3) / Decimal::from(4)
                && ratio < Decimal::from(5) / Decimal::from(4),
            "projected {} at {} deviates from current {} (ratio {ratio})",
            p.price,
            p.ts,
            current
        );
    }
}
