---
id: SPEC-REFACTOR-001
title: "Phase 7: Batching & Structural Debt Reduction"
version: "0.1.0"
status: completed
created: 2026-07-27
updated: 2026-07-27
author: manager-spec
priority: Medium
phase: "improvement-phase-7"
module: "src (providers, collectors, db, api)"
lifecycle: spec-anchored
tags: "refactor, duplication, batching, pacer, trait-defaults, domain-typing, notify"
tier: L
related_specs: [SPEC-PROV-001, SPEC-SCHED-001, SPEC-SCHED-002, SPEC-CANDLE-001, SPEC-CANDLE-002, SPEC-CYCLE-001, SPEC-API-001, SPEC-API-003, SPEC-OBS-002]
issue_number: null
---

# SPEC-REFACTOR-001 — Phase 7: Batching & Structural Debt Reduction

Phase 7 (final) of the crypto-collector review-driven improvement roadmap
(`research/idiomatic-rust.md` §7, Phase 7 — "Batching & structural debt reduction",
Categories C and G, findings F-16, F-50..F-56). This SPEC lands on top of the corrected
behavior delivered by Phases 1–6, so behavior-preservation is verifiable against the fixed
baseline.

Phase 7 is a **behavior-preserving refactor** with **EXACTLY TWO intended behavior changes**:

- **(a) Per-provider pacing in chain fallback (F-16)** — the pacer slot is acquired for the
  provider that *actually serves* the request, not the first capability-supporting member.
- **(b) No NOTIFY on backfill writes (F-51)** — historical candle rows written by the backfill
  path no longer emit `pg_notify`, so they no longer flood the WebSocket broadcast.

Every other change in this SPEC is characterization-equivalent (observably identical behavior).

## HISTORY

- **v0.1.0** (2026-07-27) — Initial draft. Scopes F-50/F-51/F-52/F-53/F-54/F-55/F-16 plus the
  three explicitly-selected F-56 items (`Arc<[…]>`, dead `CgMarketItem.vs_currency`, legacy
  `coingecko_days_to_interval`). Six milestones M1–M6, one reviewable commit per milestone.
  Full tier-L 5-artifact set (spec.md + plan.md + acceptance.md + design.md + research.md +
  progress.md). Open items resolved at Implementation Kickoff Approval:
  **(OR-REFACTOR-1) → Opt A** (single `Provider` trait, search pair defaults to `Ok(vec![])`);
  Opt B (extract a `CoinDirectory` trait) is OUT OF SCOPE, filed as future SPEC-COINDIR-001.
  **(OR-REFACTOR-2) → LITERAL-ANCHOR path**: both `SUPPORTED_INTERVALS` and `interval_to_seconds`
  fold into a single `ApiInterval` enum covering the full fixed-duration vocabulary, with an
  `is_api_facing()` boundary predicate preserving the existing 400 for storage-only intervals.

## Goal

Reduce the systemic duplication that the review identifies as **the dominant idiomatic debt
and the proven source of every confirmed drift bug** (research §3, §6 F-53), batch the hot
write paths that hold the longest DB connections, and strengthen domain typing so a class of
runtime-panic / dummy-sentinel bugs becomes unrepresentable at compile time — all while
preserving observable behavior except the two named changes.

Success is measured by: exactly one implementation of each previously-duplicated construct
remains; the two named behavior changes are demonstrated by tests; and the entire pre-existing
test suite plus `cargo clippy -D warnings` remain green with no observable behavior drift
elsewhere.

## Scope

**In scope** (research §7 Phase 7 row): F-16, F-50, F-51, F-52, F-53, F-54, F-55, and three
selected F-56 items (user decision): `Arc<[Arc<dyn Provider>]>`, dead `CgMarketItem.vs_currency`
removal, legacy `coingecko_days_to_interval` removal.

**Two intended behavior changes only**: (a) F-16 per-provider chain-fallback pacing; (b) F-51
no-NOTIFY-on-backfill. All else characterization-equivalent.

**Milestone map** (single Tier-L SPEC, one reviewable commit per milestone):

