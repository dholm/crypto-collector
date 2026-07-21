---
id: SPEC-API-003
version: 0.3.0
status: completed
created: 2026-07-01
updated: 2026-07-21
author: dholm
priority: medium
issue_number: 0
---

# SPEC-API-003 — Coin-Candle Interval Aggregation Fallback

Brownfield evolution of the coin-keyed OHLCV read endpoint defined in
[SPEC-API-002](../SPEC-API-002/spec.md) (Module 4, `GET /v1/coins/{coin_id}/candles`,
handler `list_candles` at `src/api/candles.rs:46-94`). Today, when the database holds no
natively-collected candles at the exact requested `interval`, the endpoint returns an empty
page (`src/api/candles.rs:89-93`). This SPEC adds a **read-time aggregation fallback**: when
no native candles exist at the exact interval, the API composes coarser OHLCV buckets from the
finer-grained candles already stored, rather than returning nothing.

Schema/data contract base: [SPEC-DB-001](../SPEC-DB-001/spec.md) (`coin_candles`,
`migrations/0011_remove_markets.sql:107-128`). Provider snapping (which determines the
interval strings actually present in the DB): [SPEC-PROV-001](../SPEC-PROV-001/spec.md)
(`src/providers/binance.rs:166-189`, `src/providers/coingecko.rs:546-553`). Keyset pagination,
DecimalString serialisation, and the Decimal-not-`f64` invariant carry over unchanged from
SPEC-API-001/002 and REQ-PROV-012.

## HISTORY

- 2026-07-01 (v0.3.0): plan-auditor pass (PASS 0.90) fixes applied. (D4) OR-API3-3 RESOLVED —
  the forming/trailing bucket is now defined by **wall clock** (`bucket_start <= now() <
  bucket_start + target_interval`), cursor-independent; any closed incomplete bucket
  (`bucket_start + target_interval <= now()`) is dropped as an interior gap regardless of page
  (REQ-API-209/210). (D6) REQ-API-217 now states an unrecognised `vs_currency` is not a 400 — it
  matches no rows and yields a 200 empty page. (D3) REQ-API-207 split into REQ-API-207a
  (State-Driven, sum when all volumes present) and REQ-API-207b (Unwanted, null if any absent).
  (D2) REQ-API-211 and REQ-API-213 relabelled Ubiquitous (they are prohibitions with no
  `If`-trigger). (D7) added precondition P1 (source `ts` epoch-aligned) referenced by
  REQ-API-208/209. (D1) requirement definitions reordered so IDs ascend monotonically
  (214/215/216 before 217/218/219). (D5) added a `start`/`end` aggregated-range acceptance
  scenario and a paging-across-the-forming-boundary scenario.
- 2026-07-01 (v0.2.0): Two decisions applied. (1) OR-API3-1 RESOLVED as **largest divisor** —
  the internal "finest-granularity / maximizes fidelity" wording that contradicted the worked
  examples is removed; the document now reads largest-divisor throughout (REQ-API-205), with the
  authoritative rationale that source granularity does not change a *complete* bucket's OHLC at
  all and only affects incomplete buckets (larger divisor ⇒ fewer interior drops ⇒ more of the
  series returned). (2) Explicit `vs_currency` filtering added to the endpoint (OR-API3-5
  RESOLVED): optional `vs_currency` query parameter defaulting to `usd`, applied to both the
  native exact-interval read and the aggregated path — new REQ-API-217/218/219.
- 2026-07-01 (v0.1.0): Initial draft. Single module: read-time interval aggregation fallback
  for `GET /v1/coins/{coin_id}/candles`. Native candles at the exact interval are served
  unchanged (no aggregation); only an exact-interval miss triggers aggregation from the
  finer-grained stored candles that evenly divide the target. Fold rule: open=first, high=max,
  low=min, close=last, volume=sum (NULL if any component NULL). Interior buckets with missing
  source candles are dropped; the trailing (most-recent) bucket is emitted as a forming candle.
  Aggregated rows are labelled `source = aggregated:<source_interval>`. No compatible source ⇒
  today's empty-page behaviour (200) is preserved. New `REQ-API-2NN` range; SPEC-API-001
  retains `REQ-API-0NN`, SPEC-API-002 retains `REQ-API-1NN`.

