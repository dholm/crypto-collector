# Design — SPEC-REFACTOR-001 (Phase 7: Batching & Structural Debt Reduction)

Tier-L design artifact. Records the adopted design decisions (post-Implementation-Kickoff-Approval),
the two new domain types, the two intended behavior changes, and the shapes of the shared helpers.
This is the HOW-shape reference for run-phase; the WHAT/WHY contract lives in `spec.md`, and the
finding rationale lives in `research.md`.

## D.1 Adopted decisions (Kickoff-resolved)

### D.1.1 F-50 CoinDirectory split → Opt A (adopted); Opt B deferred

The `search_coins` / `fetch_coin_tickers` pair are directory concerns implemented by exactly one
provider (CoinGecko). **Adopted Opt A**: the pair stays on the single `Provider` trait with an
`Ok(vec![])` default body; no separate trait is introduced.

- **Design shape**: capability-derived default bodies on the trait — every fetch method defaults to
  `Err(ProviderError::NotSupported(<capability>))`, and the search pair defaults to `Ok(vec![])`.
  `name()` and `supports()` remain the only mandatory methods.
- **Object-safety**: preserved — no default introduces a generic type parameter; `Arc<dyn Provider>`
  and `build_chain` are unchanged (the trait stays `dyn`-compatible).
- **Deferral**: Opt B (extract a CoinGecko-only `CoinDirectory` trait + rewire the API search path,
  resolving the "no `Capability::Search`" smell) is OUT OF SCOPE → **SPEC-COINDIR-001**. Reason: Opt B
  touches the externally-visible API search path and needs its own regression cover; keeping it out
  keeps Phase 7 a clean behavior-preserving refactor.

### D.1.2 interval typing → literal-anchor path (adopted)

**Adopted**: BOTH `SUPPORTED_INTERVALS` (the 7-value API allow-list) and `interval_to_seconds` (the
15-value stored→seconds table) are removed literally and fold into a single `ApiInterval` enum. See
D.2.1 for the type design and the critical API-boundary guard.

## D.2 New domain types

### D.2.1 `ApiInterval` — one enum, full vocabulary, total `secs()`, API-facing predicate

```
// full fixed-duration vocabulary (15 variants): API-facing ∪ storage-only
enum ApiInterval { M1, M3, M5, M15, M30, H1, H2, H4, H6, H8, H12, D1, D3, D4, W1 }

impl FromStr for ApiInterval { ... }   // "1m"→M1 ... "1w"→W1; "1M"/unknown → Err
impl ApiInterval {
    fn as_str(&self) -> &'static str    // "1m".."1w"
    fn secs(&self) -> i64               // TOTAL — one value per variant, no Option, no .expect
    fn is_api_facing(&self) -> bool     // true for {1m,5m,15m,1h,4h,1d,1w}; false for storage-only
    // optional: fn from_api_str(s: &str) -> ApiResult<ApiInterval>  // FromStr + is_api_facing gate → 400
}
```

Design invariants:

- **Totality of `secs()`.** Because every variant is a fixed-duration interval, `secs()` returns an
  `i64` unconditionally. Non-fixed-duration strings (`1M` monthly, or garbage) are NOT variants —
  they fail `FromStr`. This is the whole point of folding: the `Option`/`.expect` coupling between
  `SUPPORTED_INTERVALS` and `interval_to_seconds` disappears because parse-failure (not a `None`
  return from a second table) is what excludes non-fixed intervals. Value table (must match the
  retired `interval_to_seconds` verbatim): `1m`=60, `3m`=180, `5m`=300, `15m`=900, `30m`=1800,
  `1h`=3600, `2h`=7200, `4h`=14400, `6h`=21600, `8h`=28800, `12h`=43200, `1d`=86400, `3d`=259200,
  `4d`=345600, `1w`=604800.
