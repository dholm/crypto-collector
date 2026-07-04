---
id: SPEC-CYCLE-001
type: acceptance
updated: 2026-07-04
---

# SPEC-CYCLE-001 — Acceptance Criteria

Given/When/Then scenarios for the Bitcoin halving-cycle overlay. Each maps to one or more
`REQ-CYCLE-0NN`. Price and normalized values are asserted as JSON strings (`DecimalString`,
REQ-CYCLE-024/051). Unless stated, the configured target is `bitcoin`/`usd`, the source is the
persisted daily (`1d`) `coin_candles`, and route requests target
`GET /v1/coins/{coin_id}/cycle-overlay`.

## Scenario 1 — Derived locally, no Bitbo scrape, no new provider (REQ-CYCLE-001, 003)

- Given the collector has daily `1d` `bitcoin`/`usd` candles persisted in `coin_candles`,
- When the overlay is recomputed,
- Then every point is derived solely from those stored candles,
- And no HTTP request is made to `charts.bitbo.io` and no new upstream provider is invoked.

## Scenario 2 — Cycle partitioning and day-0 = halving date (REQ-CYCLE-010, 011)

- Given daily candles spanning `2020-05-11` through `2024-04-19`,
- When the overlay is recomputed,
- Then those candles are assigned to cycle 3 (`[2020-05-11, 2024-04-20)`), the candle dated
  `2020-05-11` has `days_since_halving == 0`, and the candle dated `2024-04-19` has
  `days_since_halving == 1439` (whole days from the halving date),
- And a candle dated `2024-04-20` belongs to cycle 4, not cycle 3 (half-open boundary).

## Scenario 3 — Halving-day baseline: anchor day normalises to exactly 1.0 (REQ-CYCLE-002, 020, 022)

- Given cycle 3 has a `1d` close of `8600` on the halving date `2020-05-11` and a `1d` close of
  `17200` on a later day,
- When the overlay is recomputed,
- Then the halving-date point has `norm_halving == "1"` (or `"1.0…"`, exactly 1.0),
- And the later day has `norm_halving == "2"` (`17200 / 8600`),
- And both points also carry a `norm_cycle_low` value (both baselines stored).

## Scenario 4 — Cycle-low baseline: the cycle-low day normalises to exactly 1.0 (REQ-CYCLE-021, 002)

- Given cycle 3's minimum daily close over its available days is `4000` (the cycle low), occurring
  on some day D, and another day has a close of `12000`,
- When the overlay is recomputed,
- Then day D has `norm_cycle_low == "1"` (exactly 1.0, because numerator and denominator are the
  same close series),
- And the `12000` day has `norm_cycle_low == "3"` (`12000 / 4000`).

## Scenario 5 — Deliberate asymmetry: cycle-low series is plotted against days-since-halving (REQ-CYCLE-023)

- Given cycle 3's low occurs on a day whose `days_since_halving == 200`,
- When the overlay is recomputed,
- Then the point carrying `norm_cycle_low == "1"` has `days_since_halving == 200` (its X-axis
  coordinate is measured from the halving date, NOT reset to 0 at the low),
- And no point uses a days-since-low X-axis anywhere in the dataset.

## Scenario 6 — Both baselines present on every point (REQ-CYCLE-022, 050)

- Given any cycle with stored daily candles,
- When the client requests the overlay,
- Then every returned item carries both a `norm_halving` and a `norm_cycle_low` field (plus the raw
  daily `price`), and neither baseline field is omitted.

## Scenario 7 — Missing early history is not an error; absent cycles are omitted (REQ-CYCLE-030, 031)

- Given the persisted daily history begins only in 2019 (no candles before then), so cycles 1
  (2012) and 2 (2016) have no stored candles,
- When the overlay is recomputed,
- Then cycles 1 and 2 produce zero points and the recompute completes successfully (no error, no
  failure),
- And cycle 3 (2020) and cycle 4 (2024) are present with points for their available days.

## Scenario 8 — Partial cycle represented by available days only (REQ-CYCLE-030)

- Given cycle 3's stored candles begin `2021-01-01` (roughly 235 days after the `2020-05-11`
  halving) rather than at the halving date,