---

## Goal

A client requesting OHLCV candles for a coin at an `interval` that the collector does not
natively store should still receive candles at that interval — derived on the fly from the
finer-grained candles already in `coin_candles` — instead of an empty page. Native data is
always preferred and served unchanged; aggregation is a pure read-time fallback that never
fabricates, never interpolates, and clearly labels its output as computed
(`source = aggregated:<source_interval>`) so clients can distinguish derived candles from
provider-collected ones. When no stored interval can compose the target, the endpoint's
current empty-page behaviour is preserved (non-breaking).

## Scope

In scope:
- **Read-time aggregation** inside the `GET /v1/coins/{coin_id}/candles` read path
  (`src/api/candles.rs`): triggered only when no native candle exists at the exact requested
  interval for the coin.
- **Source-interval discovery** over the interval strings actually present in `coin_candles`
  for the coin (which, per provider snapping, may be values outside the API-supported set).
- **OHLCV bucket folding** in `rust_decimal::Decimal` arithmetic (open=first, high=max,
  low=min, close=last, volume=sum-or-NULL), UTC/epoch-aligned to the target interval.
- **Partial-bucket policy**: emit only the wall-clock forming bucket (the bucket whose window
  contains `now()`) when incomplete; drop every closed incomplete bucket as an interior gap.
- **Response labelling** of aggregated candles via the existing `source` field
  (`aggregated:<source_interval>`).
- **Explicit `vs_currency` filtering**: an optional `vs_currency` query parameter (default
  `usd`) applied to both the native exact-interval read and the aggregated path, closing the
  gap that today's handler has no `vs_currency` filter at all (`src/api/candles.rs:33-41,69-87`).
- **Preservation** of keyset pagination (`cursor`, `limit`, `start`, `end`, `ORDER BY ts
  DESC`), the DecimalString contract, and the `interval` validation already in place
  (`SUPPORTED_INTERVALS`, `validate_interval`, `src/api/candles.rs:29,99-108`).

Out of scope: see Exclusions. This SPEC does not add native collection of new intervals, does
not change the provider chain, does not change what is written to `coin_candles`, and does not
alter the quote endpoints or WebSocket streams.

## Decisions Restated (authoritative)

Confirmed with the user; encoded here verbatim in intent.

- **D1 — Source-interval selection = largest stored divisor.** On an exact-interval miss, the
  system aggregates from the stored interval that evenly divides the target and is **closest to
  the target** (the largest such divisor, i.e. the fewest source candles per bucket). Search
  candidate divisors from the target downward and take the first one that is stored.
  - Worked examples (all name the largest divisor): to build `4h` — prefer stored `1h`, else
    `15m`, else `5m`, else `1m`; to build `1d` on a CoinGecko-only coin storing `30m/4h/4d` —
    prefer `4h` (6×4h = 1d); to build `1h` on that coin — use `30m` (2×30m = 1h).
  - Rationale (authoritative): for a **complete** bucket, source granularity does not change the
    OHLC result at all — aggregating `4h` from `1h` yields identical open/high/low/close as
    aggregating it from `1m`, because each finer candle's extremes are already folded into the
    coarser one. The choice of divisor therefore matters **only** for incomplete buckets: under
    D3 (drop incomplete interior buckets), the largest divisor needs the fewest source candles
    per bucket, so it produces the fewest interior drops and returns more of the series. Largest
    divisor is thus strictly preferable (never worse on complete buckets, better on incomplete
    ones). (OR-API3-1 is RESOLVED to largest divisor on this basis.)
- **D2 — Divisibility is defined purely on seconds.** A stored source interval divides the
  target if and only if `target_seconds % source_seconds == 0`. Intervals whose duration is not
  a fixed number of seconds (e.g. `1M` calendar month) are never eligible sources; in practice
  no interval coarser than the target (`3d`, `4d`, `1M`) can divide any target `<= 1w` anyway.
- **D3 — Partial/incomplete bucket handling.** The **forming** bucket — the one whose window
  contains the current time (`bucket_start <= now() < bucket_start + target_interval`) — is
  emitted as a live forming candle built from whatever source candles exist so far. Every
  **closed** bucket (`bucket_start + target_interval <= now()`) that is missing any expected
  source candle is **dropped** — never fabricated or interpolated. This wall-clock definition is
  cursor-independent (D4, OR-API3-3) and matches how exchanges present a forming candle.
