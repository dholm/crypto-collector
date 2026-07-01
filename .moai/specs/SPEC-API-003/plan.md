---
id: SPEC-API-003
type: plan
updated: 2026-07-01
---

# SPEC-API-003 — Implementation Plan

Brownfield, read-only change confined to the `GET /v1/coins/{coin_id}/candles` read path
(`src/api/candles.rs`) plus the aggregation logic it calls. No migration, no schema change, no
writes. Methodology per `quality.yaml` (brownfield: characterize the existing native path
first, then add the fallback). Commit directly to `main` (no feature branches). Quality gate
after each phase: `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D
warnings`, `cargo test`.

## Milestones (priority-ordered, no time estimates)

### Phase 1 — Characterize the existing native read (Priority High)

- Capture the current behaviour of `list_candles` (`src/api/candles.rs:46-94`) as the
  baseline to preserve: exact-interval query, keyset pagination, time-range filters, empty-page
  on miss. Native precedence (REQ-API-200) and unchanged `interval` validation (REQ-API-215)
  must survive untouched.
- Gate: existing candle handler tests continue to pass.

### Phase 2 — Interval arithmetic + source-interval discovery (Priority High)

- A pure helper mapping interval strings to fixed-second durations for the full stored
  vocabulary (Binance `1m..1M`, CoinGecko `30m/4h/4d`), returning "not a fixed duration" for
  calendar-variable units (`1M`). (REQ-API-203/204)
- Divisibility test `target_secs % source_secs == 0` and largest-divisor selection over the
  distinct `interval` strings present in `coin_candles` for the coin. (REQ-API-205)
- Mark this helper `@MX:ANCHOR` (correctness core; every aggregated response depends on it).
- Gate: `cargo test` — pure unit tests for divisibility, exclusion of `1M`, and largest-divisor
  selection against the worked examples (`4h`←`1h`; `1d`←`4h`; `1h`←`30m`).

### Phase 3 — Bucket alignment + Decimal OHLCV fold (Priority High)

- UTC/epoch-aligned truncation to the target interval; assign source candles by `ts` into
  half-open windows `[bucket_start, bucket_start + target)`. (REQ-API-208)
- Fold in `rust_decimal::Decimal`: open=first-`ts`, high=max, low=min, close=last-`ts`; volume
  = sum iff all components non-null (REQ-API-207a) else null (REQ-API-207b). (REQ-API-206/216)
- Group strictly within the resolved `vs_currency` (REQ-API-213/219).
- Mark the volume-NULL propagation `@MX:WARN`/`@MX:REASON` (easy to regress into a silent zero).
- Gate: `cargo test` — fold unit tests including the volume-null-propagation and Decimal-string
  cases.

### Phase 4 — Partial-bucket policy (Priority High)

- Complete-bucket predicate: `N = target_secs / source_secs` distinct source candles present
  (relies on precondition P1: source `ts` epoch-aligned to their own interval).
- Classify buckets by wall clock: the forming bucket (window contains `now()`) is emitted even
  if incomplete (REQ-API-210); every closed incomplete bucket (`bucket_start + target <= now()`)
  is dropped (REQ-API-209); never interpolate (REQ-API-211). Classification MUST use `now()`,
  not the newest row in the page — a `cursor`/`end` bound would otherwise mislabel a closed
  bucket as forming.
- Mark the wall-clock forming-vs-closed boundary `@MX:WARN`/`@MX:REASON` (cursor-independence is
  a correctness invariant, OR-API3-3).
- Gate: `cargo test` — interior-gap-drop, forming-emitted, and a paging-across-the-boundary test
  (a closed incomplete bucket on an older page must be dropped).

### Phase 5 — `vs_currency` parameter + native read filter (Priority High)

- Add an optional `vs_currency` field to `ListCandlesParams`; resolve to `usd` when omitted,
  mirroring `src/api/coin_market.rs:51,86` (`.unwrap_or("usd")`). (REQ-API-217)
- Add a `vs_currency = $` predicate to the native exact-interval query WHERE clause
  (`src/api/candles.rs:69-87`) so the native read returns only the resolved currency's candles.
  (REQ-API-218)
- Thread the resolved currency into source discovery and folding so aggregation is scoped to one
  currency. (REQ-API-219)
- Gate: `cargo test` — handler tests for explicit-currency filtering and the `usd` default.

### Phase 6 — Wire the fallback into `list_candles` (Priority High)

- After the exact-interval query (now currency-filtered) returns no rows for the coin, run source
  discovery; if a divisor is found, fold and return; if none, return the current empty page.
  (REQ-API-201/202)
- Preserve keyset pagination, `start`/`end`, and `ORDER BY ts DESC` over the aggregated
  `ts` (= `bucket_start`); reuse the existing `TsKey`/`paginate_ts` helpers so aggregated pages
  behave identically to native pages. (REQ-API-214)
- Set `source = aggregated:<source_interval>` on aggregated `CoinCandle`s before mapping to
  `CoinCandleDto`. (REQ-API-212)