- When the overlay is recomputed,
- Then cycle 3's earliest point has `days_since_halving` ≈ 235 (not 0), reflecting only the days
  that actually have candles.

## Scenario 9 — Missing halving-day anchor falls back to first available day (REQ-CYCLE-032)

- Given cycle 3 has no candle on `2020-05-11` (the halving date) but its first available candle is
  on `2020-05-23` (`days_since_halving == 12`) with close `9700`,
- When the overlay is recomputed,
- Then the `2020-05-23` point has `norm_halving == "1"` (the forward-searched anchor normalises to
  1.0), and its `days_since_halving == 12` (still measured from the true halving date),
- And that cycle's halving baseline is marked approximate.

## Scenario 10 — No interpolation / no forward-fill; sequence may be non-contiguous (REQ-CYCLE-033)

- Given cycle 3 is missing candles for `days_since_halving` 100 through 104 (a 5-day gap) while
  surrounding days have candles,
- When the overlay is recomputed,
- Then no point exists for `days_since_halving` 100–104 (the sequence jumps from 99 to 105), and no
  interpolated or forward-filled value appears in the gap.

## Scenario 11 — In-progress cycle extends to the latest candle and its low is provisional (REQ-CYCLE-012, 034)

- Given cycle 4 (`2024-04-20`, in-progress) has candles through the latest available day and its
  running-minimum close so far is `50000` on day D1,
- When the overlay is recomputed, then day D1 has `norm_cycle_low == "1"` and the newest point
  corresponds to the latest available daily candle (no assumed cycle end),
- And Given a subsequent recompute after a new, lower close of `40000` arrives on a later day D2,
- When the overlay is recomputed again, then day D2 now has `norm_cycle_low == "1"` and day D1's
  `norm_cycle_low` has changed (to `50000 / 40000`), and this shift is not treated as a data error.

## Scenario 12 — Recompute on the periodic tick is idempotent (REQ-CYCLE-041, 042)

- Given a fixed set of stored daily candles that does not change between two ticks,
- When the periodic collector tick triggers the overlay recompute twice in succession,
- Then the overlay table contents are identical after both runs (idempotent replacement; no
  duplicate rows, no drift), and each point is uniquely keyed by
  `(coin_id, vs_currency, cycle_number, days_since_halving)`.

## Scenario 13 — Configuration via environment variables only (REQ-CYCLE-043)

- Given no `CYCLE_OVERLAY_*` environment overrides are set,
- When the service starts, then the target defaults to `bitcoin`/`usd`,
- And Given `CYCLE_OVERLAY_VS_CURRENCY=eur` is set with `eur` daily candles present,
- When the overlay is recomputed, then it is computed over the `eur` daily series.

## Scenario 14 — Route returns keyset-paginated points ordered by (cycle, day) (REQ-CYCLE-050, 051)

- Given cycles 3 and 4 have overlay points,
- When the client requests `?limit=2`,
- Then the response returns 2 items ordered by `(cycle_number ASC, days_since_halving ASC)` with a
  non-null `next_cursor`,
- And When the client requests `?limit=2&cursor=<next_cursor>`,
- Then the next items follow in the same total order and `next_cursor` is null once exhausted,
- And all `price`/`norm_halving`/`norm_cycle_low` values are JSON strings (`DecimalString`).

## Scenario 15 — Unknown/non-target coin yields an empty page, not an error (REQ-CYCLE-052)

- Given `ethereum` (a coin with no halving schedule / no computed overlay),
- When the client requests `GET /v1/coins/ethereum/cycle-overlay`,
- Then the response is HTTP 200 with body `{"items": [], "next_cursor": null}` (not 404, not an
  error).

## Scenario 16 — Bad cursor or limit returns 400 (REQ-CYCLE-053)

- Given any request to the overlay route,
- When the client supplies an undecodable `cursor` or a `limit` outside the documented range
  (e.g. `0` or above the maximum),
- Then the response is HTTP 400 without querying.

## Scenario 17 — Optional vs_currency and cycle filters (REQ-CYCLE-052)