- **D4 — Native precedence.** If candles are stored at the exact requested interval, they are
  served directly with no aggregation (unchanged current behaviour). Aggregation is a fallback
  triggered only when the exact-interval read yields no rows.
- **D5 — Fallback on impossibility is the current empty page.** If no stored interval divides
  the target evenly, the endpoint returns HTTP 200 with `{"items": [], "next_cursor": null}`,
  exactly as today. No 404, no error is introduced for this case (non-breaking).
- **D6 — Response labelling.** Aggregated candles set `source = aggregated:<source_interval>`
  (e.g. `aggregated:1h`, `aggregated:30m`). Natively-collected candles keep their provider
  `source` unchanged.

---

## Change Surface (brownfield delta markers)

File-level scope only; exact functions/algorithms are deferred to `plan.md`/Run.

[MODIFY]
- `src/api/candles.rs` — the `list_candles` handler (`:46-94`) gains the aggregation fallback:
  after the exact-interval read yields nothing, discover a source interval and fold. The
  inline exact-interval query and `validate_interval` (`:99-108`) are otherwise unchanged.

[NEW]
- Aggregation logic (source-interval discovery, bucket alignment, Decimal OHLCV fold, partial-
  bucket policy). Placement (a candle-read module under `src/db/` versus helpers within
  `src/api/candles.rs`) is a Run-phase decision; note there is currently no dedicated candle-
  read function in `src/db/` (OR-API3-4).

[UNCHANGED]
- `coin_candles` schema, `CoinCandle` (`src/models/quote.rs:33-46`), `CoinCandleDto`
  (`src/api/dto.rs:166-183`), the keyset cursor helpers, and `SUPPORTED_INTERVALS`. No
  migration is required — aggregation is read-only over existing rows.

---

## Design Summary (WHAT, not HOW)

### Trigger and precedence