| Milestone | Theme | Findings | Change class |
|-----------|-------|----------|--------------|
| M1 | Provider trait capability-derived defaults | F-50 | mechanical + open decision |
| M2 | Generic chain fallback + honest per-provider pacing | F-53a, **F-16 (intended change a)** | dedup + behavior change |
| M3 | Shared lease-queue scaffold | F-53b | dedup |
| M4 | Batched writes + NOTIFY policy | F-51 (**intended change b**), F-52 | perf + behavior change |
| M5 | API dedup | F-53c, F-55 | dedup |
| M6 | Domain typing (interval enum, keyed query) | F-54, F-56 selections | type-shape change |

## Decisions Restated (Dn — inherited invariants this SPEC MUST preserve)

- **D1 — native-wins collision guard (SPEC-CANDLE-002 REQ-CANDLE-052).** `rollup::batched_upsert_candles`
  carries `ON CONFLICT … DO UPDATE … WHERE coin_candles.source LIKE 'rollup:%'`, so a rollup
  writer upgrades ONLY a prior rollup row and never overwrites a native provider row. The
  **rollup path MUST keep this guard**. The generalized native-write batcher (M4) writes genuine
  provider rows and MUST use the unconditional `DO UPDATE` of the per-row `upsert_coin_candle`
  path — it MUST NOT inherit the rollup-only WHERE guard. The two conflict policies are distinct
  and MUST NOT be conflated by "sharing" the batcher.
- **D2 — fallback order = declaration order (CLAUDE.md invariant, SPEC-PROV-001 REQ-PROV-003).**
  `PROVIDERS=coingecko,binance` means CoinGecko primary, Binance fallback. `chain_try` (M2) MUST
  iterate in declared order and preserve this.
- **D3 — Provider trait object-safety (SPEC-PROV-001 REQ-PROV-001).** The trait is consumed as
  `Arc<dyn Provider>` by `build_chain` and every worker. Adding default bodies (M1) MUST keep the
  trait object-safe; chain construction is unchanged.
- **D4 — money/config/deps invariants (CLAUDE.md Key Invariants).** `rust_decimal::Decimal` for
  all prices/monetary values (never f64); env-only config; no new dependencies.
- **D5 — OHLC continue-on-empty semantics (SPEC-PROV-001).** `chain_fetch_ohlc{,_range}` implement
  a distinct continue-on-empty fallthrough (Binance serves recent, Bitstamp fills pre-2017 daily).
  These are NOT the four loops M2 consolidates; their semantics MUST be preserved exactly.

## Decisions This SPEC Makes

- **DEC-1** — `chain_try` (M2) replaces the **four** non-OHLC fallback loops only
  (`chain_fetch_spot`, `chain_fetch_spot_local`, `chain_fetch_coin_metadata`,
  `chain_fetch_coin_market`). The two OHLC chains keep their distinct continue-on-empty semantics
  (D5) and are NOT folded into `chain_try`.
- **DEC-2 (LITERAL-ANCHOR — resolved at Kickoff)** — `ApiInterval` (M6) is the **single interval
  type** for BOTH parse and seconds, over the **full fixed-duration vocabulary** (API-facing set
  `{1m, 5m, 15m, 1h, 4h, 1d, 1w}` PLUS storage-only set `{3m, 30m, 2h, 6h, 8h, 12h, 3d, 4d}`).
  Both `SUPPORTED_INTERVALS` and `interval_to_seconds` are removed literally; `ApiInterval::secs()`
  is the single **total** seconds source (no `.expect`, no `Option`-panic path). The **API boundary
  validation** uses the `is_api_facing()` subset — a public API request for a storage-only interval
  (e.g. `3m`) still returns the existing 400. Persisted `interval` strings remain as-is (no data
  migration); the storage vocabulary is still wider than the API-accepted set, now expressed as a
  **predicate over one enum** instead of two hand-synced tables. This is behavior-preserving
  (identical seconds values, identical API 400 behavior) — only the internal representation changes.
- **DEC-3 (RESOLVED at Kickoff → Opt A)** — the F-50 CoinDirectory split resolves to **Opt A**:
  the `search_coins` / `fetch_coin_tickers` pair remains on the single `Provider` trait with an
  `Ok(vec![])` default. **Opt B** (extract a CoinGecko-only `CoinDirectory` trait + rewire the API
  search path) is OUT OF SCOPE for this SPEC and is filed as future **SPEC-COINDIR-001** (see
  Exclusions). M1 acceptance is the Opt-A form.

