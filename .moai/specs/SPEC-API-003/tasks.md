---
id: SPEC-API-003
type: tasks
updated: 2026-07-01
---

# SPEC-API-003 — Task Decomposition

Brownfield, read-only enhancement of `GET /v1/coins/{coin_id}/candles`. Methodology: TDD
(RED-GREEN-REFACTOR). Ordering is by dependency, not strict delta order: the pure aggregation
building blocks (`[NEW]` `src/api/candles_agg.rs`) are unit-testable in isolation and land before
the handler wiring (`[MODIFY]` `src/api/candles.rs`) that depends on them. There are no `[REMOVE]`
deltas and no migration.

Each task: write/extend the test first (RED), implement the minimum to pass (GREEN), refactor.
Every task must end with the tree compiling AND `cargo clippy --all-targets --all-features -- -D
warnings` clean AND `cargo test` (non-ignored) green. DB-backed scenarios are `#[ignore]` and run
via `DATABASE_URL=... cargo test -- --ignored` (per CLAUDE.md), kept inline in the `candles.rs`
tests module consistent with the existing `db_list_candles_unknown_coin_returns_404`.

The OHLCV fold, divisibility, source selection, bucket alignment, and partial-bucket policy are
pure functions over `&[CoinCandle]` / interval strings with `now()` injected — no SQL — so
REQ-API-203..211 are proven by hermetic unit tests (no DATABASE_URL). SQL only does indexed row
retrieval (EXISTS probe, distinct-interval discovery, bounded source fetch).

## Tasks