The read path first performs the existing exact-interval query. If it returns at least one
row, the response is served natively and unchanged (D4). If it returns no rows for the coin at
the exact interval, the aggregation fallback engages. The native-vs-aggregate decision is a
property of "does the coin have any native candle at this exact interval," so it is stable
across pagination pages for the same request (OR-API3-2 refines the probe's scope).

### Source-interval discovery

The system enumerates the distinct `interval` strings present in `coin_candles` for the coin
(these come from provider snapping and may lie outside the API set — Binance can emit
`1m,3m,5m,15m,30m,1h,2h,4h,6h,8h,12h,1d,3d,1w,1M`; CoinGecko emits only `30m,4h,4d`). Each
candidate is mapped to a duration in seconds and tested against the target with the D2
divisibility rule. Among the divisors that are stored, the **largest** (closest to the target)
is chosen (D1). If none divides the target, the fallback yields an empty page (D5).

Canonical interval → seconds mapping used for divisibility:

| interval | 1m | 3m | 5m | 15m | 30m | 1h | 2h | 4h | 6h | 8h | 12h | 1d | 3d | 4d | 1w |
|----------|----|----|----|-----|-----|----|----|-----|-----|-----|------|------|------|------|------|
| seconds | 60 | 180 | 300 | 900 | 1800 | 3600 | 7200 | 14400 | 21600 | 28800 | 43200 | 86400 | 259200 | 345600 | 604800 |

`1M` (and any calendar-variable unit) has no fixed second-count and is excluded as a source.

### Bucket alignment and folding

Aggregated bucket boundaries are UTC/epoch-aligned truncations to the target interval. Each
bucket `[bucket_start, bucket_start + target_interval)` groups the source candles whose `ts`
falls in that half-open window (source `ts` is the candle open time). Within a bucket, source
candles are folded in Decimal arithmetic:

- `open`  = the open of the earliest-`ts` source candle in the bucket
- `high`  = the maximum `high` across the bucket's source candles
- `low`   = the minimum `low` across the bucket's source candles
- `close` = the close of the latest-`ts` source candle in the bucket
- `volume` = the sum of the source candles' `volume` **iff every** source candle in the bucket
  has a non-null volume; if **any** source candle has a null volume, the aggregated bucket's
  `volume` is null (the total is unknown). Source-collected candles with null volume come from
  CoinGecko (`CoinCandle.volume: Option<Decimal>`).

An aggregated bucket's `ts` is `bucket_start`; its `interval` is the requested target; its
`vs_currency` is the resolved request currency shared by all its source candles
(REQ-API-213/219); its `source` is `aggregated:<source_interval>`.

### Partial and gapped buckets

A complete bucket contains exactly `N = target_seconds / source_seconds` distinct source
candles (relying on precondition P1, below). Buckets are classified by wall clock, not by
position within a page:

- The **forming bucket** is the single bucket whose window contains the current time
  (`bucket_start <= now() < bucket_start + target_interval`). It is always emitted, even if it
  holds fewer than `N` source candles — this is the live forming candle. Its identity does not
  depend on `cursor`/`limit`/`start`/`end`.
- A **closed bucket** is any bucket whose window has already elapsed
  (`bucket_start + target_interval <= now()`). A closed bucket is emitted only if complete; a
  closed bucket missing any expected source candle is an interior gap and is dropped. The system
  never invents or interpolates the missing candle (D3).

The wall-clock definition (D4, OR-API3-3 RESOLVED) is deliberately cursor-independent: under
keyset pagination a `cursor`/`end` bound imposes an upper `ts` limit, so the newest in-range
bucket on an older page could be a closed incomplete bucket — that bucket must still be dropped
(REQ-API-209), not mislabelled as forming. The forming bucket therefore appears only on the
first/newest page.

### Preconditions

- **P1 — source-candle epoch alignment.** Stored source-candle `ts` values are epoch-aligned to
  their own interval (provider klines are aligned in practice; SPEC-PROV-001 snapping). The
  completeness count (`N` distinct source candles per target window, REQ-API-208/209) relies on
  this: if a source series were not interval-aligned, "N distinct `ts` in the window" would not
  correspond to a full bucket. Verify at run against real `coin_candles` rows.

### Currency isolation and the `vs_currency` parameter

The endpoint accepts an optional `vs_currency` query parameter. When omitted, the effective
currency resolves to `usd` — the pervasive convention in this codebase (e.g. the coin-market
reads use `.unwrap_or("usd")` at `src/api/coin_market.rs:51,86`, and every collector writes
`vs_currency: "usd"`); there is no `DEFAULT_VS_CURRENCY` config constant (OR-API3-5a).

Both paths filter to the resolved currency. The native exact-interval read gains a
`vs_currency = $` predicate in its WHERE clause (today's handler has no `vs_currency` field on
`ListCandlesParams` and its query omits the column entirely —
`src/api/candles.rs:33-41,69-87`). The aggregation fallback scopes both source-interval
discovery and bucket grouping to the resolved currency, so a single aggregated bucket is
composed only from source candles of that one `vs_currency` (folding USD and EUR candles into
one OHLCV bucket would be meaningless). The explicit filter makes REQ-API-213 enforceable by
construction rather than relying on an implicit single-currency assumption.

### Pagination and serialisation

Keyset pagination (`cursor` decoding to a `ts` key, `limit`, `start`, `end`, `ORDER BY ts
DESC`) behaves identically over aggregated results as over native rows: the aggregated
candles' `ts` (= `bucket_start`) are the ordering key, and `next_cursor` is null when
exhausted. All OHLCV values remain `rust_decimal::Decimal` end to end (no `f64`) and serialise
as JSON strings via `CoinCandleDto` (REQ-PROV-012, REQ-API-152).

---

## Requirements (EARS)

### Trigger and precedence

- **REQ-API-200** (State-Driven): While the database holds one or more native candles at the
  exact requested `interval` for the coin, the system shall serve those candles directly with
  no aggregation and shall leave their `source` field unchanged (D4).
- **REQ-API-201** (Event-Driven): When a client requests `GET /v1/coins/{coin_id}/candles` and
  no native candle exists at the exact requested `interval` for the coin, the system shall
  attempt to aggregate the requested interval from finer-grained stored candles before
  responding.
- **REQ-API-202** (If/Unwanted): If no stored interval evenly divides the requested target
  interval, then the system shall respond HTTP 200 with an empty page
  (`{"items": [], "next_cursor": null}`) and shall not return 404 or any error for this case
  (D5).

### Source-interval discovery

- **REQ-API-203** (Ubiquitous): The system shall treat a stored interval as a valid
  aggregation source for a target if and only if `target_seconds % source_seconds == 0`, where
  both durations are the fixed-second values of the interval strings (D2).
- **REQ-API-204** (Ubiquitous): The system shall discover candidate source intervals from the
  distinct `interval` strings actually present in `coin_candles` for the coin — including
  interval strings outside the API-supported set (e.g. `30m`, `2h`, `4d`) produced by provider
  snapping — and shall exclude any interval whose duration is not a fixed number of seconds
  (e.g. `1M`).
- **REQ-API-205** (Event-Driven): When more than one stored interval evenly divides the target,
  the system shall select the largest such divisor (the stored interval closest to the target),
  searching candidate divisors from the target downward (D1). Example: for target `1d` on a
  coin storing `30m/4h/4d`, the system shall aggregate from `4h`.

### OHLCV folding

- **REQ-API-206** (Ubiquitous): For each aggregated bucket the system shall compute `open` from
  the earliest-`ts` source candle's open, `high` as the maximum source `high`, `low` as the
  minimum source `low`, and `close` from the latest-`ts` source candle's close, all in
  `rust_decimal::Decimal` arithmetic (D6, REQ-PROV-012).
- **REQ-API-207a** (State-Driven): While every source candle in an aggregated bucket has a
  non-null `volume`, the system shall set the bucket's `volume` to the Decimal sum of those
  component volumes.
- **REQ-API-207b** (Unwanted): If any source candle in an aggregated bucket has a null `volume`,
  then the system shall set the bucket's `volume` to null (the total is unknown; components are
  not partially summed).