## Domain Model

Phase 7 introduces or consolidates the following shared constructs. None are new domain
*entities* — they are single-implementation replacements for duplicated scaffolding plus two
new domain *types*:

| Construct | Milestone | Replaces | Nature |
|-----------|-----------|----------|--------|
| Capability-derived trait default bodies | M1 | ~450 lines of `NotSupported` stubs | trait method defaults |
| `chain_try<T>(chain, capability, registry, f)` | M2 | 4 fallback loops | generic helper |
| Shared lease-queue scaffold (claim/heartbeat/complete/release) | M3 | 2 duplicated scaffolds | parameterized helper |
| Shared batched candle upsert (UNNEST) | M4 | per-row candle upserts in dispatch + backfill | db-layer helper |
| `paginate<T, K: Serialize>(items, limit, key_fn)` | M5 | 3 paginators | generic helper |
| Single `ensure_coin_exists` | M5 | 2 verbatim copies | shared fn |
| `concat!`-assembled tracked_coins column const | M5 | 5× inlined column list | const |
| `#[cfg(test)] AppState::test()` | M5 | 6+ duplicated test-state builders | test constructor |
| `ApiInterval` enum (`FromStr`+total `secs()`+`as_str()`+`is_api_facing()`) | M6 | BOTH `SUPPORTED_INTERVALS` AND `interval_to_seconds` (literal fold) | domain type |
| Keyed `MarketQuery` enum (`CoinKeyed`/`MarketKeyed`) | M6 | `MarketQuery { market_id: 0 /* dummy */ }` (4 sites) | domain type |

## Requirements (GEARS)

Requirements use GEARS notation. The generalized `<subject>` names the specific component
(trait, helper, worker, handler) rather than "the system" where that is clearer. Two
requirements are explicitly tagged **[INTENDED BEHAVIOR CHANGE]**; every other requirement is
behavior-preserving.

### M1 — Provider trait capability-derived defaults (F-50)

- **REQ-REFACTOR-010** — The `Provider` trait shall provide a capability-derived default body for
  every fetch method: each defaults to `Err(ProviderError::NotSupported(<capability>))`, and the
  search pair (`search_coins`, `fetch_coin_tickers`) defaults to `Ok(vec![])`. Only `name()` and
  `supports()` shall remain mandatory (without a default).
- **REQ-REFACTOR-011** — The `Provider` trait shall remain object-safe: `Arc<dyn Provider>`
  construction and `build_chain` shall be unchanged.
- **REQ-REFACTOR-012** — `src/providers/coinbase.rs` and `src/providers/kraken.rs` shall contain
  zero `NotSupported` stub method bodies, relying on the trait defaults; each shall retain only
  its real content (`name()`, `supports()`, and any capability it genuinely serves).
- **REQ-REFACTOR-013** — Each test double in `src/providers/mod.rs` shall define only the methods
  it actually exercises; dead stub methods shall be shed.
- **REQ-REFACTOR-014** — Per the DEC-3 Opt-A resolution, the `search_coins` / `fetch_coin_tickers`
  pair shall remain on the single `Provider` trait with the `Ok(vec![])` default (a separate
  `CoinDirectory` trait is NOT introduced by this SPEC — see Exclusions / SPEC-COINDIR-001).

### M2 — Generic chain fallback + honest per-provider pacing (F-53a, F-16)

- **REQ-REFACTOR-020** — Exactly one generic `chain_try<T>(chain, capability, registry, f)` helper
  shall implement the ordered fallback loop, replacing the four duplicated loops
  (`chain_fetch_spot`, `chain_fetch_spot_local`, `chain_fetch_coin_metadata`,
  `chain_fetch_coin_market`) and owning the `HealthRegistry` bookkeeping (per-provider success,
  per-provider network-failure, chain success, chain all-failed).
- **REQ-REFACTOR-021 [INTENDED BEHAVIOR CHANGE (a)]** — While `chain_try` iterates the chain,
  When it attempts a provider, the helper shall acquire that attempted provider's pacer slot and
  signal that provider's cooldown, so fallback traffic is paced and charged to the provider that
  actually serves the request — not to the first capability-supporting member. This replaces the
  prior behavior of keying `acquire_slot` on `first_provider_for_cap` before the loop.
