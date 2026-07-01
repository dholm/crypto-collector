---
id: SPEC-API-003
type: acceptance
updated: 2026-07-01
---

# SPEC-API-003 — Acceptance Criteria

Given/When/Then scenarios for the coin-candle interval aggregation fallback. Each maps to one
or more `REQ-API-2NN`. OHLCV fields are asserted as JSON strings (DecimalString, REQ-API-216).
Unless stated, the service is running against a database with `coin_candles` populated as
described, and every request targets `GET /v1/coins/{coin_id}/candles`.

## Scenario 1 — Native precedence: exact interval served without aggregation (REQ-API-200, 215)

- Given `bitcoin` is registered and `coin_candles` holds native rows at `interval=1h` with
  `source="binance"`,
- When the client requests `?interval=1h`,
- Then the response is 200 and each item has `interval=="1h"` and `source=="binance"` (the
  provider source, unchanged),
- And no item has a `source` beginning with `aggregated:` (no aggregation was performed).

## Scenario 2 — Aggregate a target from a finer divisor with correct OHLC fold (REQ-API-201, 205, 206, 208, 212)

- Given `bitcoin` has no native `4h` candles but has native `1h` candles, and four consecutive
  `1h` candles fall in one epoch-aligned `4h` bucket with opens/highs/lows/closes such that the
  first candle open is `100`, the maximum high across the four is `130`, the minimum low is `95`,
  and the last candle close is `120`,
- When the client requests `?interval=4h`,
- Then the response is 200 and the aggregated `4h` candle for that bucket has `open=="100"`,
  `high=="130"`, `low=="95"`, `close=="120"`,
- And its `ts` equals the `4h` bucket start (UTC/epoch-aligned),
- And its `source=="aggregated:1h"`.

## Scenario 3 — Largest divisor selected when several divisors are stored (REQ-API-205)

- Given a CoinGecko-only `dogecoin` whose `coin_candles` hold stored intervals `30m`, `4h`, and
  `4d` (per CoinGecko snapping) and no native `1d` candles,
- When the client requests `?interval=1d`,
- Then the aggregated candles carry `source=="aggregated:4h"` (the largest stored divisor of
  `1d`: 6×4h), not `aggregated:30m`.

## Scenario 4 — Non-API stored interval used as source (REQ-API-204, 205, 212)

- Given the same `dogecoin` (stored `30m/4h/4d`) with no native `1h` candles,
- When the client requests `?interval=1h`,
- Then the aggregated candles carry `source=="aggregated:30m"` (2×30m = 1h), demonstrating that
  a stored interval outside the API-supported set is a valid aggregation source.

## Scenario 5 — Volume sums only when every component has volume (REQ-API-207a)

- Given `bitcoin` has no native `4h` candles and its `1h` source candles all have non-null
  `volume` within a complete `4h` bucket whose component volumes sum to `1500`,
- When the client requests `?interval=4h`,
- Then that aggregated `4h` candle has `volume=="1500"`.

## Scenario 6 — Volume is null when any component volume is null (REQ-API-207b)

- Given `dogecoin` aggregating `1h` from stored `30m` candles where at least one `30m` source
  candle in a bucket has `volume == null` (CoinGecko-sourced),
- When the client requests `?interval=1h`,
- Then that aggregated `1h` candle has `volume == null` (the total is unknown; components are
  not summed).

## Scenario 7 — Closed interior gap drops the bucket, never interpolates (REQ-API-209, 211)

- Given `bitcoin` aggregating `4h` from native `1h` candles, where one `4h` bucket whose window
  has already closed (`bucket_start + 4h <= now()`) is missing one of its four `1h` source
  candles while the closed buckets before and after it are complete,
- When the client requests `?interval=4h`,
- Then the response omits the incomplete closed bucket entirely (its `ts` is absent),
- And the complete neighbouring buckets are present,
- And no fabricated or interpolated candle appears in place of the dropped bucket.

## Scenario 8 — Wall-clock forming bucket emitted incomplete (REQ-API-210)

- Given `bitcoin` aggregating `4h` from native `1h` candles, where the `4h` bucket whose window
  contains the current time (`bucket_start <= now() < bucket_start + 4h`) currently holds only
  two of its four `1h` source candles (the interval is still forming),
- When the client requests `?interval=4h`,
- Then the response includes that forming bucket as a candle folded from the two available `1h`
  candles (open of the first, close of the latest available, high/low over the two),