- **API-facing subset guard (the critical, characterization-preserving part).** `SUPPORTED_INTERVALS`
  used to be the public API allow-list; removing it must NOT let a client request a storage-only
  interval. The public handler (today `validate_interval`, `api/candles.rs`) MUST validate against
  `is_api_facing()` (or call `from_api_str`), returning the existing 400 (`ApiError::BadRequest`) for
  any storage-only or unparseable interval. So `GET candles?interval=3m` → 400, identical to the prior
  `SUPPORTED_INTERVALS.contains("3m") == false` path. This is behavior-preserving, NOT an intended
  change (AC-REFACTOR-062b is the characterization test).
- **Internal callers use the full vocabulary.** `candles_agg` (source selection), `rollup`,
  `cycle_overlay`, `backfill`, and the CoinGecko range-stamp validation resolve any persisted
  interval string via `ApiInterval::from_str(iv).ok().map(|i| i.secs())` — the `.ok()?`/`.map()`
  shape preserves the prior "unparseable/non-fixed → excluded" semantics that `interval_to_seconds(iv)?`
  provided. `ApiInterval::secs()` becomes the new `@MX:ANCHOR`, inheriting the 59 references measured
  on `interval_to_seconds` (2026-07-27).

Why one enum instead of two tables: the F-54 defect is precisely the hand-synced coupling between the
allow-list and the seconds-table, bridged by a runtime `.expect`. One enum with a total `secs()` plus
an `is_api_facing()` predicate expresses BOTH concerns (parse+seconds over the full vocabulary; the
narrower API-accepted subset) without a sync obligation and without a panic path. The storage-vs-API
width difference is now `!is_api_facing()` over the single enum.

### D.2.2 Keyed `MarketQuery` — remove the `market_id: 0 /* dummy */` sentinel

The coin-keyed collection paths construct `MarketQuery { market_id: 0 /* dummy */, coin_id: Some(..),
.. }` at 4 sites (`live_poller.rs:342`, `backfill.rs:705`, `collection_queue.rs:535`, `:643`). The
`market_id: 0` sentinel is meaningless for the coin-keyed path and is a latent footgun.

- **Design shape**: replace the dummy with a keyed enum — `CoinKeyed { coin_id, symbol }` |
  `MarketKeyed { market_id }` (plus the shared fields the providers consume: `base`, `quote`,
  `venue`, `vs_currency`), or an equivalent that makes `market_id: 0` unrepresentable. The keyed
  variant carries exactly the fields its path uses; the market-keyed provider consumers pattern-match
  the variant instead of reading a possibly-dummy `market_id`.
- **Scope**: the 4 construction sites + the provider methods that consume `MarketQuery`. Behavior is
  unchanged (the coin-keyed path never used `market_id`); only the representation changes.

## D.3 The two intended behavior changes

### D.3.1 (a) Per-provider chain-fallback pacing (F-16) — inside `chain_try`

Prior behavior: `acquire_slot` is keyed on the FIRST capability-supporting provider
(`first_provider_for_cap` in `collection_queue.rs`; the inline `chain.iter().find(..)` + `acquire_slot`
in `live_poller.rs`) BEFORE the fallback loop runs — so when the primary fails and a fallback serves,
the fallback's traffic is unpaced and the failing primary's credits are burned.

New behavior: `chain_try` acquires the pacer slot and signals cooldown for EACH attempted provider,
inside the loop, so pacing is charged to the provider that actually serves. The pacer `@MX:WARN`
(fleet-wide egress governor invariant) moves into `chain_try`.

### D.3.2 (b) No NOTIFY on backfill writes (F-51)

Prior behavior: every candle upsert (live + backfill) runs its own transaction with its own
`pg_notify`, so backfill historical rows flood the listener → broadcast → WebSocket path, accelerating
`Lagged` drops for live consumers.

New behavior: live-poll quote/candle upserts KEEP per-event NOTIFY; the backfill page-write path emits
NONE. Verified by a `LISTEN`-based DB-gated test (live=1, backfill=0).

## D.4 Shared-helper shapes

### D.4.1 `chain_try<T>` (M2)

```
async fn chain_try<T, F, Fut>(
    chain: &[Arc<dyn Provider>],
    capability: Capability,
    registry: Option<&HealthRegistry>,
    pool: &PgPool,          // for per-attempt acquire_slot / signal_cooldown (F-16)
    f: F,                    // |&Arc<dyn Provider>| -> Fut  (the per-provider fetch closure)
) -> Result<T, ProviderError>
where F: Fn(&Arc<dyn Provider>) -> Fut, Fut: Future<Output = Result<T, ProviderError>>
```

