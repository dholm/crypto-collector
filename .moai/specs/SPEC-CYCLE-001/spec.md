---
id: SPEC-CYCLE-001
version: 0.1.0
status: completed
created: 2026-07-04
updated: 2026-07-04
author: dholm
priority: medium
issue_number: 0
---

# SPEC-CYCLE-001 — Bitcoin Halving-Cycle Overlay (Derived "Cycle Repeat" Data)

A **derived-analytics** feature that reproduces the *data* behind Bitbo's "Cycle Repeat"
chart (`https://charts.bitbo.io/cycle-repeat/`) **without scraping it**. That page is gated
behind Cloudflare Turnstile (HTTP 428 human challenge), so it is not a data source. The chart
is nothing more than a **local transform of Bitcoin daily price history sliced at each
halving**, and this SPEC computes that transform from data the collector already persists.

The transform: partition the coin's daily (`1d`) BTC/USD price history into halving cycles and,
for each cycle, emit points of `(days_since_halving, normalized_price)` so a downstream consumer
can overlay every cycle on a shared X-axis and compare cycle-over-cycle performance.

Data contract / source of daily candles: [SPEC-DB-001](../SPEC-DB-001/spec.md) (`coin_candles`,
PK `(coin_id, vs_currency, interval, ts)`, `src/models/quote.rs:33-46`) and the coin-keyed read
model of [SPEC-API-002](../SPEC-API-002/spec.md). The optional read-time derivation of a `1d`
series from finer stored candles is [SPEC-API-003](../SPEC-API-003/spec.md). Route surface,
keyset cursor pagination, `DecimalString` serialisation, error model, and OpenAPI parity carry
over from [SPEC-API-001](../SPEC-API-001/spec.md). The periodic collector tick that drives
recomputation is [SPEC-SCHED-001](../SPEC-SCHED-001/spec.md) (collection-queue worker). The
`Decimal`-not-`f64` money invariant is REQ-PROV-012.

## HISTORY