- And it carries `source=="aggregated:1h"`.

## Scenario 8b — Closed incomplete bucket is dropped even when it is the newest row on a page (REQ-API-209, 210, 214)

- Given `bitcoin` aggregating `4h` from native `1h` candles, with a forming bucket (window
  contains `now()`) plus several older complete `4h` buckets and one older `4h` bucket whose
  window has closed but is incomplete,
- When the client paginates with a `cursor` that lands strictly before the forming bucket (so the
  forming bucket is not on this page) and the closed incomplete bucket is the newest row of this
  older page,
- Then no incomplete bucket is emitted on the older page — the closed incomplete bucket is
  dropped (REQ-API-209), not mislabelled as forming,
- And the forming bucket appears only on the first/newest page (REQ-API-210).

## Scenario 9 — No divisor stored yields an empty page, not an error (REQ-API-202)

- Given `dogecoin` stores only `4h` and `4d` candles and the client requests a target that no
  stored interval divides,
- When the client requests `?interval=1h` (neither `4h` nor `4d` divides `1h`: `3600 % 14400 !=
  0`),
- Then the response is HTTP 200 with body `{"items": [], "next_cursor": null}` (not 404, not an
  error).

## Scenario 10 — Divisibility is decided purely by seconds modulo (REQ-API-203)

- Given `litecoin` stores only `30m` candles and no native `1w` candles,
- When the client requests `?interval=1w`,
- Then aggregation proceeds because `30m` divides `1w` (`604800 % 1800 == 0`, 336 buckets of
  source candles), and the aggregated candles carry `source=="aggregated:30m"`,
- And Given a separate `litecoin2` that stores only `4h` candles, When the client requests
  `?interval=1h`, Then the response is an empty 200 page because `4h` does not divide `1h`
  (`3600 % 14400 != 0`).

## Scenario 11 — Keyset pagination works identically over aggregated results (REQ-API-214)

- Given `bitcoin` aggregating `4h` from native `1h` candles yields at least 3 complete `4h`
  buckets ordered `ts` DESC,
- When the client requests `?interval=4h&limit=2`,
- Then the response returns 2 aggregated items (newest first) and a non-null `next_cursor`,
- And When the client requests `?interval=4h&limit=2&cursor=<next_cursor>`,
- Then the response returns the remaining aggregated item(s) with a null `next_cursor` when
  exhausted,
- And an undecodable `cursor` on the aggregated path returns 400.

## Scenario 12 — Aggregation respects the vs_currency boundary (REQ-API-213, 219)

- Given `bitcoin` has no native `4h` candles but has native `1h` candles in both `vs_currency=usd`
  and `vs_currency=eur` for the same time window,
- When the client requests `?interval=4h&vs_currency=usd`,
- Then every returned aggregated `4h` candle is composed only from `usd` source candles (no `eur`
  source candle is folded into any bucket), and no `eur`-derived candle appears in the response.

## Scenario 13 — Target interval validation is unchanged (REQ-API-215)

- Given any coin,
- When the client requests `?interval=2h` (not in `SUPPORTED_INTERVALS`) or omits `interval`,
- Then the response is 400 without querying and without attempting aggregation (unchanged from
  SPEC-API-002 REQ-API-131).

## Scenario 14 — Explicit vs_currency filters both native and aggregated reads (REQ-API-217, 218, 219)

- Given `bitcoin` has native `1h` candles in both `vs_currency=usd` and `vs_currency=eur`,
- When the client requests `?interval=1h&vs_currency=eur` (native path — exact interval stored),
- Then every returned candle has `vs_currency=="eur"` and no `usd` candle appears (REQ-API-218),
- And Given `bitcoin` has no native `4h` candles but has `1h` candles in both currencies,
- When the client requests `?interval=4h&vs_currency=eur` (aggregated path),
- Then every returned aggregated candle is folded solely from `eur` `1h` source candles and
  carries `vs_currency=="eur"` (REQ-API-219).

## Scenario 15 — Omitted vs_currency defaults to usd (REQ-API-217)

- Given `bitcoin` has native `1h` candles in both `vs_currency=usd` and `vs_currency=eur`,
- When the client requests `?interval=1h` with no `vs_currency` query parameter,
- Then the response contains only `usd` candles (the effective currency defaults to `usd`,
  matching the `.unwrap_or("usd")` convention at `src/api/coin_market.rs:51,86`),
