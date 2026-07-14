# Bitcoin Halving-Cycle Prediction — Research Log

Living research log for the redesign of the cycle-overlay projection algorithm, now served at
`GET /v1/coins/{coin_id}/cycle-projection/composite` (composite model) and
`GET /v1/coins/{coin_id}/cycle-projection/replay` (replay baseline) per SPEC-CYCLE-001. Maintained continuously during the
work; sections are appended/revised as evidence accumulates.

Status: **implemented** (SPEC-CYCLE-001 v0.4.0) — model in
`src/collectors/cycle_projection.rs`, regression backtest in
`tests/backtest_projection.rs`.

---

## 1. The current algorithm (v0.3.0, "Bitbo cycle-repeat replay")

Implemented in `src/collectors/cycle_overlay.rs` (`project_cycle_repeat`).

### 1.1 Mechanics

For `k = 1..=1458`:

```text
projected_price[today + k] = current_price * P[today − 1458 + k] / P[today − 1458]
```

i.e. the *actual* daily closes of the trailing 1458-day window are replayed forward from
today, rescaled so the path is continuous at `today`. Projected points are flagged
`projected = true`, assigned cycles via an extended halving list containing one estimated
future halving (2028-04-20).

### 1.2 Assumptions, heuristics and constants

| Item | Value | Nature |
|---|---|---|
| Halving dates | 2012-11-28, 2016-07-09, 2020-05-11, 2024-04-20 | Historical fact (block-derived) |
| `CYCLE_DAYS` | 1458 | **Heuristic constant** — actual halving intervals were 1319, 1402, 1440 days |
| Reference window | trailing 1458 days, single window | **Assumption**: the next 4 years repeat the last 4 years exactly |
| Scaling | ratio replay anchored at today's price | Guarantees continuity at the join |
| Uncertainty | none | Single deterministic path presented with no error model |

### 1.3 Weaknesses identified

1. **Single-sample extrapolation.** The projection is an exact replay of one window (n = 1).
   Every idiosyncrasy of the last cycle — flash crashes, exchange outages, one-off macro
   events — is reproduced verbatim at a 1–4 year horizon. Daily wiggles four years out are
   spurious precision.
2. **No diminishing returns.** Cycle peak multiples measured from the halving-day price
   decay ~92.6× → 29.5× → 8.5× (see §3). A pure replay of the previous cycle's multiple
   systematically over-forecasts the next cycle's amplitude. (The replay's use of the
   *trailing* window partially bakes recent damping in, but only by accident of window
   position, not by modelling.)
3. **Wrong cycle length.** 1458 days matches no historical halving interval (1319/1402/1440)
   and phase-misaligns the replay against the real cycle structure.
4. **No uncertainty model.** Consumers cannot distinguish a confident near-term projection
   from a speculative 4-year point. There are no bands, no quantiles, no calibration.
5. **Phase ignorance.** The replay does not know where in the halving cycle "today" is. A
   projection started at a cycle top replays the *post-halving accumulation* pattern from
   the reference window into what is historically a drawdown phase, and vice versa.
6. **No trend anchor.** Nothing ties the projection to any long-run growth model; the
   replay can wander arbitrarily far from any historically-supported envelope.

### 1.4 What the current algorithm gets right (to preserve)

- **Continuity at the join** — projection starts exactly at today's real price. (The v0.2.0
  predecessor got this wrong; REQ-CYCLE-060's history documents the discontinuity bug.)
- **Determinism, explainability** — pure function of stored candles; no RNG, no black box.
- **Clean separation of observed vs projected** (`projected = true` flag) and idempotent
  full-rebuild materialisation.
- `Decimal` end-to-end (REQ-PROV-012/REQ-CYCLE-024).

---

## 2. Data constraints (production)

The production `coin_candles` history for `bitcoin/usd` begins **2017-08-17** (Binance-era
backfill). Consequences:

- Cycle 1 (2012 halving) is entirely unobservable in production; cycle 2 is observable only
  from day ~404 onward.