- 2026-07-07 (v0.4.0): Replaced the v0.3.0 cycle-repeat replay with a **composite projection
  model** (`src/collectors/cycle_projection.rs`; research + validation in
  `docs/prediction-research.md`): power-law spine (log-log OLS over the full daily history,
  augmented for `bitcoin`/`usd` by 25 compiled-in pre-2017 quarterly calibration anchors) + a
  damped, phase-conditioned cycle component (encoding measured diminishing returns) + a
  mean-reversion continuity term (half-life 180 days, join-continuous at today's real price).
  Projected points now carry P10/P90 confidence bands (`price_low`/`price_high`, additive
  nullable columns + DTO fields, REQ-CYCLE-064); `price` is the P50 path. Selected by
  walk-forward backtest: log10 RMSE 0.21 vs the replay's 0.47 on yearly origins 2016–2025
  (deterministic regression test `tests/backtest_projection.rs` on the committed fixture
  `tests/fixtures/btc_daily_close.csv`). The REQ-CYCLE-060 replay formula is superseded; the
  REQ-CYCLE-061/062/063 contracts (cycle assignment via the extended halving list, projected
  flags, provisional recompute, zero points under insufficient history) are preserved
  verbatim. All model math is `Decimal` (`maths` feature), deterministic, no RNG.
- 2026-07-04 (v0.3.0): Replaced the v0.2.0 "repeat the reference cycle's shape onto the current
  cycle's own anchor price" projection with Bitbo's "cycle-repeat" replay methodology
  (REQ-CYCLE-060/061/062/063, superseding the v0.2.0 wording of REQ-CYCLE-060/061/062 below).
  The old model anchored the projection on the reference cycle's halving-day price, which
  produced a large discontinuity at the join (the projection did not start from today's real
  price) and an unbounded overshoot (it replayed the reference cycle's full multiple with no
  reference to the current cycle's actual trajectory). The new model instead replays the
  ACTUAL trailing 1458-day (one halving-cycle, `CYCLE_DAYS`) daily-return series forward from
  today, scaled by today's real price: `projected_price[today + k] = current_price *
  (P[today - 1458 + k] / P[today - 1458])`. This is continuous at the join by construction (k=1
  is one reference-day return away from `current_price`) and its magnitude reflects the current
  cycle's own actual gains (diminishing returns are baked in, not modelled). Projected points
  span up to 1458 days forward and may cross the (unknown) next halving; they are assigned
  `cycle_number`/`days_since_halving` using a NEW projection-only extended halving list — the
  four real halvings plus one ESTIMATED 2028-04-20 halving (block-height projection, not a
  confirmed date) — that exists solely to place projected points on the correct side of that
  future boundary. This extended list is entirely separate from `halving_dates()`/`assign_cycle`,
  which are unchanged: real-data cycle 4 remains open-ended (REQ-CYCLE-012 preserved verbatim).
- 2026-07-04 (v0.2.0): Added a forward-projected extension of the current (in-progress) cycle
  (REQ-CYCLE-060/061/062): repeats the last COMPLETED cycle's shape onto the current cycle out
  to the next halving, flagged `projected = true`. New `projected` column/field
  (backward-compatible, defaults `false`). Supersedes the "No forecasting" exclusion for this
  narrow, clearly-flagged case — real historical points and their semantics are unchanged.
- 2026-07-04 (v0.1.0): Initial draft. Derived Bitcoin halving-cycle overlay computed locally
  from persisted daily (`1d`) `coin_candles` — no Bitbo scrape, no new upstream provider. Two
  normalization baselines stored per point (halving-day anchor and cycle-low anchor), both
  plotted against `days_since_halving` (deliberate asymmetry: the cycle-low series is **not**
  re-based to days-since-low). Delivered as a materialised table recomputed on the periodic
  collector tick **and** a new keyset-paginated `/v1` read route. New `REQ-CYCLE-0NN` range.

---

## Goal

Given the daily BTC/USD candle history the collector already stores, produce a **materialised
cycle-overlay dataset** and expose it over the existing `/v1` REST surface, such that a client can
plot each halving cycle on a shared `days_since_halving` X-axis under two independent Y-axis
normalizations — one anchored to the price on the halving day, one anchored to the cycle's lowest
price — reproducing the shape of Bitbo's "Cycle Repeat" chart entirely from local data. The
dataset is a pure function of the stored daily candles: it fabricates nothing, interpolates
nothing, and never contacts Bitbo or any new upstream.

## Scope

In scope:
- **Deriving** cycle-overlay points from the persisted daily (`1d`) OHLCV history in
  `coin_candles` for a single configured target coin/currency (default `bitcoin`/`usd`).
- **Two normalization baselines per point**, both stored:
  1. halving-day baseline: `price / price_on_halving_anchor` (the halving day = `1.0`);
  2. cycle-low baseline: `price / cycle_low_price` (the cycle low = `1.0`).
- **X-axis** = `days_since_halving` (day 0 = the halving date) for **all** series, including the
  cycle-low-normalized series (the deliberate asymmetry, REQ-CYCLE-023).
- **A new materialised DB table** (new migration, run at startup via `sqlx::migrate!()`) storing
  the computed points, **recomputed on the periodic collector tick** (consistent with candle
  refresh; SPEC-SCHED-001, SPEC-API-003).
- **A new keyset-paginated `/v1` read route** serving the overlay, reusing the cursor, limit,
  `DecimalString`, and error conventions of SPEC-API-001, plus its OpenAPI parity requirement.
- **Explicit edge-case behaviour** for incomplete/partial history, the open-ended in-progress
  cycle, intra-cycle gaps, and a missing halving-day anchor.
- **Env-var-only configuration** of the target coin, currency, and recompute cadence.

Out of scope: see Exclusions. This SPEC adds no upstream provider, changes no existing collection
or candle write path, adds no forecasting, and adds no altcoin/generic cycle model.

## Decisions Restated (authoritative)

Confirmed with the user; encoded here verbatim in intent.

- **D1 — No scraping; derive locally.** Bitbo's page is a Turnstile-gated visualisation (HTTP
  428), rejected as a data source. The overlay is computed from the collector's own persisted
  daily candles (SPEC-API-003 daily history), with **no new upstream provider dependency**.
- **D2 — Both baselines, stored per point.** Every point stores both `norm_halving`
  (`price / price_on_halving_anchor`, halving day = `1.0`) and `norm_cycle_low`
  (`price / cycle_low_price`, cycle low = `1.0`). The consumer chooses which to plot.
- **D3 — Single X-axis = `days_since_halving`, deliberately asymmetric.** All series, *including*
  the cycle-low-normalized series, are plotted against `days_since_halving` (day 0 = the halving
  date). The cycle-low series is **not** re-based onto a days-since-low axis. This asymmetry is
  intentional and MUST NOT be "corrected" to days-since-low in a later change (REQ-CYCLE-023,
  Exclusions).
- **D4 — Dual delivery: materialised table + REST route.** A DB table stores the computed points
  and is **recomputed on the periodic collector tick** (like candles refresh — SPEC-API-003, the
  collectors module). A new `axum` `/v1` route serves the overlay using the existing keyset
  cursor + DTO conventions (SPEC-API-001).
- **D5 — Daily granularity, reused history.** The source is the stored daily (`1d`) candle series
  for the configured coin/currency; **no** new collection cadence and **no** new provider are
  introduced (SPEC-API-003, SPEC-SCHED-001).
- **D6 — Known halving dates are constants (approximate/block-derived).** `2012-11-28`,
  `2016-07-09`, `2020-05-11`, `2024-04-20`. They are block-height-derived historical facts, not
  secrets or tunables; the next halving is unknown, so the most recent cycle is open-ended.

### Decisions this SPEC makes (flagged; confirmable at run — see Open Items)

- **D7 — Daily price basis = the `1d` candle `close`.** The representative daily price is the
  close of the day's `1d` candle in the resolved `vs_currency`. **The same close series** is used
  for the numerator (the point's price) **and** for both denominators (the halving-day anchor and
  the `cycle_low_price`). Using one consistent series is what makes "halving day = 1.0" and "cycle
  low = 1.0" hold *exactly*: if the cycle-low denominator were the minimum daily *low* while the
  numerator were the *close*, the lowest point would normalise to slightly above `1.0`. (OR-CYCLE-1)
- **D8 — Missing-halving-day anchor fallback.** When the exact halving-date daily candle is
  absent (common for older, partially-backfilled cycles), the halving-day anchor is the **first
  available** daily candle whose `ts >= halving_date` within the cycle. That first available day
  then normalises to `1.0`, while `days_since_halving` is still measured from the true halving
  date (so the series may begin at, e.g., day 12 rather than day 0). Such a cycle's halving
  baseline is marked **approximate**. (OR-CYCLE-2)
- **D9 — No interpolation, no forward-fill.** Only days that have an actual daily candle produce a
  point. Missing intra-cycle days yield no point; the `days_since_halving` sequence is therefore
  **sparse / non-contiguous** where data is missing. Nothing is fabricated or gap-filled
  (consistent with SPEC-API-003 REQ-API-211). (OR-CYCLE-3)

---

## Domain Model (WHAT, not HOW)

### Halving cycles

A **cycle** is the half-open interval `[halving_date, next_halving_date)`. The halving date
belongs to its own cycle (it is day 0). Cycles are numbered by the halving that starts them:

| cycle_number | halving_date (UTC, approximate) | cycle window                     | state       |
|--------------|---------------------------------|----------------------------------|-------------|
| 1            | 2012-11-28                      | [2012-11-28, 2016-07-09)         | closed      |
| 2            | 2016-07-09                      | [2016-07-09, 2020-05-11)         | closed      |
| 3            | 2020-05-11                      | [2020-05-11, 2024-04-20)         | closed      |
| 4            | 2024-04-20                      | [2024-04-20, +∞) — open-ended    | in-progress |

The genesis-to-first-halving era (pre-2012-11-28) is **not** a cycle in this model — the overlay
begins at the first known halving. The in-progress cycle (4) has no known end and extends to the
**latest available daily candle** (REQ-CYCLE-012).

### Per-point quantities

For each day `d` in a cycle that has a stored daily candle, with `price(d)` = that day's `1d`
close in the resolved currency (D7):

- `days_since_halving(d)` = whole-day count `floor(date(d) - halving_date)`, day 0 = halving date.
- `norm_halving(d)`   = `price(d) / price_on_halving_anchor` — the anchor day is `1.0` (D2, D8).
- `norm_cycle_low(d)` = `price(d) / cycle_low_price` — the cycle-low day is `1.0` (D2), where
  `cycle_low_price` = the minimum of `price(·)` over the cycle's available days (running minimum
  for the in-progress cycle, REQ-CYCLE-034).

Both `norm_halving` and `norm_cycle_low` are plotted at the **same** x = `days_since_halving(d)`
(D3). All three quantities (`price`, `norm_halving`, `norm_cycle_low`) are `rust_decimal::Decimal`
and are never round-tripped through `f64` (REQ-CYCLE-024, REQ-PROV-012).

### Materialisation and refresh

The points are stored in a new table and **recomputed on the periodic collector tick** as a full
derived rebuild from the current `coin_candles` contents (D4). Because the in-progress cycle's
minimum and latest day advance over time, a recompute can change previously-emitted points of the
current cycle — this is expected (REQ-CYCLE-034), not a regression.

### Read route

`GET /v1/coins/{coin_id}/cycle-overlay` returns the stored points, keyset-paginated, ordered by
`(cycle_number ASC, days_since_halving ASC)`, each item carrying **both** normalized baselines and
the raw daily price. Pagination, limit validation, `DecimalString` serialisation, and the error
model are exactly those of SPEC-API-001; the endpoint is added to the published OpenAPI document.

---

## Requirements (EARS)

### Source, price basis, and no-scrape boundary

- **REQ-CYCLE-001** (Ubiquitous): The system shall derive the cycle overlay solely from the
  persisted daily (`1d`) OHLCV history in `coin_candles` for the configured target coin and
  `vs_currency`, and shall introduce no new upstream provider dependency and issue no request to
  Bitbo or any external chart source (D1, D5).
- **REQ-CYCLE-002** (Ubiquitous): The system shall use a single consistent daily price basis — the
  `1d` candle `close` in the resolved `vs_currency` — as both the per-point price and the source
  of both normalization denominators (the halving-day anchor and the cycle-low price), so that the
  anchor day and the cycle-low day each normalise to exactly `1.0` (D7).
- **REQ-CYCLE-003** (Unwanted): The system shall not scrape, fetch, or attempt to bypass the
  Cloudflare Turnstile challenge of `charts.bitbo.io`; the "Cycle Repeat" shape shall be produced
  only by local transform of stored candles (D1).

### Halving cycles and the X-axis

- **REQ-CYCLE-010** (Ubiquitous): The system shall treat the halving dates `2012-11-28`,
  `2016-07-09`, `2020-05-11`, and `2024-04-20` as compiled-in constants (block-derived,
  approximate), shall define each cycle as the half-open interval `[halving_date,
  next_halving_date)` with the halving date belonging to its own cycle, and shall number cycles by
  their starting halving (cycle 1 = 2012-11-28 … cycle 4 = 2024-04-20) (D6).
- **REQ-CYCLE-011** (Ubiquitous): For every point the system shall compute `days_since_halving` as
  the whole-day difference between the candle's UTC date and its cycle's halving date, with day 0
  being the halving date, and shall use this value as the X-axis coordinate for **all** stored
  series (D3).
- **REQ-CYCLE-012** (State-Driven): While a cycle is the most recent (its next halving has not
  occurred), the system shall treat that cycle as open-ended, extending its overlay from the
  halving date to the latest available daily candle, and shall not assume or fabricate a cycle end
  date (D6).

### Normalization (both baselines)

- **REQ-CYCLE-020** (Ubiquitous): For each point the system shall compute the halving-day baseline
  as `price / price_on_halving_anchor` in `Decimal`, such that the anchor day normalises to `1.0`
  (D2).
- **REQ-CYCLE-021** (Ubiquitous): For each point the system shall compute the cycle-low baseline as
  `price / cycle_low_price` in `Decimal`, where `cycle_low_price` is the minimum daily price over
  the cycle's available days, such that the cycle-low day normalises to `1.0` (D2, D7).
- **REQ-CYCLE-022** (Ubiquitous): The system shall store **both** normalized baselines
  (`norm_halving` and `norm_cycle_low`) on every point, together with the raw daily `price` (D2).
- **REQ-CYCLE-023** (Ubiquitous): The system shall plot the cycle-low-normalized series against
  `days_since_halving` (day 0 = the halving date), **not** against days-since-low; this asymmetry
  between the two baselines is deliberate and shall be preserved (D3).
- **REQ-CYCLE-024** (Ubiquitous): The system shall compute and store `price`, `norm_halving`, and
  `norm_cycle_low` as `rust_decimal::Decimal` (Postgres `NUMERIC`) with no `f64` round-trip at any
  stage (REQ-PROV-012).

### Edge cases (incomplete history, gaps, in-progress cycle)

- **REQ-CYCLE-030** (State-Driven): While a cycle has no stored daily candles, the system shall
  omit that cycle from the overlay entirely (no points), and while a cycle has only part of its
  daily history stored, the system shall represent it using only the days that have candles.
- **REQ-CYCLE-031** (Unwanted): If the stored daily history does not reach back to a cycle's
  halving date (or does not cover a cycle at all), then the system shall not treat the missing
  early data as an error and shall not fail the recompute; absent early cycles simply produce no
  points.
- **REQ-CYCLE-032** (If): If the exact halving-date daily candle is absent for a cycle, then the
  system shall use the first available daily candle whose `ts >= halving_date` within that cycle as
  the halving-day anchor (so that day normalises to `1.0`), shall still measure `days_since_halving`
  from the true halving date, and shall mark that cycle's halving baseline as approximate (D8).
- **REQ-CYCLE-033** (Unwanted): If a cycle is missing daily candles for interior days, then the
  system shall not interpolate, forward-fill, or synthesise any point; only days with an actual
  stored candle produce a point, so the `days_since_halving` sequence may be non-contiguous (D9).
- **REQ-CYCLE-034** (State-Driven): While a cycle is in-progress (REQ-CYCLE-012), the system shall
  compute `cycle_low_price` as the running minimum over the daily prices available so far and shall
  treat that cycle's normalized values as provisional — a later recompute MAY change previously
  emitted points of the in-progress cycle as new data (including new lows) arrives, and this shall
  not be treated as a data error.

### Materialisation and recompute on the collector tick

- **REQ-CYCLE-040** (Ubiquitous): The system shall persist the overlay points in a new database
  table created by a new migration applied at startup via `sqlx::migrate!()`, with `Decimal`
  (`NUMERIC`) columns for `price`, `norm_halving`, and `norm_cycle_low`, and a primary key that
  uniquely identifies a point by `(coin_id, vs_currency, cycle_number, days_since_halving)`.
- **REQ-CYCLE-041** (Event-Driven): When the periodic collector tick fires (the same cadence
  mechanism that refreshes candles — SPEC-SCHED-001, SPEC-API-003), the system shall recompute the
  overlay for the configured coin/currency from the current `coin_candles` contents and persist the
  result, replacing stale points, as an idempotent operation.
- **REQ-CYCLE-042** (Ubiquitous): The recompute shall be a pure derived rebuild from `coin_candles`
  that fabricates no candle and no price, and shall be safe to run repeatedly and under multiple
  replicas (idempotent replacement; single-owner execution via the existing collection-queue lease
  / `SKIP LOCKED` discipline of SPEC-SCHED-001).
- **REQ-CYCLE-043** (Ubiquitous): The system shall read the target coin id (default `bitcoin`),
  the `vs_currency` (default `usd`), and the recompute cadence from environment variables only,
  consistent with the env-var-only configuration invariant (no config files, no hardcoded
  secrets).

### Read route (`/v1`), pagination, and OpenAPI parity

- **REQ-CYCLE-050** (Event-Driven): When a client requests `GET /v1/coins/{coin_id}/cycle-overlay`,
  the system shall return a keyset-paginated page of overlay points, each carrying
  `cycle_number`, `halving_date`, `days_since_halving`, the raw daily `price`, and **both**
  `norm_halving` and `norm_cycle_low`, ordered by `(cycle_number ASC, days_since_halving ASC)`.
- **REQ-CYCLE-051** (Ubiquitous): The endpoint shall use an opaque base64url-no-pad keyset cursor
  encoding the ordering key `(cycle_number, days_since_halving)` of the last returned row (via the
  existing `encode_keyset_cursor` / `decode_keyset_cursor` helpers), shall accept a `limit`
  validated against the documented maximum, shall return a `next_cursor` that is null when
  exhausted, and shall serialise all `Decimal` values losslessly as `DecimalString`
  (SPEC-API-001 REQ-API-070/072/073).
- **REQ-CYCLE-052** (Event-Driven): When a client requests the overlay with an optional
  `vs_currency` query parameter (default `usd`) and/or an optional `cycle` filter, the system shall
  scope the response accordingly; when the requested coin is not the configured target coin or has
  no computed overlay (e.g. a coin with no halving schedule), the system shall respond HTTP 200
  with an empty page (`{"items": [], "next_cursor": null}`), not 404 and not an error.
- **REQ-CYCLE-053** (If/Unwanted): If a supplied `cursor` cannot be decoded into the endpoint's
  keyset key, or a supplied `limit` is out of range, then the system shall respond HTTP 400 without
  querying (SPEC-API-001 REQ-API-071/072).
- **REQ-CYCLE-054** (Optional): Where the OpenAPI document `api/crypto-collector.yaml` is published
  (SPEC-API-001 REQ-API-002/003), the system shall add this endpoint, its response schema (both
  baselines), and its operationId to that document and keep it in parity via the doc-parity test.

### Forward-projected current cycle — Bitbo cycle-repeat replay (v0.3.0, supersedes v0.2.0)

- **REQ-CYCLE-060** (Ubiquitous): The system shall extend the daily price series forward by
  replaying the ACTUAL trailing `CYCLE_DAYS` (= 1458, one halving cycle) daily-return window,
  scaled by today's real price: for `k = 1..=CYCLE_DAYS`, `projected_price[today + k] =
  current_price * (P[today - CYCLE_DAYS + k] / P[today - CYCLE_DAYS])`, where `today` is the
  latest date with a real daily candle, `current_price = P[today]`, and `P` is the same daily
  `(date, close)` series used by `compute_overlay` (last-observation-carried-forward across any
  gaps in the reference window). This replaces the v0.2.0 model of repeating the last completed
  cycle's shape anchored on that cycle's own halving-day price.
- **REQ-CYCLE-061** (Ubiquitous): Each projected point shall carry a real future `ts` (`today +
  k` days), the computed `price` (`Decimal`, never `f64` — REQ-CYCLE-024/REQ-PROV-012),
  `halving_baseline_approximate = true` (a projection, and any cycle-5 halving date it is keyed
  against is itself an estimate — REQ-CYCLE-063), and `projected = true`; real historical points
  continue to carry `projected = false`. `norm_halving` and `norm_cycle_low` are computed against
  each projected point's own assigned cycle: the anchor/low for a cycle that also has real points
  reuses that cycle's real halving-day anchor and folds real prices into the cycle low; a fully
  projected future cycle anchors and lows against its own projected points only. Projected points
  shall sort after real points under the existing `(cycle_number, days_since_halving)` order
  (REQ-CYCLE-050/051) with no change to the cursor/keyset contract.
- **REQ-CYCLE-062** (Unwanted): If fewer than `CYCLE_DAYS` days of daily history are available
  (i.e. `today - CYCLE_DAYS` predates the earliest stored daily candle), then the system shall
  emit zero projected points — this is not an error (mirrors REQ-CYCLE-030/031). Projected points
  are provisional: they are recomputed on every recompute tick and their reference window shifts
  forward as new real data arrives (same idempotent-rebuild contract as REQ-CYCLE-034/041).
- **REQ-CYCLE-063** (Ubiquitous): Because the `CYCLE_DAYS`-day projection horizon can cross the
  next (unknown) halving, the system shall assign each projected point's `cycle_number` /
  `days_since_halving` using a projection-only extended halving-date list: the four compiled-in
  real halving dates (REQ-CYCLE-010) plus one ESTIMATED next halving (`2028-04-20`, a
  block-height projection, clearly marked as an estimate in code) used solely to place projected
  points on the correct side of that future boundary. This extended list is entirely separate
  from `halving_dates()`/`assign_cycle`, which remain unchanged for real data — cycle 4 stays
  open-ended for real points regardless of the projection (REQ-CYCLE-012 preserved verbatim).
- **REQ-CYCLE-064** (Ubiquitous, v0.4.0): Each projected point shall carry P10 and P90
  confidence bands (`price_low`, `price_high`; `Decimal`, additive nullable columns/fields —
  `NULL` on real points), derived from deterministic empirical horizon quantiles of the
  model's historical errors blended with residual-change quantiles (see
  `docs/prediction-research.md` §5/§7.4); `price` on a projected point is the model's median
  (P50) path, and `price_low <= price <= price_high` shall hold on every projected point.
  The projection model itself is the composite decomposition of v0.4.0 (power-law spine +
  damped cycle-phase component + mean-reversion continuity term), which supersedes the
  REQ-CYCLE-060 replay formula while preserving the REQ-CYCLE-061/062/063 contracts, and
  any change to its constants shall be re-validated against the walk-forward backtest
  (`tests/backtest_projection.rs`).

## Exclusions (What NOT to Build)

- **No Bitbo scraping / no Turnstile bypass** — the "Cycle Repeat" data is derived locally from
  stored candles; the collector never fetches `charts.bitbo.io` (REQ-CYCLE-001/003).
- **No new upstream provider and no new collection cadence** — the overlay reuses the existing
  daily `coin_candles` history; the provider chain, live poller, backfill, and candle write paths
  are untouched (SPEC-PROV-001, SPEC-SCHED-001).
- **No interpolation, forward-fill, or synthetic points** — missing days are simply absent; the
  `days_since_halving` sequence may be sparse (REQ-CYCLE-033).
- **No days-since-low X-axis** — the cycle-low baseline is deliberately plotted against
  `days_since_halving`; a days-since-low re-basing is explicitly out of scope and must not be added
  as a "fix" (REQ-CYCLE-023).
- **No forecasting beyond the flagged projection** — the next halving date and future prices are
  not predicted using models, trends, or external inputs; the only forward-looking data is the
  explicitly `projected = true` repetition of the last completed cycle's shape (REQ-CYCLE-060),
  which is provisional, recomputed every tick, and never presented as an unflagged real point.
- **No generic / altcoin cycle model** — halving cycles are Bitcoin-specific; the overlay is
  computed for the single configured target coin, and other coins yield an empty page
  (REQ-CYCLE-052). No configurable per-coin cycle schedules.
- **No `f64`** — every price and normalized value stays `Decimal` end to end (REQ-CYCLE-024).
- **No change to existing endpoints, cursors, or DTOs** — a new route, a new keyset key type, and a
  new DTO are added; the existing candle/quote/market reads and their cursors are unchanged.
- **No secrets or config files** — coin, currency, and cadence come from environment variables only
  (REQ-CYCLE-043).

## @MX Annotation Targets (high fan_in)

- The cycle-partitioning + `days_since_halving` helper (halving-date constants → cycle assignment →
  whole-day offset) — `@MX:ANCHOR` (every overlay point depends on it) + `@MX:REASON`: half-open
  `[halving, next_halving)` boundaries and day-0 = halving-date are the correctness core
  (REQ-CYCLE-010/011).
- The dual-normalization fold (`norm_halving`, `norm_cycle_low` in `Decimal`, with the D8 anchor
  fallback and the D7 single-series rule) — `@MX:WARN`/`@MX:REASON`: mixing series (close vs low)
  or dividing before the running-min settles silently breaks the "= 1.0" invariants
  (REQ-CYCLE-020/021/034); `Decimal`-only, never `f64` (REQ-CYCLE-024).
- The recompute-on-tick entry point — `@MX:NOTE` that it is a full idempotent derived rebuild and
  that in-progress-cycle points are provisional and may change between ticks (REQ-CYCLE-041/034).
- The new `(cycle_number, days_since_halving)` keyset key encode/decode — `@MX:ANCHOR` (the
  route's pagination contract; reuses the SPEC-API-001 opaque-cursor helpers, REQ-CYCLE-051).

## Open Items (do not guess)

- **OR-CYCLE-1 — Daily price basis (RESOLVED to `close`, confirm at run).** D7 uses the `1d`
  `close` for both numerator and denominators so the anchor and cycle-low normalise to exactly
  `1.0`. Alternative bases (e.g. cycle-low as the minimum daily *low*, or a typical
  `(H+L+C)/3` price) are possible but would break the exact "= 1.0" property unless applied to both
  sides. Confirm `close` at run.
- **OR-CYCLE-2 — Missing-halving-day anchor (RESOLVED to forward-search first-available, confirm
  at run).** D8 anchors on the first available daily candle with `ts >= halving_date`. An
  alternative (nearest candle within a bounded tolerance on either side of the halving date, or
  omitting the halving baseline entirely when day 0 is absent) is possible. Confirm the
  forward-search rule and whether the "approximate" flag is surfaced in the DTO at run.
- **OR-CYCLE-3 — Recompute wiring.** Whether the recompute is a new `collection_queue` `kind`
  enqueued periodically (like candle refresh) or a dedicated periodic task in the collectors
  module is a Run-phase decision; both satisfy REQ-CYCLE-041/042. Recommend a new queue `kind`
  for consistency with SPEC-SCHED-001; confirm at run.
- **OR-CYCLE-4 — `1d` source availability vs SPEC-API-003 aggregation.** The overlay reads native
  `1d` candles. Whether it MAY additionally reuse the SPEC-API-003 read-time aggregation to derive
  a `1d` series from finer stored candles when native `1d` rows are absent is a Run-phase decision;
  it is allowed but not required by this SPEC. Confirm at run.
- **OR-CYCLE-5 — Future-halving extensibility.** Halving dates are compiled-in constants (D6). If a
  fifth halving occurs, the current cycle closes and a new one opens via a code update. Whether to
  additionally allow an env-configurable list of future halving dates is deferred; recommend
  keeping constants for now (block-derived facts, not tunables).
- **OR-CYCLE-6 — `cycle` filter shape.** REQ-CYCLE-052 allows an optional `cycle` filter; whether
  it accepts a `cycle_number` (e.g. `?cycle=3`) or a `halving_date` is a minor Run-phase decision.
  Recommend `cycle_number`; confirm at run.