- Iterates in declared order (D2); skips `!supports(capability)` members; per attempted provider:
  acquire slot + signal cooldown (F-16), run `f`, record registry success/network-failure.
- Preserves the empty-chain vs non-empty-all-unsupported distinction → `NoCapableProvider(capability)`.
- Replaces `chain_fetch_spot`, `chain_fetch_spot_local`, `chain_fetch_coin_metadata`,
  `chain_fetch_coin_market`. Does NOT replace `chain_fetch_ohlc{,_range}` (D5 — distinct
  continue-on-empty semantics, preserved unmodified).

### D.4.2 Shared lease-queue scaffold (M3)

Extract the claim / heartbeat / complete / release lifecycle shared by `collection_queue` and
`backfill` into one parameterized helper, including the spawned heartbeat task. Prefer a
`tokio::sync::watch`-based stop signal over `abort()` (cleaner cancellation; a fencing-failed
heartbeat can also cancel the dispatch to stop wasted upstream credits). Preserve the SPEC-SCHED-002
transient/permanent/soft-skip classification exactly (`DispatchOutcome`, `ChunkOutcome`,
`DispatchError`) — the F-02 classification drift that duplication caused must not reappear.

### D.4.3 Shared batched candle upsert + the D1 two-policy split (M4)

Generalize `rollup::batched_upsert_candles` (UNNEST-based single-round-trip insert) into the shared db
layer. The critical design constraint is that **two conflict policies must stay distinct** (D1):

- **Native-write path** (dispatch, backfill page writes): unconditional `ON CONFLICT ... DO UPDATE`
  (matching the per-row `upsert_coin_candle`). NO `rollup:%` guard.
- **Rollup path**: retains `ON CONFLICT ... DO UPDATE ... WHERE coin_candles.source LIKE 'rollup:%'`
  (the native-wins guard — a derived materializer must not overwrite genuine provider rows, F-07).

Shape (OR-REFACTOR-3, run-phase choice): either (i) two thin functions over one UNNEST core, or (ii)
one function with a conflict-policy parameter. Either satisfies D1; the implementer picks the simpler.
`recompute_cycle_overlay` (M4) batches its inserts via UNNEST per model group inside the existing
single DELETE + INSERT transaction (idempotent-rebuild semantics preserved).

### D.4.4 API dedup helpers (M5)

- Single `ensure_coin_exists` (currently duplicated in `api/quotes.rs` + `api/metadata.rs`).
- `concat!`-assembled `tracked_coins` column-list const (currently inlined 5× in `coins.rs`).
- Generic `paginate<T, K: Serialize>(items, limit, key_fn)` replacing `paginate_coins` /
  `paginate_ts` / `paginate_cycle_overlay`, preserving each one's truncate-and-encode cursor.
- Remove dead `AppState` fields `http_client` + `coingecko_base_url`; add `#[cfg(test)]
  AppState::test()` to collapse the 6+ duplicated test-state builders.
- F-56: `chain` becomes `Arc<[Arc<dyn Provider>]>`; remove dead `CgMarketItem.vs_currency`; remove
  legacy `coingecko_days_to_interval` + migrate its tests.

## D.5 Behavior-preservation posture

Everything except D.3.1 (a) and D.3.2 (b) is characterization-equivalent. In particular the
literal-anchor interval work (D.2.1) is behavior-preserving: identical seconds values, identical API
400 behavior — only the internal representation changes from two hand-synced tables to one enum with a
predicate. Rely on the dense pure-core test suite plus new characterization tests where coverage is
thin (chain helpers, heartbeat scaffold, `ApiInterval` round-trip, storage-only-interval-400).

## D.6 Cross-references

- `spec.md` — REQ contract, Decisions Restated (D1–D5), Exclusions, @MX targets, Open Items.
- `plan.md` — milestones, risks, §D decision detail.
- `research.md` — F-16/F-50..F-56 finding rationale.