- **REQ-REFACTOR-022** — The `chain_fetch_ohlc` and `chain_fetch_ohlc_range` continue-on-empty and
  error-surfacing semantics shall be preserved exactly; their regression tests (including
  `chain_fetch_ohlc_range_error_not_masked_by_earlier_empty`) shall remain GREEN and unmodified.
- **REQ-REFACTOR-023** — `chain_try` shall preserve the existing empty-chain vs all-unsupported
  distinction: a non-empty chain whose members are all unsupported shall surface
  `NoCapableProvider(capability)` rather than the misleading empty-chain label.

### M3 — Shared lease-queue scaffold (F-53b)

- **REQ-REFACTOR-030** — Exactly one parameterized lease-queue scaffold shall implement the
  claim / heartbeat / complete / release lifecycle (post-Phase-1 semantics) shared by
  `collection_queue` and `backfill`, including the spawned heartbeat task.
- **REQ-REFACTOR-031** — The heartbeat task shall be stopped via a `tokio::sync::watch`-based stop
  signal rather than `abort()`.
- **REQ-REFACTOR-032** — The lease-queue scaffold shall preserve the exact transient/permanent/
  soft-skip classification (`DispatchOutcome`, `ChunkOutcome`, `DispatchError`) that SPEC-SCHED-002
  established; the F-02 classification drift that duplication caused shall not be reintroduced.

### M4 — Batched writes + NOTIFY policy (F-51, F-52)

- **REQ-REFACTOR-040** — A shared UNNEST-based batched candle upsert, generalized from
  `rollup::batched_upsert_candles`, shall live in the shared db layer; the candles dispatch path
  (`collection_queue`) and the backfill page-write path shall both write through it.
- **REQ-REFACTOR-041** — The generalized batched upsert shall preserve the
  `(coin_id, vs_currency, interval, ts)` conflict target and row-for-row idempotency/parity with
  the per-row path, AND shall keep the two conflict policies distinct (D1): the native-write path
  (dispatch, backfill) uses the unconditional `DO UPDATE`; the rollup path retains its
  `WHERE coin_candles.source LIKE 'rollup:%'` native-wins guard.
- **REQ-REFACTOR-042 [INTENDED BEHAVIOR CHANGE (b)]** — Live-poll quote and candle upserts shall
  continue emitting one `pg_notify` per event; the backfill page-write path shall emit NO
  `pg_notify`, so historical rows are not broadcast to WebSocket consumers.
- **REQ-REFACTOR-043** — `recompute_cycle_overlay` shall batch its inserts via UNNEST per model
  group inside the existing single DELETE + INSERT transaction, preserving the idempotent-rebuild
  semantics (REQ-CYCLE-041/042/043) and the per-call one-transaction contract.

### M5 — API deduplication (F-53c, F-55)

- **REQ-REFACTOR-050** — Exactly one `ensure_coin_exists` shall remain (currently duplicated
  verbatim in `api/quotes.rs` and `api/metadata.rs`).
- **REQ-REFACTOR-051** — The `tracked_coins` column list shall be a single `concat!`-assembled
  const, replacing the list inlined 5× in `api/coins.rs`.
- **REQ-REFACTOR-052** — Exactly one generic `paginate<T, K: Serialize>(items, limit, key_fn)`
  shall replace the three paginators (`paginate_coins`, `paginate_ts`, `paginate_cycle_overlay`),
  preserving each one's truncate-and-encode cursor semantics.
- **REQ-REFACTOR-053** — The two dead `AppState` fields `http_client` and `coingecko_base_url`
  shall be removed (read by no handler; search routes through the provider chain).
- **REQ-REFACTOR-054** — A shared `#[cfg(test)] AppState::test()` constructor shall replace the
  6+ duplicated test-state builders across the API test modules.

### M6 — Domain typing (F-54, F-56 selections)

- **REQ-REFACTOR-060** — An `ApiInterval` enum shall be the single interval type for the full
  fixed-duration vocabulary — the API-facing set `{1m, 5m, 15m, 1h, 4h, 1d, 1w}` PLUS the
  storage-only set `{3m, 30m, 2h, 6h, 8h, 12h, 3d, 4d}` — exposing `FromStr`, `as_str()`, and a
  **total** `secs()` (returning `i64`, one value per variant, with NO `.expect` and NO
  `Option`-panic path). Non-fixed-duration strings (e.g. `1M` monthly) shall NOT be `ApiInterval`
  variants — they fail `FromStr` and are thereby excluded, exactly as `interval_to_seconds`
  previously returned `None`.