- **REQ-API-208** (Ubiquitous): The system shall align aggregated bucket boundaries to
  UTC/epoch truncation of the target interval and shall assign each source candle to the bucket
  whose half-open window `[bucket_start, bucket_start + target_interval)` contains the source
  candle's `ts`; the aggregated candle's `ts` shall be `bucket_start`. This relies on the stated
  precondition (P1) that stored source-candle `ts` values are epoch-aligned to their own
  interval, as provider klines are in practice.

### Partial and gapped buckets

- **REQ-API-209** (State-Driven): While a **closed** bucket — one whose window has already
  elapsed in wall-clock terms, i.e. `bucket_start + target_interval <= now()` — is missing any of
  its `N = target_seconds / source_seconds` expected source candles (counted assuming the
  precondition P1 that source `ts` are epoch-aligned to their own interval), the system shall
  drop that bucket from the response. This holds regardless of pagination: a closed incomplete
  bucket is dropped even when a `cursor`/`end` bound makes it the newest row on a given page.
- **REQ-API-210** (State-Driven): While a bucket is the **forming** bucket — the single bucket
  whose window contains the current time, i.e. `bucket_start <= now() < bucket_start +
  target_interval` — the system shall emit it as a live forming candle built from whatever source
  candles it currently contains, even if incomplete. Only the forming bucket, identified by
  wall-clock and independent of `cursor`/`limit`/`start`/`end`, may be emitted incomplete.
- **REQ-API-211** (Ubiquitous): The system shall never fabricate, interpolate, or synthesise any
  source candle to fill a gap; aggregated buckets shall be composed only from source candles
  that actually exist.

### Labelling and currency isolation

- **REQ-API-212** (Ubiquitous): The system shall set the `source` field of every aggregated
  candle to `aggregated:<source_interval>` (the stored interval string it was folded from, e.g.
  `aggregated:1h`, `aggregated:30m`) and shall not alter the `source` of natively-served
  candles (D6).
- **REQ-API-213** (Ubiquitous): The system shall never fold source candles of differing
  `vs_currency` into the same aggregated bucket; aggregation shall group source candles within a
  single `vs_currency` (enforced by the explicit filter in REQ-API-219).

### Carried-over contracts (restated for the aggregated path)