- Given `bitcoin` overlay points exist for `usd`,
- When the client requests `?vs_currency=usd&cycle=3`,
- Then only cycle-3 points are returned,
- And When the client omits `vs_currency`, then the effective currency defaults to `usd`.

## Scenario 18 — OpenAPI document parity (REQ-CYCLE-054)

- Given the published `api/crypto-collector.yaml`,
- When the doc-parity test runs,
- Then the document contains the overlay endpoint, its operationId (e.g. `listCycleOverlay`), and a
  response schema exposing both `norm_halving` and `norm_cycle_low`.

## Edge Cases

- A cycle whose only stored candle is the halving day itself ⇒ one point with `days_since_halving
  == 0`, `norm_halving == "1"`, and `norm_cycle_low == "1"` (that single day is both anchor and low).
- The in-progress cycle with a single day of data ⇒ that day is both the latest point and the
  running-min low; `norm_cycle_low == "1"` (REQ-CYCLE-034).
- A day whose `1d` candle `close` is present but `volume` is null ⇒ irrelevant to the overlay; the
  overlay uses `close` only, so the point is still produced (REQ-CYCLE-002).
- Requesting a `cycle` filter for a cycle with no stored candles (e.g. `?cycle=1`) ⇒ HTTP 200 empty
  page (REQ-CYCLE-030/052).
- All overlay values serialise as JSON strings, never JSON numbers, with no `f64` round-trip
  (REQ-CYCLE-024/051).
- The halving dates are treated as approximate/block-derived constants; a candle exactly on a
  next-halving date belongs to the next cycle (half-open boundary, REQ-CYCLE-010).

## Definition of Done

- [ ] Overlay points are derived only from persisted daily (`1d`) `coin_candles`; no Bitbo request
      and no new upstream provider — REQ-CYCLE-001/003.
- [ ] Cycles use half-open `[halving, next_halving)` windows with day 0 = halving date;
      `days_since_halving` is the whole-day offset for all series — REQ-CYCLE-010/011.
- [ ] Every point stores both baselines: `norm_halving` (anchor day = 1.0) and `norm_cycle_low`
      (cycle low = 1.0), computed from the single `close` series, plus the raw `price` —
      REQ-CYCLE-002/020/021/022.
- [ ] The cycle-low series is plotted against `days_since_halving`, not days-since-low (the
      deliberate asymmetry is preserved) — REQ-CYCLE-023.
- [ ] Incomplete/absent early cycles produce no points and are not errors; partial cycles use
      available days only; a missing halving-day anchor forward-searches to the first available day
      (flagged approximate); intra-cycle gaps are never interpolated — REQ-CYCLE-030/031/032/033.
- [ ] The in-progress cycle is open-ended (extends to the latest candle) with a provisional running
      cycle-low; recompute-driven changes to its points are not errors — REQ-CYCLE-012/034.
- [ ] A new migration creates the overlay table (Decimal columns, PK on
      `(coin_id, vs_currency, cycle_number, days_since_halving)`), applied at startup — REQ-CYCLE-040.
- [ ] The overlay is recomputed on the periodic collector tick as an idempotent, multi-replica-safe
      full rebuild — REQ-CYCLE-041/042.
- [ ] Coin, currency, and cadence are env-var-only (defaults `bitcoin`/`usd`) — REQ-CYCLE-043.
- [ ] `GET /v1/coins/{coin_id}/cycle-overlay` is keyset-paginated (opaque cursor over
      `(cycle_number, days_since_halving)`), returns both baselines as `DecimalString`, defaults
      `vs_currency` to `usd`, supports an optional `cycle` filter, returns 200 empty for non-target
      coins, and returns 400 on bad cursor/limit — REQ-CYCLE-050/051/052/053.
- [ ] The endpoint and its schema (both baselines) are in `api/crypto-collector.yaml` and verified
      by the doc-parity test — REQ-CYCLE-054.
- [ ] No `f64` used for any price or normalized value — REQ-PROV-012 / REQ-CYCLE-024.
- [ ] Quality gate green: `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D
      warnings`, `cargo test`; DB-backed scenarios verified via `DATABASE_URL=... cargo test --
      --ignored`.