| ID | Δ | Description | REQ | Files | Pred | Done criterion |
|----|---|-------------|-----|-------|------|----------------|
| T-001 | NEW | Pure `interval_to_seconds(&str) -> Option<i64>` covering the full stored vocabulary (`1m,3m,5m,15m,30m,1h,2h,4h,6h,8h,12h,1d,3d,4d,1w`), returning `None` for non-fixed-duration units (`1M`) and unrecognised strings. Table-driven, read-side only (do not couple to providers' secs→string tables). Create `src/api/candles_agg.rs`; register `pub mod candles_agg;` in `src/api/mod.rs`. | 203,204 | `src/api/candles_agg.rs` (new), `src/api/mod.rs` | — | Unit tests: every vocab string → exact seconds; `1M`→`None`; unknown→`None`; module compiles + `cargo test` green |
| T-002 | NEW | Pure `select_source_interval(stored: &[&str], target_secs: i64) -> Option<&str>`: keep stored intervals whose seconds divide `target_secs` evenly (`target_secs % source_secs == 0`), exclude non-fixed units via T-001, return the **largest** divisor (closest to target); `None` when no stored interval divides. Mark `@MX:ANCHOR` + `@MX:REASON` (correctness core). | 203,204,205 | `src/api/candles_agg.rs` | T-001 | Unit tests for worked examples: `4h`←`1h` over `{1h,15m,5m,1m}`; `1d`←`4h` over `{30m,4h,4d}` (Scenario 3); `1h`←`30m` (Scenario 4); `1h` over `{4h,4d}`→`None` (Scenario 9/10, `3600 % 14400 != 0`); `cargo test` green |
| T-003 | NEW | Pure bucket alignment: `bucket_start(ts, target_secs) -> DateTime<Utc>` = UTC/epoch truncation (`epoch_secs - epoch_secs.rem_euclid(target_secs)`) applied uniformly to all targets incl `1w` (epoch-Thursday anchor, OR-API3-6); plus half-open assignment of a source candle `ts` into `[bucket_start, bucket_start + target)`. | 208 | `src/api/candles_agg.rs` | T-001 | Unit tests: `4h`/`1d`/`1w` alignment; `1w` bucket starts land on Thursday 00:00 UTC; boundary `ts` assigned to the correct half-open window; aggregated `ts == bucket_start` (Scenario 2); `cargo test` green |
| T-004 | NEW | Pure OHLC fold within a bucket in `rust_decimal::Decimal`: `open` = earliest-`ts` source open, `high` = max source high, `low` = min source low, `close` = latest-`ts` source close. No `f64` at any point. | 206,216 | `src/api/candles_agg.rs` | T-003 | Unit test with `dec!` fixtures reproducing Scenario 2 (open `100`, high `130`, low `95`, close `120`); values round-trip as DecimalString; `cargo test` green |
| T-005 | NEW | Pure volume fold with null propagation: sum component volumes in `Decimal` **iff every** source candle in the bucket has `Some` volume (REQ-API-207a); if **any** is `None`, result is `None` (REQ-API-207b) — never a silent zero. Mark `@MX:WARN` + `@MX:REASON`. | 207a,207b,216 | `src/api/candles_agg.rs` | T-004 | Unit tests: all-present sums to `1500` (Scenario 5); one `None` component → `None` (Scenario 6); `cargo test` green |
| T-006 | NEW | Compose the pure pipeline `aggregate_candles(source: Vec<CoinCandle>, target_secs, source_secs, source_interval, vs_currency, now) -> Vec<CoinCandle>`: assign→fold (T-004/005)→classify by wall clock. Forming bucket (`bucket_start <= now < bucket_start + target`) emitted even if it holds `< N` source candles; every **closed** bucket (`bucket_start + target <= now`) missing any of its `N = target_secs/source_secs` source candles is dropped; never fabricate/interpolate. Set `ts = bucket_start`, `interval = target`, `vs_currency`, `source = aggregated:<source_interval>`; output ordered `ts DESC`. Mark the wall-clock boundary `@MX:WARN` + `@MX:REASON` (cursor-independence invariant). | 209,210,211,212 | `src/api/candles_agg.rs` | T-003,T-005 | Unit tests: closed interior gap dropped, neighbours kept, nothing interpolated (Scenario 7); forming incomplete emitted from partial sources (Scenario 8); a closed-incomplete bucket dropped regardless of list position (Scenario 8b logic); every output carries `source == "aggregated:<si>"`; `cargo test` green |
| T-007 | MODIFY | Handler currency plumbing + native path + trigger. Add `vs_currency: Option<String>` to `ListCandlesParams`; resolve `params.vs_currency.as_deref().unwrap_or("usd").to_lowercase()` (mirror `coin_market.rs:51,86`; no new constant, OR-API3-5a). Add `AND vs_currency = $` to the native exact-interval query (`candles.rs:69-87`). Add a cheap trigger probe `SELECT EXISTS(SELECT 1 FROM coin_candles WHERE coin_id=$1 AND interval=$2 AND vs_currency=$3)` (OR-API3-2): true → serve native (even if this page is empty), false → aggregation branch. Mark the branch `@MX:NOTE`. | 200,201,217,218,213 | `src/api/candles.rs` | — | Handler tests: omitted `vs_currency` defaults to `usd` (Scenario 15); unknown `vs_currency` is NOT 400 (acceptance edge / Scenario 14). DB-gated `#[ignore]`: native precedence, no `aggregated:` source (Scenario 1); native currency filter (Scenario 14 native). `cargo test` green |
| T-008 | MODIFY | Wire the aggregation fallback. When the probe is false: query the distinct stored `interval` strings for `(coin_id, vs_currency)`; `select_source_interval` (T-002); if a divisor exists, fetch the currency+interval+range-scoped source candles ordered `ts DESC`, call `aggregate_candles` (T-006), map to `CoinCandleDto`; if no divisor, return today's HTTP 200 empty page `{"items": [], "next_cursor": null}` (no 404/error). Aggregation input is scoped to the single resolved `vs_currency` so no bucket mixes currencies. | 201,202,212,219,213 | `src/api/candles.rs` | T-002,T-006,T-007 | DB-gated `#[ignore]`: aggregate `4h`←`1h` OHLC (Scenario 2); largest divisor (Scenario 3); non-API source `30m` (Scenario 4); volume sum / null (Scenario 5,6); closed-gap drop (Scenario 7); forming emitted (Scenario 8); no-divisor empty page (Scenario 9); currency isolation (Scenario 12). `cargo test` green + `--ignored` green |
| T-009 | MODIFY | Keyset pagination + `start`/`end` over aggregated results. Bound the source read from the page window: upper = `min(end, cursor_ts)` (cursor exclusive) mapped to bucket space, floor at `start` or `upper - (limit+1)*target`, source-row cap `(limit+1)*N`. Emit buckets ordered `ts DESC`; reuse `paginate_ts` for truncation/`next_cursor`, with a truncation-aware `has_more` so a gap-thinned page still continues (see Risk R1). Forming-only-on-first-page holds by the T-006 wall-clock rule, independent of `cursor`. | 214,213 | `src/api/candles.rs`, `src/api/candles_agg.rs` | T-008 | Unit test for the source-window/`has_more` helper. DB-gated `#[ignore]`: `limit`+`cursor` roundtrip and null final cursor (Scenario 11); closed-incomplete dropped on an older page while forming appears only on page 1 (Scenario 8b); `start`/`end` range filter (Scenario 16). `cargo test` green |
| T-010 | MODIFY | Final gate + regression + `@MX`. Confirm `interval` validation unchanged (existing 400 tests still green, REQ-API-215) and Decimal end-to-end / DecimalString with no `f64` (REQ-API-216). Ensure `@MX:ANCHOR` (T-002), `@MX:WARN` (T-005 volume-null, T-006 wall-clock), `@MX:NOTE` (T-007 branch, T-003 `1w` epoch-Thursday) are present with `@MX:REASON`. Run the full gate. | 215,216 (+ all) | `src/api/candles.rs`, `src/api/candles_agg.rs` | T-009 | `cargo fmt --check` + `cargo clippy --all-targets --all-features -- -D warnings` + `cargo test` all green; `DATABASE_URL=... cargo test -- --ignored` green; acceptance.md Definition-of-Done checklist satisfied |

## Requirement coverage map

| REQ | Task(s) |
|-----|---------|
| REQ-API-200 native precedence | T-007 (probe), T-008 |
| REQ-API-201 aggregate on miss | T-007, T-008 |
| REQ-API-202 no-divisor empty page | T-008 |
| REQ-API-203 divisibility (secs modulo) | T-001, T-002 |
| REQ-API-204 discover stored intervals, exclude `1M` | T-001, T-002 |
| REQ-API-205 largest divisor | T-002 |
| REQ-API-206 OHLC fold in Decimal | T-004 |
| REQ-API-207a volume sum when all present | T-005 |
| REQ-API-207b volume null if any absent | T-005 |
| REQ-API-208 epoch-aligned buckets | T-003 |
| REQ-API-209 drop closed incomplete | T-006 |
| REQ-API-210 forming emitted | T-006 |
| REQ-API-211 never interpolate | T-006 |
| REQ-API-212 `aggregated:<si>` label | T-006 (set), T-008 (assert) |
| REQ-API-213 never fold across currency | T-007, T-008, T-009 |
| REQ-API-214 keyset pagination | T-009 |
| REQ-API-215 interval validation unchanged | T-010 (regression) |
| REQ-API-216 Decimal end-to-end / DecimalString | T-004, T-005, T-010 |
| REQ-API-217 `vs_currency` param + `usd` default + unrecognised not 400 | T-007 |
| REQ-API-218 native read currency filter | T-007 |
| REQ-API-219 aggregation scoped to currency | T-008 |

## Resolved open items (Run-phase decisions)

- **OR-API3-2 (trigger scope):** coin+interval+`vs_currency` `EXISTS` probe, evaluated once, NOT
  scoped by `cursor`/`start`/`end`. Required for correctness (not just cost): a deep cursor
  legitimately returns an empty native page, so "empty read ⇒ aggregate" would misfire
  (acceptance edge, `acceptance.md:174`). `vs_currency` is in the probe because native rows may
  exist in one currency but not the requested one. — T-007.
- **OR-API3-4 (placement):** pure aggregation logic in new `src/api/candles_agg.rs`; SQL stays
  inline in `src/api/candles.rs` (the codebase inlines reads in handlers — `quotes.rs`,
  `coin_market.rs`, `candles.rs`; no `src/db/` candle-read module exists, and adding one just for
  this would break convention). Keeps the fold hermetically unit-testable and `list_candles` thin.
- **OR-API3-5a (`usd` default):** mirror `.unwrap_or("usd").to_lowercase()`; no
  `DEFAULT_VS_CURRENCY` constant (out of read-path scope; convention is already pervasive). — T-007.
- **OR-API3-6 (`1w` anchor):** epoch-Thursday (uniform epoch truncation for all targets). Avoids a
  second alignment rule; epoch-day sources tile epoch-weeks exactly, so `N=7` completeness holds.
  Documented via `@MX:NOTE` so it is not mistaken for a bug. — T-003.

## Risks

- **R1 — pagination over computed rows.** A gap-heavy series can thin a page below `limit`; naive
  reliance on `paginate_ts`'s `len > limit` heuristic could emit `next_cursor = null` while older
  complete buckets remain, dropping data. Mitigation (T-009): source-row cap `(limit+1)*N` and a
  truncation-aware `has_more` (source read returned the cap ⇒ more pages), `next_cursor` from the
  oldest emitted bucket. Covered by Scenario 11/8b/16.
- **R2 — volume-null regression.** `Option` fold can coerce a missing component to zero.
  Mitigation: dedicated unit tests (T-005) + `@MX:WARN`.
- **R3 — wall-clock vs page-newest.** Classifying the forming bucket by "newest row on the page"
  breaks under a `cursor`/`end` bound. Mitigation: `now()`-based classification (T-006) + `@MX:WARN`.
- **R4 — precondition P1 (source `ts` epoch-aligned).** The `N`-count completeness check assumes
  aligned source `ts`. Verify against real `coin_candles` rows in T-008 DB-gated tests.