- And the same default applies on the aggregated path when the exact interval is absent.

## Scenario 16 — start/end time-range filtering over aggregated results (REQ-API-214)

- Given `bitcoin` aggregating `4h` from native `1h` candles produces closed complete `4h` buckets
  at aligned times `T0 < T1 < T2 < T3` (each `T` a `4h` bucket start),
- When the client requests `?interval=4h&start=<T1>&end=<T3>`,
- Then the response contains only aggregated buckets whose `ts` falls in the inclusive range
  `[start, end]` (`T1` and `T2`, matching the native path's `ts >= start` / `ts <= end`
  semantics), and excludes `T0` (before `start`) and any bucket after `T3` (after the
  inclusive `end`),
- And the results remain ordered `ts` DESC with a `next_cursor` that is null once the in-range
  buckets are exhausted.

## Edge Cases

- Exact-interval rows present but the requested page is empty because the `cursor` is past all
  rows ⇒ this is still the native path (REQ-API-200); aggregation is not triggered by an
  empty page, only by the coin having no native candle at the exact interval (see OR-API3-2).
- Target with multiple stored divisors including one outside the API set (e.g. stored `30m` and
  `1h`, target `4h`) ⇒ largest divisor `1h` chosen ⇒ `source=="aggregated:1h"` (REQ-API-205).
- A stored `1M` (calendar month) interval is never selected as a source (non-fixed duration),
  even if present (REQ-API-204).
- `limit` out of range (0 or above the documented maximum) on the aggregated path ⇒ 400
  (REQ-API-214).
- `start`/`end` filters restrict which aggregated bucket `ts` values are returned, identically to
  the native path (REQ-API-214).
- Omitting `vs_currency` on either the native or aggregated path defaults the effective currency
  to `usd` (REQ-API-217).
- An unrecognised/unsupported `vs_currency` (e.g. `?vs_currency=xyz`) is NOT rejected with 400
  (unlike an invalid `interval`); it matches no stored rows and returns a 200 empty page
  `{"items": [], "next_cursor": null}` (REQ-API-217, consistent with REQ-API-202).
- All aggregated OHLCV values serialise as JSON strings, never as JSON numbers, with no `f64`
  round-trip (REQ-API-216).

## Definition of Done

- [ ] `GET /v1/coins/{coin_id}/candles` serves native candles unchanged when the exact interval
      is stored (no `aggregated:` source appears) — REQ-API-200.
- [ ] Aggregation engages only on an exact-interval miss and composes the target from the
      largest stored divisor discovered over the coin's actual stored interval strings —
      REQ-API-201/204/205.
- [ ] Divisibility is `target_secs % source_secs == 0`; non-fixed-duration intervals (`1M`) are
      excluded as sources — REQ-API-203/204.
- [ ] OHLCV fold is open=first, high=max, low=min, close=last in Decimal; volume sums only when
      all components are non-null (REQ-API-207a), else null (REQ-API-207b) — REQ-API-206.
- [ ] Buckets are UTC/epoch-aligned (precondition P1); only the wall-clock forming bucket (window
      contains `now()`) may be emitted incomplete, every closed incomplete bucket is dropped
      regardless of page, and nothing is interpolated — REQ-API-208/209/210/211.
- [ ] Aggregated candles carry `source == "aggregated:<source_interval>"`; native candles keep
      their provider source — REQ-API-212.
- [ ] Optional `vs_currency` parameter (default `usd`) filters both the native exact-interval
      read and the aggregated path; aggregation never folds across `vs_currency` —
      REQ-API-213/217/218/219.
- [ ] No divisor stored ⇒ HTTP 200 empty page `{"items": [], "next_cursor": null}` (no 404/error)
      — REQ-API-202.
- [ ] Keyset pagination (`cursor`/`limit`/`start`/`end`, `ORDER BY ts DESC`) and DecimalString
      serialisation behave identically over aggregated results; `interval` validation unchanged —
      REQ-API-214/215/216.
- [ ] Quality gate green: `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D
      warnings`, `cargo test`.
- [ ] No `f64` used for any OHLCV value (REQ-PROV-012 / REQ-API-216).
- [ ] DB-backed scenarios verified via `DATABASE_URL=... cargo test -- --ignored`.
