# Research — SPEC-REFACTOR-001 (Phase 7: Batching & Structural Debt Reduction)

Tier-L research artifact. Summarizes the finding rationale from the crypto-collector idiomatic-Rust
review (`research/idiomatic-rust.md`, §§3/6/7/8) for the Phase-7 findings (F-16, F-50..F-56), and
records the codebase grounding measured while authoring this SPEC. Source is cited and summarized, not
copied wholesale.

## R.1 Source

- **Primary**: `research/idiomatic-rust.md` — the full idiomatic-Rust review of crypto-collector.
  Relevant sections: §3 (overall idiomatic assessment), §6 Category C (F-16, transport & pacing) and
  Category G (F-50..F-56, structure & idiomatic debt), §7 (recommended implementation order — Phase 7
  row), §8 (risks/trade-offs — F-51 NOTIFY).
- **Grounding**: direct reads of the cited code (line ranges below), performed 2026-07-27.

## R.2 §3 — why this phase exists (idiomatic assessment)

The review's overall idiomatic assessment (§3) makes three points that motivate Phase 7:

1. **Duplication is the dominant idiomatic debt.** "Every confirmed drift bug in this review maps onto
   one of these duplication sites" — request/parse/429 scaffolding, four chain-fetch helpers, two
   claim/heartbeat scaffolds, duplicated `ensure_coin_exists`, three paginators. Consolidation is the
   highest-leverage maintainability work.
2. **Type-system leverage is inconsistent.** `ProjectionModel` (enum + `FromStr` + single source of
   truth for validation/discovery/SQL) is the pattern done right; against it, candle intervals are
   stringly-typed with two hand-synced tables coupled by a runtime `.expect` (F-54), and
   `MarketQuery { market_id: 0 /* dummy */ }` appears four times.
3. **Trait design is mostly right but stub-heavy.** The `Provider` trait is correctly object-safe and
   `Arc<dyn>`-based (dynamic dispatch is right for a runtime-configured chain), but 8 of 9 methods
   lack default bodies, producing ~450 lines of `NotSupported` boilerplate, and the two search methods
   are lopsided directory concerns implemented by one provider (F-50).

## R.3 §6 findings summarized

### Category C — transport & pacing

- **F-16 [Medium, architecture/behavioral] — pacer slot charged to the wrong provider.** All three
  workers key `acquire_slot` on the FIRST capability-supporting chain member, then run the fallback
  chain, which may be served by a DIFFERENT provider. Exactly when the primary fails (when fallback
  fires), fallback providers get unpaced traffic while the failing primary's credits are burned. The
  review flags this as "a design decision to make explicit, not a quick patch", and recommends
  acquiring per attempted provider inside the chain loop — pairing naturally with the F-53 `chain_try`
  consolidation. → This SPEC's **intended behavior change (a)** (REQ-REFACTOR-021).

### Category G — structure & idiomatic debt

- **F-50 [Medium] — Provider trait: no default bodies (~450 lines of stubs); lopsided search
  methods.** Only `fetch_ohlc_range` has a default. Coinbase/Kraken are all-stub; the test doubles
  repeat dead lines. `supports()` and per-method `NotSupported` encode the same fact twice.
  Recommendation: capability-derived default bodies (only `name()`/`supports()` mandatory);
  longer-term move the search pair to a `CoinDirectory` trait. → M1; the `CoinDirectory` split is the
  Kickoff decision, resolved to Opt A (defer the split to SPEC-COINDIR-001).
- **F-51 [Medium, performance] — per-row upserts + per-row NOTIFY in hot candle paths.** Dispatch
  upserts each candle individually (~2016 rows per 7-day/5m refresh); backfill likewise per page (up
  to ~1000 rows/page × ~1000 pages), each in its own transaction with its own NOTIFY. `rollup.rs`'s own
  `@MX:NOTE` documents the cost and provides `batched_upsert_candles` (UNNEST) — unused by the two
  heaviest writers. Backfill NOTIFYs also flood the WebSocket path with historical data. Recommendation:
  generalize the UNNEST batcher for collection + backfill, WITHOUT NOTIFY on the backfill path; keep
  NOTIFY for live polls. → M4; the no-NOTIFY-on-backfill is **intended behavior change (b)**
  (REQ-REFACTOR-042), and §8 explicitly says this must be stated in the SPEC/acceptance.