- **REQ-REFACTOR-061** — Both the standalone `SUPPORTED_INTERVALS` const and the standalone
  `interval_to_seconds` function shall be removed literally; BOTH fold into `ApiInterval`.
  `ApiInterval::secs()` shall become the new `@MX:ANCHOR` and inherit the high fan-in previously
  carried by `interval_to_seconds` (59 references across `candles_agg` / `rollup` / `cycle_overlay`
  / `backfill` / `coingecko` + tests, measured 2026-07-27). The
  `.expect("validated interval must have a known second count")` panic path (`api/candles.rs`)
  shall no longer exist.
- **REQ-REFACTOR-062** — The public API boundary shall validate against the API-facing subset via
  an `ApiInterval::is_api_facing()` predicate (or an `ApiInterval::from_api_str` that rejects
  storage-only variants). When a public API request names a storage-only interval (e.g. `3m`) or
  any non-API-facing string, the handler shall return the existing 400 (`ApiError::BadRequest`)
  without issuing a query — identical to the prior `SUPPORTED_INTERVALS` allow-list behavior. This
  is behavior-preserving (see REQ-REFACTOR-080), NOT an intended behavior change.
- **REQ-REFACTOR-063** — The `MarketQuery { market_id: 0 /* dummy */ }` construction (4 sites:
  `live_poller.rs`, `backfill.rs`, `collection_queue.rs` ×2) shall be replaced by a keyed enum
  (`CoinKeyed { coin_id, symbol } | MarketKeyed { market_id }`) or equivalent that makes the dummy
  `market_id: 0` sentinel unrepresentable.
- **REQ-REFACTOR-064** — The persisted `interval` strings shall remain unchanged and no stored data
  shall be migrated (no migration file is added). The storage vocabulary shall remain wider than
  the API-accepted set, now expressed as the negation of `is_api_facing()` over the single
  `ApiInterval` enum rather than a second table; internal callers (`candles_agg`, `rollup`,
  `cycle_overlay`, `backfill`) shall map any persisted interval to seconds via
  `ApiInterval::from_str(..).ok().map(|i| i.secs())`, preserving the prior exclusion semantics for
  unparseable/non-fixed-duration intervals.
- **REQ-REFACTOR-070** — The provider chain type shall become `Arc<[Arc<dyn Provider>]>`, replacing
  `Arc<Vec<Arc<dyn Provider>>>` (F-56).
- **REQ-REFACTOR-071** — The dead `CgMarketItem.vs_currency` field shall be removed (F-56).
- **REQ-REFACTOR-072** — The legacy `coingecko_days_to_interval` helper shall be removed and its
  tests migrated (F-56).

### Cross-cutting requirements (behavior preservation + quality + docs)

- **REQ-REFACTOR-080** — Every change other than REQ-REFACTOR-021 (a) and REQ-REFACTOR-042 (b)
  shall be characterization-equivalent: observable behavior shall be unchanged.
- **REQ-REFACTOR-081** — The refactor shall use `rust_decimal::Decimal` for all price/monetary
  values (never f64), shall read all config from env vars only, and shall add no new dependencies.
- **REQ-REFACTOR-082** — `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo fmt --check`, and `cargo test` (non-DB-gated) shall all pass.
- **REQ-REFACTOR-083** — Provider and collector module docs shall document the new shared helpers
  (`chain_try`, lease-queue scaffold, shared batched upsert); @MX tags shall be updated where an
  enforcement point moves — the pacer `@MX:WARN` moves into `chain_try` (REQ-REFACTOR-021), and the
  NOTIFY-policy and pacing-attribution changes shall be noted at their new sites.

## Exclusions

This SPEC is a Phase-7 refactor. The following are explicitly NOT in scope.

### Out of Scope — behavioral defect fixes (Phases 1–6)

- All F-01..F-49 behavioral defect fixes — they are the subject of Phases 1–6 and are assumed
  already corrected. Phase 7 lands on top of that corrected baseline and MUST NOT re-open them.

### Out of Scope — additional F-56 / optional-craft items