- A power-law fit over 2017+ data alone is badly biased (measured: exponent 4.48 vs 5.60
  on full history — the early-era leverage points are essential).
- **Mitigation adopted**: compile in a small set (~25) of quarterly historical closes
  2011-08 → 2017-08 (public, immutable historical facts, same status as the halving-date
  constants) used *only* as regression calibration anchors, each weighted by the ~91 days
  it represents. Measured result: anchored fit recovers the full-history fit almost exactly
  (b = 5.554 vs 5.603 true; 2017-only fit degrades to 4.481). Anchors are never emitted as
  data points.

For **backtesting** we assembled a full daily-close series 2011-08-18 → present from
Bitstamp public API (pre-2017) merged with the production DB export (2017+): 5 404 daily
closes, 30 small gaps (checked into `tests/fixtures/btc_daily_close.csv`).

**Update (2026-07-07):** the production gap is now closed at the source. A deep-history
daily backfill (SPEC-SCHED-001 v1.2.0 / SPEC-PROV-001 v1.2.0) pulls native `1d` BTC/USD
candles from Bitstamp back to 2011-08-18 into `coin_candles`. Once that job completes in
production, the power-law spine fits directly on real pre-2017 daily data and the
compiled-in `CALIBRATION_ANCHORS` become a redundant safety net (they remain as a
deterministic fallback for environments where the deep backfill has not run or is
disabled). The anchors and the backfilled data are the *same* Bitstamp daily closes, so
the two agree by construction.

---

## 3. Empirical cycle statistics (measured from the assembled series)

Halving-anchored, close-price basis, half-open cycle windows `[halving, next_halving)`:

| Cycle | Halving | Anchor close | Peak (close) | Peak day | Peak multiple | Post-peak trough | Drawdown |
|---|---|---|---|---|---|---|---|
| 1 | 2012-11-28 | $12.22 | $1 132 (2013-12-04) | 371 | **92.6×** | $171 @ day 777 | −85% |
| 2 | 2016-07-09 | $647.78 | $19 103 (2017-12-16) | 525 | **29.5×** | $3 212 @ day 889 | −83% |
| 3 | 2020-05-11 | $8 561.52 | $73 072 (2024-03-13)¹ | 1402¹ | 8.5× | — | −16%¹ |
| 4 | 2024-04-20 | $64 940.59 | $124 659 (2025-10-06)² | 534 | 1.9ײ | $58 625 @ day 801² | −53%² |

¹ Cycle 3's *max close* falls on the pre-halving run-up of early 2024 that exceeded the
Nov-2021 cyclical top ($69k, day ~548). Peak-*timing* statistics therefore use the
cyclical top (day ~548), not the window max — an explicit outlier-handling rule.
² Provisional; cycle 4 is in progress.

Key regularities:

- **Log peak multiples decay linearly**: ln(92.6)=4.53, ln(29.5)=3.38, ln(8.5)=2.14 —
  differences −1.15, −1.24 per cycle. Extremely regular diminishing returns.
- **Cycle lengths grow**: 1319 → 1402 → 1440 days. (The current code's 1458 is wrong for
  every historical cycle; the estimated next interval, 2024-04-20 → 2028-04-20, is 1461.)