- Mark the native-vs-aggregate branch `@MX:NOTE` (exact-interval first, aggregation is fallback
  only).
- Gate: `cargo test` — handler tests for native precedence, aggregation, empty-page fallback.

### Phase 7 — Full suite + DB-backed scenarios (Priority Medium)

- Full `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo test`.
- DB integration coverage for the aggregation scenarios opt-in via `DATABASE_URL=... cargo test
  -- --ignored` (the fold and fixtures need real `coin_candles` rows across intervals and
  currencies).

## Technical Approach Notes

- **Read-only, on demand.** Aggregation computes per request from existing `coin_candles` rows;
  nothing is written and no aggregated row is cached (Exclusions). This keeps the change a pure
  addition to the read path.
- **Source query.** The fold needs the finer source candles ordered by `ts` for the coin, the
  chosen source interval, and the requested time range — a superset of the requested target
  window sufficient to build the boundary buckets. Whether this read lives in a new `src/db/`
  candle-read function or inline in `src/api/candles.rs` is open (OR-API3-4); there is no
  dedicated candle-read function in `src/db/` today.
- **Largest-divisor selection (OR-API3-1 resolved).** Enumerate distinct stored `interval`
  strings for the coin, map to seconds, keep those dividing the target, take the maximum. For a
  complete bucket the divisor choice does not change the OHLC result at all; it only affects
  incomplete buckets, where the largest divisor needs the fewest source candles and therefore
  drops the fewest interior buckets — so largest divisor is strictly preferable (REQ-API-205).
- **Bucket completeness (precondition P1).** A bucket is complete when it holds `N = target_secs
  / source_secs` distinct source `ts`. The `coin_candles` PK `(coin_id, vs_currency, interval,
  ts)` guarantees distinct source `ts`, so a simple count equals the completeness check —
  **provided** the source `ts` are epoch-aligned to their own interval (P1, stated in spec.md).
  Verify P1 against real rows at run; if a provider emitted unaligned `ts`, the count-based
  completeness check would need revisiting.
- **Forming bucket (OR-API3-3 resolved).** Defined by wall clock as the bucket whose window
  contains `now()` (`bucket_start <= now() < bucket_start + target`); emitted even when
  incomplete. This is cursor-independent: a `cursor`/`end` bound never turns a closed incomplete
  bucket into a forming one, so closed incomplete buckets are always dropped (REQ-API-209/210).
- **Currency isolation (OR-API3-5 resolved).** The endpoint adds an optional `vs_currency`
  parameter (default `usd`, `.unwrap_or("usd")` convention — no `DEFAULT_VS_CURRENCY` constant,
  OR-API3-5a). Both the native WHERE clause and the aggregation source read filter by the
  resolved currency, so the fold never mixes currencies within a bucket (REQ-API-217/218/219).
- **Decimal only.** `CoinCandle.open/high/low/close` are `Decimal` and `volume` is
  `Option<Decimal>` (`src/models/quote.rs:33-46`); the fold stays in Decimal and serialises via
  `CoinCandleDto`'s `rust_decimal::serde::str` / `str_option` (`src/api/dto.rs:166-183`). No
  `f64` at any point (REQ-PROV-012, REQ-API-216).

## Risk Analysis

- **Volume-NULL regression.** Summing volume across components with a Rust `Option` fold can
  silently coerce a missing component to zero; the requirement is null-if-any-null. Covered by a
  dedicated unit test and an `@MX:WARN`.
- **Pagination over computed rows.** Keyset semantics must hold over aggregated `ts` the same as
  native rows; a naive per-page recomputation could double-count or skip a boundary bucket. The
  source read must span enough range to build the buckets adjacent to the cursor.
- **Bucket alignment / week anchor (OR-API3-6).** Epoch-aligned `1w` buckets anchor to Thursday,
  not ISO Monday; confirm the desired anchor for `1w` targets.
- **Trigger scope (OR-API3-2).** If the native-vs-aggregate decision is derived from the first
  page's emptiness rather than an existence probe, a deep cursor could wrongly flip a native
  request into aggregation. Recommend a coin+interval+`vs_currency` `EXISTS` probe.
- **Performance.** Aggregating a long range from a fine source (e.g. `1w` from `30m` = 336
  candles/bucket) reads many rows; the existing `(coin_id, vs_currency, interval, ts DESC)`
  btree and `limit`-bounded pagination keep this bounded, but source-read row counts should be
  capped consistent with the page `limit`.

## Dependencies / Sequencing

- Phase 1 (characterize) precedes all changes to keep native behaviour provably intact.
- Phases 2–4 (arithmetic, fold, partial policy) are independently unit-testable and can land
  before the handler wiring.
- Phase 5 (`vs_currency` param + native filter) and Phases 2–4 both feed Phase 6 (handler
  wiring), which threads the resolved currency into the aggregation path. Phase 7 (full suite +
  DB tests) closes the loop.
- No dependency on other SPECs beyond the existing `coin_candles` schema (SPEC-DB-001) and the
  SPEC-API-002 read path this extends.