- **REQ-API-214** (Ubiquitous): The aggregated read shall honour keyset pagination identically
  to the native read — `cursor` (opaque base64url `ts` key; undecodable ⇒ 400), `limit`
  (validated maximum; out-of-range ⇒ 400), `start`/`end` time-range filtering on the aggregated
  candle `ts`, and `ORDER BY ts DESC` — returning a `next_cursor` that is null when exhausted
  (per SPEC-API-002 REQ-API-150/151).
- **REQ-API-215** (Ubiquitous): The `interval` query parameter shall remain required and
  validated against `SUPPORTED_INTERVALS` (`1m, 5m, 15m, 1h, 4h, 1d, 1w`); an absent or
  unsupported target `interval` shall still return 400 without querying, unchanged by this SPEC
  (SPEC-API-002 REQ-API-131).
- **REQ-API-216** (Ubiquitous): Every OHLCV value on the aggregated path shall remain
  `rust_decimal::Decimal` end to end with no `f64` round-trip and shall serialise losslessly as
  a DecimalString via `CoinCandleDto` (REQ-PROV-012, SPEC-API-002 REQ-API-152).

### `vs_currency` filtering

- **REQ-API-217** (Event-Driven): When a client requests `GET /v1/coins/{coin_id}/candles` with
  an optional `vs_currency` query parameter, the system shall resolve the effective currency to
  the supplied value; when the parameter is omitted, the system shall default the effective
  currency to `usd` (the codebase convention — `.unwrap_or("usd")` at
  `src/api/coin_market.rs:51,86`; no `DEFAULT_VS_CURRENCY` config constant exists, OR-API3-5a).
  Unlike `interval` (a closed, validated set), `vs_currency` is not validated against an allow-
  list: an unrecognised/unsupported `vs_currency` shall NOT be rejected with 400; it simply
  matches no stored rows and yields an HTTP 200 empty page, consistent with "absence = no data"
  (REQ-API-202).
- **REQ-API-218** (Ubiquitous): The native exact-interval read shall filter `coin_candles` by
  the resolved `vs_currency` (adding a `vs_currency = $` predicate to its WHERE clause) and shall
  return only that currency's candles.
- **REQ-API-219** (Ubiquitous): The aggregation fallback shall restrict both source-interval
  discovery (REQ-API-204/205) and bucket grouping/folding (REQ-API-206/207a/207b/208) to the
  resolved `vs_currency`, so every aggregated candle is composed solely from source candles of
  that single currency.

## Exclusions (What NOT to Build)

- **No new native intervals** — the API-supported target set stays `1m, 5m, 15m, 1h, 4h, 1d,
  1w` and `validate_interval` is unchanged; aggregation composes existing data, it does not add
  new collection cadences (SPEC-SCHED-001 is untouched).
- **No 404 or error on aggregation-impossible** — the no-divisor case preserves today's 200
  empty page; this SPEC introduces no new error status (D5).
- **No interpolation, gap-filling, or synthetic candles** — missing interior source candles
  cause bucket drops, never fabricated data (REQ-API-211).
- **No fold across vs_currency** — aggregation stays within one currency (REQ-API-213/219). The
  endpoint now filters by an explicit `vs_currency` parameter (REQ-API-217/218), but this SPEC
  does not add new currencies, currency conversion, or FX between currencies.
- **No writes / no migration** — aggregation is read-only over `coin_candles`; no aggregated
  candle is persisted and no schema change is made.
- **No provider or snapping changes** — the set of stored interval strings is whatever
  SPEC-PROV-001 already produces; this SPEC only reads them.
- **No changes to quote reads or WebSocket streams** — only `GET /v1/coins/{coin_id}/candles`
  is affected; `/quotes*` and `/coins/stream/*` are untouched (SPEC-API-002).
- **No caching or precomputation of aggregated candles** — each request computes on demand;
  materialised roll-ups are out of scope.
- **No change to keyset cursor encoding, DecimalString serialisation, or `interval`
  validation** — these carry over verbatim (REQ-API-214/215/216).

## @MX Annotation Targets (high fan_in)

- The source-interval discovery + divisibility helper (interval string ⇄ seconds, largest-
  divisor selection) — `@MX:ANCHOR` (the correctness core of the fallback; every aggregated
  response depends on it) + `@MX:REASON`: divisibility is `target_secs % source_secs == 0` and
  non-fixed-duration intervals (`1M`) must be excluded (REQ-API-203/204/205).