- **Cyclical peak timing clusters** at days 371 / 525 / ~548 / 534 post-halving.
- **Bear legs**: ~360–410 days top→trough; drawdowns −85% → −83% → (−77% in the
  literature's cycle partitioning) — shrinking roughly 5–7 pp per cycle.

### 3.1 Power-law fit (log10 price vs log10 days-since-genesis, genesis 2009-01-03)

| Fit window | a | b | Residual σ (log10) |
|---|---|---|---|
| ≤ 2016-07 | −15.994 | 5.531 | 0.362 |
| ≤ 2020-05 | −16.541 | 5.701 | 0.320 |
| ≤ 2024-04 | −16.367 | 5.650 | 0.300 |
| full | −16.211 | 5.603 | 0.281 |

The exponent is stable across walk-forward cutoffs (5.53–5.70) — consistent with the
literature (Burger 2019: b = 5.845 on 2010+ data; Santostasi: 5.8–5.9). Fit stability is
the property that makes the spine usable out-of-sample.

### 3.2 Detrended structure (residual r = log10 price − power-law fit)

- **The floor is stable**: every cycle's minimum residual sits at −0.35…−0.43 — the
  "power-law support corridor" of the literature, reproduced independently in our data.
- **Peak residuals decay linearly**: +1.04, +0.80, +0.56, (+0.14 provisional) ≈ −0.24
  log10/cycle. Diminishing returns persist *after* detrending, so the cyclical component
  needs explicit amplitude damping.
- **Phase shape is consistent across cycles**: residual rises through phase 0.2–0.45 of the
  cycle (peak), declines through 0.5–1.0 (bear + accumulation). Phase = days-since-halving
  ÷ cycle length.
- **Mean reversion**: AR(1) fit of the daily residual series gives ρ ≈ 0.9973/day,
  half-life ≈ 255 days — today's deviation from trend decays toward the cyclical norm over
  ~8 months.

---

## 4. Literature review (summary)

Full agent research reports are summarised here; sources at the bottom.

### 4.1 Power-law models — adopt

Burger's "power-law corridor" (2019): `log10 P = −17.016 + 5.845·log10(days since genesis)`,
R² 0.931; support line = parallel offset under historical lows; tops line fit through cycle
peaks has a *shallower* slope (5.03) — the corridor narrows, an independent statement of
diminishing returns. Out-of-sample record since 2019 is good ($100k crossing predicted for
2021–2028 window; support unbroken including the 2022 bottom). Santostasi's "Bitcoin Power
Law Theory" (b ≈ 5.8–5.9, R² ≈ 0.95) adds a generative story (adoption t³ × Metcalfe ²).
Quantile-regression variants produce tighter, better-calibrated bands than OLS ± z·σ.

Criticisms acknowledged: non-stationary regressand (R² inflated; classical CIs invalid —
bands must be treated as *descriptive quantiles*), only 3–4 effective cycles, parameter
drift 5.5 → 5.9. Mitigation: walk-forward validation (§7), quantile bands from empirical
residuals rather than Gaussian theory.

### 4.2 Diminishing returns / LGC — adopt the regularity, not the hand-drawn curves

Halving-day peak multiples 94→30→8→~2 (log-linear decay ~−0.5 log10/cycle) are documented
independently (hcburger.com/diminishingreturns, NYDIG, CoinDesk). Dave-the-Wave's LGC
channels called the 2021 top / 2022 bottom zones but are hand-tuned and non-reproducible —
rejected as a component, but the phenomenon it draws is the same detrended amplitude decay
we measured (§3.2).

### 4.3 Lengthening cycles — do not extrapolate timing

Halving→top: 367 → 526 → ~548, but cycle 4 peaked *earlier* (~534 days, and CoinGecko
reports ~68 days earlier than the prior cycle in its partitioning). Two data points do not
establish a trend; use the pooled peak window (~370–550 days) as a wide prior only.

### 4.4 Stock-to-Flow — reject

Invalidated post-2021 (price exited the model's own bands; S2FX's $288k epoch average vs
$69k actual peak). Econometrically unsound: non-cointegrated spurious regression
(M. Burger, Kripfganz, Emblow), and for Bitcoin S2F is a deterministic step function of
time — any signal it carries is already in a time-based power law + halving-phase term.

### 4.5 On-chain metrics — infeasible here except by proxy

Realized Price and MVRV-Z require the full UTXO set with last-moved prices — cannot be
computed from OHLCV; out of scope (would require a new data provider, violating the
no-new-upstream constraint). Puell Multiple is approximable (issuance is deterministic:
subsidy × ~144 blocks/day × close vs its 365d MA) and the Mayer Multiple (close/200d MA)
is free — both noted as future work; neither is needed for the core projection. The
**power-law deviation oscillator is the OHLCV-native substitute for MVRV** as a
cycle-position signal, and it is exactly the residual `r` our model is built on.

### 4.6 Probabilistic forecasting — deterministic empirical quantiles

Monte Carlo around a deterministic trend converges to quantiles computable in closed form —
rejected (RNG, non-reproducible, adds nothing). Gaussian ± z·σ bands understate BTC's heavy
tails. Adopted: **empirical quantiles of horizon-k residual changes** (deterministic,
calibrated by construction against history) conditioned on the model's own projection.

### 4.7 Small-sample cycle weighting

With n = 3–4 cycles, per-cycle free parameters are hopelessly overfit. Literature-backed
translation: exponential cycle weights (recent cycle ~2–4× oldest), shrink per-cycle
features toward the pooled decay line rather than trusting the last cycle, keep the
cyclical model to ≤ 2 free parameters beyond the shared trend.

---

## 5. Improved model design

**Decomposition** (all in log10 space, all `Decimal` via `MathematicalOps`):

```text
log10 P̂(today + k) = S(today + k)              power-law spine
                    + C(phase(today + k))       damped cycle-phase component
                    + D(k)                      continuity/mean-reversion term
band_q(today + k)  = P̂ · 10^(Q_q(k))           empirical horizon quantiles
```

1. **Spine `S`**: weighted OLS of log10(close) on log10(days since 2009-01-03) over all
   available daily closes, augmented with the compiled-in quarterly calibration anchors
   (§2) weighted by days-represented. Two parameters, refit on every recompute.

2. **Cycle-phase component `C`**: for each historical cycle with data, compute the mean
   detrended residual in phase bins (phase = days_since_halving / cycle_length; the
   in-progress cycle uses the estimated next halving). Combine bins across cycles with
   exponential cycle weights (recent ×2 per cycle) **and per-cycle amplitude
   damping**: cycle c's residual curve is rescaled by `A(target)/A(c)` where `A` is the
   linear extrapolation of the per-cycle peak-residual decay (§3.2). Interpolate bins
   piecewise-linearly for a smooth curve.

3. **Continuity term `D`**: at k = 0 the model must equal today's actual price. The gap
   `g = r(today) − C(phase(today))` decays as `D(k) = g · ρ^k` with ρ estimated from the
   AR(1) fit of the residual series (measured half-life ≈ 255 days). This preserves the
   current algorithm's join-continuity property while pulling the path toward the
   cycle-phase norm instead of replaying noise.

4. **Bands `Q_q`**: for a grid of horizons k ∈ {30, 60, …, 1440}, compute the empirical
   P10/P50/P90 of `r(d + k) − r(d)` over all historical days d with data at both ends;
   interpolate between grid points (and scale ∝ k below the first point). Bands are
   centred by subtracting the median so P50 stays on the model path, then anchored
   multiplicatively. Deterministic, heavy-tail-aware, horizon-widening by construction.

Properties: deterministic ✓; explainable (four named, separately-inspectable terms) ✓;
diminishing returns (damped `C` + sublinear spine) ✓; cycle-length variation (phase
normalisation) ✓; adaptive cycle weighting ✓; outlier handling (cycle-1 down-weighting via
exponential weights; window-max vs cyclical-top rule §3) ✓; uncertainty (P10/P90) ✓;
graceful degradation (with few cycles of data, `C` shrinks toward 0 and the model tends to
the spine + mean reversion) ✓.

### 5.1 API surface

Backward compatible: existing fields unchanged; projected points additionally carry
nullable `price_low` (P10) and `price_high` (P90); `price` becomes the P50 path for
projected points (real points: bands NULL). Additive migration + additive DTO fields.

---

## 6. Candidate comparison plan

Walk-forward backtests (fit strictly on data before each origin) over origins spaced
90 days apart from 2016-01-01, horizons up to 1458 days, scoring:

- log10 RMSE / MAE (report `10^MAE` as typical multiplicative error), MAPE for reference
- P10/P90 coverage (target 10%/90%) and pinball loss — band calibration
- peak timing error and peak log-price error per cycle (descriptive only)

Candidates:

| ID | Model |
|---|---|
| `replay` | current v0.3.0 cycle-repeat replay (baseline) |
| `spine` | power-law spine + mean reversion only (no cycle component) |
| `composite` | full model of §5 |
| variants | bin count, ρ, damping on/off — ablations |

Results: see §7 (pending).

---

## 7. Validation results

Walk-forward backtest on the assembled 2011–2026 daily series; **43 origins every 90 days
from 2016-01-01**, fits strictly on data ≤ origin, horizons {30, 90, 180, 365, 730, 1095,
1458} days, errors in log10 space (Python prototype; reproduced in Rust in
`tests/backtest_projection.rs`).

### 7.1 Median-path accuracy (log10 RMSE per horizon)

| Horizon | replay (current) | spine + MR | composite (chosen) |
|---|---|---|---|
| 30d | 0.111 | 0.078 | 0.078 |
| 90d | 0.242 | 0.175 | 0.172 |
| 180d | 0.303 | 0.239 | 0.230 |
| 365d | 0.414 | 0.287 | 0.246 |
| 730d | 0.503 | 0.238 | 0.191 |
| 1095d | 0.520 | 0.216 | 0.222 |
| 1458d | 0.666 | 0.288 | 0.266 |
| **mean** | **0.394 (×2.48)** | 0.217 (×1.65) | **0.201 (×1.59)** |

(×N = typical multiplicative error `10^RMSE`.) On origins ≥ 2020 only: replay-era mean for
the composite improves to 0.173 (×1.49) vs spine 0.181 (×1.52).

### 7.2 Ablations

| Variant | mean log10 RMSE | Conclusion |
|---|---|---|
| composite, shrink 1.0 | 0.2061 | shrinkage helps |
| **composite, shrink 0.75** | **0.2006** | chosen |
| composite, shrink 0.5 | 0.2007 | flat optimum 0.5–0.75 |
| no amplitude damping | 0.2298 | damping is essential |
| cycle 1 excluded | 0.2307 | keep all cycles, damp instead of drop |
| exp. cycle weights (w=2) | 0.2099 | equal weights slightly better; kept equal |
| half-life 255d (vs 180d) | 0.2085 | 180d marginally better; matches BTCautoresearch |

### 7.3 Peak prediction from halving-day origins (descriptive; n = 3)

| Cycle | Model | Predicted peak | Actual peak (window max) | Timing err | log10 price err |
|---|---|---|---|---|---|
| 2 | replay | 2017-12-01 $96 613 | 2017-12-16 $19 103 | −15d | **+0.704** |
| 2 | composite | 2017-08-22 $8 489 | | −116d | −0.352 |
| 3 | replay | 2021-12-13 $358 996 | 2024-03-13 $73 072 | −821d | **+0.691** |
| 3 | composite | 2024-04-20 $63 393 | | +38d | **−0.062** |
| 4 | replay | 2028-03-10 $634 205 | 2025-10-06 $124 659 | +886d | **+0.707** |
| 4 | composite | 2028-04-17 $210 127 | | +924d | +0.227 |

The replay over-forecasts every cycle peak by ~×5 (log10 err ≈ +0.70 — exactly the missing
diminishing-returns factor). The composite's peak *price* errors are 2–10× smaller; peak
*timing* remains hard (cycle 4's predicted maximum landed at the window end) and is
documented as a limitation, not a solved problem.

### 7.4 Band calibration (blended estimator; target 10% / 80% / 10%)

| Horizon | below P10 | inside | above P90 |
|---|---|---|---|
| 30d | 7% | 88% | 5% |
| 90d | 7% | 86% | 7% |
| 180d | 15% | 80% | 5% |
| 365d | 3% | 95% | 3% |
| 730d | 0% | 97% | 3% |
| 1095d | 0% | 87% | 13% |
| 1458d | 30% | 52% | 19% |

Band estimator comparison: raw residual-change quantiles were far too wide (100% inside at
365–730d); the model's own in-sample horizon-error quantiles too narrow (44–62% inside at
long horizons — in-sample optimism). The adopted band is the per-side midpoint of the two
offsets: calibration is good through ~3 years and admittedly approximate at the full 4-year
horizon (few effective independent windows exist in 15 years of data — documented
limitation).

### 7.5 Selected model ("composite v1") — final parameters

| Parameter | Value | Provenance |
|---|---|---|
| Spine | OLS log10(close) ~ log10(days since 2009-01-03) | refit each recompute; anchored (§2) |
| Phase bins | 12 per cycle | ablation-flat 8–12 |
| Cycle weights | equal | ablation §7.2 |
| Amplitude damping | linear extrapolation of per-cycle max bin-mean residual, floor 0.05 | §3.2 regularity |
| Cycle-component shrink | 0.75 | ablation §7.2 |
| Mean-reversion half-life | 180 days | ablation + BTCautoresearch |
| Horizon | 1458 days (unchanged) | API stability |
| Bands | midpoint of model-error and residual-change empirical P10/P90, 30-day grid, linear interpolation | §7.4 |

### 7.6 Rust implementation confirmation

The production implementation (`Decimal` end-to-end via `rust_decimal`'s `maths` feature —
no `f64` in any price path, REQ-CYCLE-024) reproduces the prototype's result on the
committed fixture. `tests/backtest_projection.rs`, yearly origins 2016–2025, horizons
{90, 365, 730, 1458}:

```text
walk-forward log10 RMSE: replay 0.4662 vs composite 0.2120
P10–P90 coverage: 0.83 (30/36; target 0.80)
```

The test asserts composite < 0.85 × replay RMSE, composite RMSE < 0.32, and coverage
≥ 0.60 — deliberately slack bounds so the test guards against regressions without being
brittle to fixture updates. Recompute cost: full fit + band grid + 1458 projected points
runs in ~2 s (debug profile) on ~5 400 daily closes — negligible for a background tick.

### 7.7 Implementation map

| Piece | Location |
|---|---|
| Model + constants + anchors | `src/collectors/cycle_projection.rs` |
| Real-data overlay (unchanged semantics) | `src/collectors/cycle_overlay.rs` |
| Bands migration (additive, nullable) | `migrations/0017_cycle_overlay_bands.sql` |
| DTO fields (`price_low`/`price_high`, DecimalString-or-null) | `src/api/dto.rs` |
| OpenAPI schema | `api/crypto-collector.yaml` |
| Walk-forward regression test + fixture | `tests/backtest_projection.rs`, `tests/fixtures/btc_daily_close.csv` |
| SPEC | `.moai/specs/SPEC-CYCLE-001/spec.md` (v0.4.0, REQ-CYCLE-064) |

API compatibility: all pre-existing fields and their semantics are unchanged; projected
points' `price` is now the model's P50 path (previously the replay path — same field, same
type, still flagged `projected = true`); `price_low`/`price_high` are additive and `null`
on real points. The keyset cursor contract is untouched.

---

## 8. Remaining limitations & future ideas

Limitations (known and accepted):

- **n = 3–4 cycles.** All cycle-level statistics (amplitude decay, phase shape) are
  anecdotes, not statistics. The model mitigates by leaning on the spine (5 000+ daily
  observations), shrinking the cycle component (×0.75), and flooring the damping
  extrapolation — but a structural break (e.g. cycles ceasing to exist post-ETF) would
  surface only gradually through the mean-reversion term.
- **4-year band calibration is approximate** (52% inside at 1458d vs 80% target): 15 years
  of data contain only ~3 independent 4-year windows. Bands at horizons ≤ 3 years are
  well-calibrated; consumers should treat the final year of the projection as indicative.
- **Peak timing is not solved.** Peak *price* errors improved 2–10× over the replay, but
  the predicted peak day can still land far from the realised one (cycle 4: +924 days when
  measured against the window max). The phase component encodes where peaks historically
  cluster; it cannot anticipate cycle-specific timing shifts.
- **Non-stationarity of the spine.** The power-law exponent has drifted 5.5 → 5.7 across
  fit windows; the walk-forward record is good but there is no economic guarantee of
  continuation. Bands are descriptive quantiles, not statistical confidence intervals.
- **BTC-specific calibration anchors.** The compiled-in pre-2017 anchors apply only to
  `bitcoin`/`usd`; any other configured pair falls back to fitting on stored history alone
  (correct but potentially biased if that history is short).
- **Estimated 2028-04-20 halving** carries ~±30 days of block-time uncertainty; projected
  cycle-5 assignment shifts accordingly.

Future ideas:

- Phase-conditioned band quantiles (band width as a function of cycle phase, not just
  horizon) if mis-coverage proves phase-concentrated.
- OHLCV-computable regime flags — Mayer Multiple (close / 200d MA) and a Puell proxy
  (deterministic issuance × close vs its 365d MA) — could modulate the mean-reversion
  half-life in bear vs bull regimes.
- Robust (Huber) spine fitting to reduce peak-era leverage (BTCautoresearch found modest
  gains; `loess-rs` offers a ready IRLS primitive).
- Refresh the compiled-in anchor list and re-run the backtest when a fifth halving occurs
  (same code-update path as the halving-date constants, OR-CYCLE-5).
- Quadratic-in-log-time spine term (LGC-style `γ·(log d)²`, γ < 0) — accept only if it
  beats the 2-parameter spine on held-out pinball loss (it did not in the prototype era
  tested; revisit with more data).

---

## Sources

- H.C. Burger, *Bitcoin's natural long-term power-law corridor of growth* —
  https://hcburger.com/blog/powerlaw/ (a = −17.016, b = 5.845, R² = 0.931, tops slope 5.029)
- H.C. Burger, *Diminishing returns* — https://hcburger.com/blog/diminishingreturns/
- G. Santostasi, *Bitcoin Power Law Theory* — https://bitcoinpower.law/ ; Fulgur Ventures
  executive summary — https://medium.com/@fulgur.ventures/bitcoin-power-law-theory-executive-summary-report-837e6f00347e
- *Bitcoin Power Law Price Prediction Using Quantile Regression* —
  https://researchbitcoin.net/bitcoin-power-law-price-prediction-using-quantile-regression/
- M. Burger, *Debunking Bitcoin's … power-law corridor* —
  https://medium.com/amdax-asset-management/debunking-bitcoins-natural-long-term-power-law-corridor-of-growth-c1f336e558f6
  and rebuttal https://medium.com/quantodian-publications/bitcoins-power-law-really-debunked-2e5add103ba9
- Dave the Wave, LGC — https://davethewave.substack.com/p/bitcoins-logarithmic-growth-curve
- S2F critiques — https://marcelrburger.medium.com/reviewing-modelling-bitcoins-value-with-scarcity-part-iv-the-theoretical-framework-leading-d248ae87a138 ;
  https://stephanlivera.com/episode/122/ ; https://www.coingecko.com/learn/bitcoin-stock-to-flow-model-explained
- MVRV-Z definition — https://docs.glassnode.com/further-information/metric-guides/mvrv/mvrv-z-score
- Cycle statistics — https://www.coingecko.com/research/publications/when-bitcoin-all-time-highs ;
  https://www.nydig.com/research/charting-drawdowns-during-up-cycles ;
  https://www.fidelity.com/learning-center/trading-investing/four-year-bitcoin-and-crypto-cycles
- Quantile-from-residuals — https://arxiv.org/html/2508.15922v1 ; walk-forward methodology —
  https://www.sciencedirect.com/science/article/pii/S2405844022031504
- Small-sample shrinkage — https://www.sciencedirect.com/science/article/abs/pii/S0169207012000817 ;
  https://arxiv.org/pdf/1109.4533
- Halving dates — https://charts.bitbo.io/halving-dates/
- Backtest fixture provenance: Bitstamp public OHLC API (2011-08-18 → 2017-08-17) merged
  with the collector's own `coin_candles` 1d closes (2017-08-18 →).