- **F-52 [Medium, performance] — `recompute_cycle_overlay` issues ~4000–8000 single-row INSERTs in one
  transaction** (the longest-held connection on a small pool). Recommendation: UNNEST batches per model
  group, keeping DELETE + INSERT in one transaction. → M4.
- **F-53 [Low, maintainability] — systemic duplication.** (a) Four near-identical fallback loops with
  identical registry bookkeeping. (b) Claim/heartbeat/complete/release scaffolding duplicated between
  the two queue workers, incl. the heartbeat task — the F-02 classification drift is a direct product
  of this. (c) `ensure_coin_exists` duplicated verbatim; tracked_coins column list inlined 5×. (d)
  Three paginators. Recommendations: generic `chain_try<T>`, a shared lease-queue scaffold, single
  `ensure_coin_exists`, `concat!` column const, generic `paginate<T, K>`. → M2/M3/M5.
- **F-54 [Low, maintainability] — stringly-typed domain values.** API-facing intervals are `&str`
  validated against `SUPPORTED_INTERVALS` then mapped by a SECOND hand-synced table
  (`interval_to_seconds`), coupled by `.expect("validated interval must have a known second count")` —
  a runtime panic path guarding a compile-time-expressible invariant. `MarketQuery { market_id: 0 }`
  repeated in four call sites. Recommendation: `enum ApiInterval` (`FromStr` + `secs()`); a keyed enum
  for coin-vs-market queries. → M6. Kickoff resolved the interval work to the LITERAL-ANCHOR path
  (both tables fold into one `ApiInterval` covering the full vocabulary, with an `is_api_facing()`
  boundary predicate preserving the existing 400).
- **F-55 [Low] — `AppState` carries two dead fields (`http_client`, `coingecko_base_url`) + 6+
  duplicated test-state builders.** Read by no handler (search goes through the provider chain).
  Recommendation: remove both; add a shared `#[cfg(test)] AppState::test()`. → M5.
- **F-56 [Informational] — structural notes.** Three items selected into this SPEC (user decision):
  `Arc<Vec<Arc<dyn Provider>>>` → `Arc<[Arc<dyn Provider>]>` (drops a hop, expresses immutable-after-
  build); dead `CgMarketItem.vs_currency` field; legacy `coingecko_days_to_interval` helper (kept only
  for tests). Other F-56 items (config snapshot, edition 2024, User-Agent) are out of scope.

## R.4 §7 — why Phase 7 lands last

The review orders the work into seven phases by operational-risk reduction per unit of change. Phase 7
("Batching & structural debt reduction", F-50/F-51/F-52/F-53/F-54/F-55/F-16 + F-56 selections) is
LAST because "pure refactors and batching land on top of corrected behavior, so behavior-preservation
is verifiable against the fixed baseline; F-16 (per-provider pacing in the chain) belongs here because
it rides the `chain_try` consolidation." Phase 7 refactors are behavior-preserving by definition; the
review recommends relying on the existing pure-core test density plus `cargo clippy -D warnings` and
characterization tests where coverage is thin (chain helpers, supervisors).

## R.5 §8 — risk that shapes acceptance

§8 calls out **F-51 (batch upserts without NOTIFY on backfill)** as an intentional WebSocket behavior
change (historical candles no longer broadcast) that "is the desired behavior but should be stated in
the SPEC/acceptance." → captured as intended behavior change (b) with a dedicated LISTEN-based DB-gated
AC (AC-REFACTOR-042a).

## R.6 Codebase grounding (measured 2026-07-27)

Direct reads confirming the finding sites and the two design tensions:

- **Provider trait** — `src/providers/mod.rs:269-355`. 9 methods; only `fetch_ohlc_range` has a
  default (`:309-317`). Object-safe (`Arc<dyn Provider>` via `build_chain` `:378-382`). Coinbase/Kraken
  have 7 `NotSupported` each; `providers/mod.rs` has 6 test doubles (`impl Provider for`).