- Config `Config::from_env()` validated-snapshot refactor (folded into Phase 6 as optional).
- Edition-2024 migration (`cargo fix --edition`).
- `User-Agent` header on provider clients (Phase 3 client-helper concern).
- Any F-56 item other than the three selected: `Arc<[…]>`, `CgMarketItem.vs_currency`,
  `coingecko_days_to_interval`.

### Out of Scope — stored-data changes

- Migrating any persisted `interval` strings, `coin_candles`/`coin_quotes` rows, or any schema
  change. No migration files are added. The stored interval vocabulary stays as-is (DEC-2).

### Out of Scope — OHLC chain restructuring

- Folding `chain_fetch_ohlc{,_range}` into `chain_try`. Their continue-on-empty semantics (D5)
  are preserved unchanged; only the four non-OHLC loops are consolidated.

### Out of Scope — CoinDirectory trait extraction (Opt B)

- Extracting a separate CoinGecko-only `CoinDirectory` trait for the `search_coins` /
  `fetch_coin_tickers` pair and rewiring the API search path (F-50 Opt B). Per the DEC-3 Opt-A
  resolution this SPEC keeps the pair on the single `Provider` trait with an `Ok(vec![])` default.
  Opt B is filed as future **SPEC-COINDIR-001**.

## @MX Annotation Targets (high fan_in)

- **`chain_try`** — `@MX:ANCHOR` (fan_in ≥ 3: spot/metadata/market callers + tests). The pacer
  `@MX:WARN` (fleet-wide egress governor invariant) MOVES here from the per-worker
  `first_provider_for_cap` sites, because the enforcement point moves inside the loop
  (REQ-REFACTOR-021).
- **Shared lease-queue scaffold** — `@MX:ANCHOR` (fan_in ≥ 2: collection_queue + backfill). Note
  the watch-based heartbeat stop (REQ-REFACTOR-031).
- **Shared batched candle upsert** — `@MX:ANCHOR` (fan_in ≥ 3). MUST note the D1 two-policy split
  (native unconditional vs rollup native-wins guard) and the F-51 NOTIFY policy (no NOTIFY on
  backfill). The existing `rollup::batched_upsert_candles` native-wins `@MX:ANCHOR` stays.
- **`ApiInterval::secs()`** — `@MX:ANCHOR` (very high fan_in — inherits the 59 references migrated
  off `interval_to_seconds`) as the single total interval→seconds source for BOTH the API boundary
  and every internal caller. The prior `interval_to_seconds` `@MX:ANCHOR` is retired (the function
  is removed). The anchor MUST note: `secs()` is total (no `Option`, no `.expect`); the API
  boundary restricts to `is_api_facing()` (storage-only intervals → 400).
- **`paginate<T, K>`** — `@MX:NOTE` (fan_in ≥ 3).

## Open Items

- **OR-REFACTOR-1 [RESOLVED at Kickoff → Opt A]** — F-50 CoinDirectory split resolved to Opt A
  (single `Provider` trait, search pair `Ok(vec![])` default). Opt B is OUT OF SCOPE (see
  Exclusions), filed as future **SPEC-COINDIR-001**. See DEC-3 and plan.md §D.
- **OR-REFACTOR-2 [RESOLVED at Kickoff → LITERAL-ANCHOR]** — `interval_to_seconds` disposition
  resolved to the literal-anchor path: BOTH `SUPPORTED_INTERVALS` and `interval_to_seconds` are
  removed and fold into a single `ApiInterval` enum covering the full fixed-duration vocabulary
  (API-facing + storage-only), with a **total** `ApiInterval::secs()` and an `is_api_facing()`
  boundary predicate that preserves the existing 400 for storage-only intervals. Behavior-preserving
  (identical seconds, identical API 400). See DEC-2 and REQ-REFACTOR-060..064.
- **OR-REFACTOR-3 [run-phase choice — remains open]** — Whether the generalized native-write
  batcher and the rollup native-wins batcher are (i) two thin functions over one UNNEST core, or
  (ii) one function with a conflict-policy parameter. Either satisfies D1/REQ-REFACTOR-041; the
  implementer chooses the simpler shape at run-phase. Recorded so the two conflict policies are
  never conflated (this is an implementation-shape choice, not a plan-blocking clarification).