- The bucket fold (open/high/low/close/volume-or-NULL in Decimal) — `@MX:WARN`/`@MX:REASON`:
  the volume-NULL propagation rule (any null component ⇒ null total) is easy to regress into a
  silent zero (REQ-API-207a/207b); Decimal-only, never `f64` (REQ-API-216).
- The partial-bucket boundary (wall-clock forming vs dropped closed-and-incomplete) —
  `@MX:WARN`/`@MX:REASON`: only the bucket whose window contains `now()` may be emitted
  incomplete; every closed incomplete bucket is dropped regardless of page
  (REQ-API-209/210). The classification MUST use wall-clock, not the newest row in the page —
  a cursor/`end` bound makes the page-newest bucket unreliable as a "forming" signal.
- The `list_candles` native-vs-aggregate branch point — `@MX:NOTE` that the exact-interval read
  must be attempted first and aggregation is a fallback only (REQ-API-200/201).

## Open Items (do not guess)

- **OR-API3-1 — Selection-rule wording. RESOLVED (largest divisor).** The source selection is
  the **largest** stored interval that evenly divides the target (REQ-API-205). The earlier
  "finest-granularity / maximizes fidelity" wording is withdrawn as contradictory. Authoritative
  basis: for a complete bucket, source granularity does not change the OHLC result; the divisor
  only affects incomplete buckets, where the largest divisor produces the fewest interior drops
  (REQ-API-209) and returns more of the series. No further action.
- **OR-API3-2 — Native-vs-aggregate probe scope.** Whether the "no native candle at the exact
  interval" trigger (REQ-API-200/201) is evaluated per coin only, or additionally scoped by the
  resolved `vs_currency` (REQ-API-217) and/or the request's `start`/`end` window, and whether it
  is a cheap `EXISTS` probe versus reusing the first read. Recommend a coin+interval+`vs_currency`
  `EXISTS` probe (stable across pagination); confirm at run.
- **OR-API3-3 — Forming-bucket definition. RESOLVED (wall-clock).** The forming bucket is the
  single bucket whose window contains `now()` (`bucket_start <= now() < bucket_start +
  target_interval`); only it may be emitted incomplete (REQ-API-210). Any closed incomplete
  bucket (`bucket_start + target_interval <= now()`) is dropped (REQ-API-209). This is
  cursor-independent, so it covers not just an explicit past `end` filter but also the general
  pagination case: the earlier "newest bucket in the effective range" definition was buggy
  because a `cursor`/`end` upper bound could mislabel an older page's newest closed-incomplete
  bucket as forming. No further action.
- **OR-API3-4 — Aggregation code placement.** There is no dedicated candle-read function in
  `src/db/` today (the query is inline in the handler, `src/api/candles.rs:69-87`). Whether the
  aggregation logic lives in a new `src/db/` read module or in `src/api/candles.rs` helpers is a
  Run-phase decision.
- **OR-API3-5 — `vs_currency` filtering. RESOLVED (parameter added).** An optional `vs_currency`
  query parameter (default `usd`) now filters both the native and aggregated reads
  (REQ-API-217/218/219), closing the SPEC-API-002 gap where `list_candles` had no `vs_currency`
  filter. No further action beyond implementation.
- **OR-API3-5a — `vs_currency` default is a convention, not a constant (minor).** The `usd`
  default is a hardcoded convention across the codebase (`.unwrap_or("usd")` at
  `src/api/coin_market.rs:51,86`; collectors write `vs_currency: "usd"`); no `DEFAULT_VS_CURRENCY`
  constant exists in `src/config.rs`. Assumption: `usd` is the correct default. Whether to
  promote it to a shared constant / config value is a minor Run-phase decision; confirm at run.
- **OR-API3-6 — `1w` week anchor.** UTC/epoch truncation (REQ-API-208) anchors `1w` buckets to
  epoch weeks (1970-01-01, a Thursday) rather than ISO Monday weeks. Confirm the desired week
  anchor for `1w` targets at run; all sub-day and `1d` targets are unaffected.