- **Four fallback loops** — `chain_fetch_spot_local`/`chain_fetch_coin_metadata`/`chain_fetch_coin_market`
  in `collection_queue.rs:355-468`; `chain_fetch_spot` in `live_poller.rs` (def ~L476, call ~L404).
  Pacing keyed on the first capable provider: `first_provider_for_cap` (def `collection_queue.rs:471`,
  calls 516/624/696/760); `live_poller.rs` paces via `acquire_slot` (~L378) on the first `Spot`-capable
  member (`chain.iter().find(..)` ~L351). `first_provider_for_cap` lives ONLY in `collection_queue.rs`.
- **Per-row upsert + NOTIFY** — `db/upserts.rs:44-150` (`upsert_coin_quote`, `upsert_coin_candle`, each
  a tx with an in-tx `pg_notify`). The UNNEST model already exists: `rollup::batched_upsert_candles`
  (`rollup.rs:175-225`) with the native-wins `WHERE coin_candles.source LIKE 'rollup:%'` guard (D1).
- **Cycle overlay** — `recompute_cycle_overlay` (`cycle_overlay.rs:397-439`): DELETE-all then a nested
  per-row INSERT loop over three model groups inside one transaction.
- **API dedup** — `ensure_coin_exists` in `api/quotes.rs:231` + `api/metadata.rs:86`; paginators
  `paginate_coins` (`coins.rs:350`), `paginate_ts` (`quotes.rs:244`), `paginate_cycle_overlay`
  (`api/cycle_overlay.rs:400`); tracked_coins column list inlined 5× in `coins.rs`; dead `AppState`
  fields `coingecko_base_url` (`mod.rs:56`) + `http_client` (`mod.rs:58`) populated in 12 test builders.
- **Interval typing (the literal-anchor tension)** — `SUPPORTED_INTERVALS` (7 API values,
  `candles.rs:35`); `interval_to_seconds` (15 stored values incl. `3m/30m/2h/6h/8h/12h/3d/4d`,
  `candles_agg.rs:28`), coupled by `.expect` at `candles.rs:164/193`; public validation is
  `validate_interval` using `SUPPORTED_INTERVALS.contains` (`candles.rs:314-322`).
  **`interval_to_seconds` has 59 references** across `candles.rs`, `candles_agg.rs`, `coingecko.rs`,
  `rollup.rs`, `cycle_overlay.rs`, `backfill.rs` (+ tests) — this is the fan-in the literal-anchor path
  migrates onto `ApiInterval::secs()`. `interval_to_seconds` returns `None` for `1M` (non-fixed
  duration) and unknown strings — the exclusion semantics `ApiInterval::from_str(..).ok()` preserves.
- **`MarketQuery { market_id: 0 /* dummy */ }`** — 4 sites: `live_poller.rs:342`, `backfill.rs:705`,
  `collection_queue.rs:535`, `:643`.
- **F-56 items** — `CgMarketItem.vs_currency: Option<String>` (`coingecko.rs:131-133`, dead);
  `coingecko_days_to_interval` (`coingecko.rs:695`, legacy + tests at 1462+); `AppState.chain:
  Arc<Vec<Arc<dyn Provider>>>` (`mod.rs:52`).

## R.7 Sibling-service reference

Per project convention crypto-collector mirrors the proven `ticker-collector` patterns (adapt, never
copy). The `chain_try` / lease-queue / batched-upsert consolidations align crypto-collector's provider
and collector layers closer to the sibling's shape without importing equities-specific machinery
(crypto is 24/7, Decimal-only money, per-provider pacer, single `/v1` API).

## R.8 Open decisions resolved

- **F-50 CoinDirectory split** → Opt A (single trait, `Ok(vec![])` default); Opt B → SPEC-COINDIR-001.
- **F-54 interval typing** → literal-anchor (both tables fold into `ApiInterval`; `is_api_facing()`
  boundary guard preserves the 400). Both are behavior-preserving.
