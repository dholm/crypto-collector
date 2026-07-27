# Acceptance Criteria — SPEC-REFACTOR-001 (Phase 7: Batching & Structural Debt Reduction)

Given/When/Then scenarios, edge cases, quality gates, and a Definition of Done. Each scenario maps
to REQ IDs. DB-gated scenarios are `#[ignore]`, require a live PostgreSQL, and MUST run with
`--test-threads=1` (shared global claim queue) — per repo convention they gate the `completed`
transition, not the merge.

## Scenarios

### M1 — Provider trait defaults (F-50)

**AC-REFACTOR-010 — capability-derived defaults** (REQ-REFACTOR-010)
- **Given** the `Provider` trait with capability-derived default bodies,
- **When** a provider that does not support a capability is asked for it,
- **Then** the default body returns `Err(NotSupported(<capability>))` (or `Ok(vec![])` for the
  search pair), and only `name()`/`supports()` are mandatory (no default).

**AC-REFACTOR-011a — object-safety preserved** (REQ-REFACTOR-011)
- **Given** the trait after defaults are added,
- **When** the crate compiles,
- **Then** `Arc<dyn Provider>` construction and `build_chain` compile unchanged (object-safety holds).

**AC-REFACTOR-012a — zero stub bodies** (REQ-REFACTOR-012, verbatim anchor)
- **Given** the refactored providers,
- **When** `grep -c NotSupported src/providers/coinbase.rs src/providers/kraken.rs` runs,
- **Then** both report `0` — coinbase.rs and kraken.rs contain zero `NotSupported` stub bodies.

**AC-REFACTOR-013a — test doubles shed dead methods** (REQ-REFACTOR-013, verbatim anchor)
- **Given** the test doubles in `providers/mod.rs`,
- **When** the suite compiles,
- **Then** each double defines only the methods it uses (no dead `NotSupported` stub methods).

**AC-REFACTOR-014a — Opt-A search-pair default** (REQ-REFACTOR-014, DEC-3 → Opt A)
- **Given** the DEC-3 Opt-A resolution,
- **When** M1 is complete,
- **Then** the `search_coins` / `fetch_coin_tickers` pair remain on the single `Provider` trait with
  an `Ok(vec![])` default, no separate `CoinDirectory` trait is introduced (that is SPEC-COINDIR-001),
  and AC-REFACTOR-010..013 all hold.

### M2 — chain_try + per-provider pacing (F-53a, F-16)

**AC-REFACTOR-020a — one fallback loop** (REQ-REFACTOR-020, verbatim anchor)
- **Given** the consolidated chain code,
- **When** the non-OHLC fallback paths are inspected,
- **Then** exactly ONE `chain_try` implementation remains — the four prior loops
  (`chain_fetch_spot`, `chain_fetch_spot_local`, `chain_fetch_coin_metadata`,
  `chain_fetch_coin_market`) are gone.

**AC-REFACTOR-020b — chain_try characterization** (REQ-REFACTOR-020/023)
- **Given** a chain of test providers,
- **When** `chain_try` runs with (i) primary success, (ii) primary error then fallback success,
  (iii) a mix where the first capable provider is unsupported for the capability, (iv) a non-empty
  all-unsupported chain,
- **Then** (i) returns the primary result and records primary success + chain success; (ii) returns
  the fallback result and records the primary network-failure + fallback success; (iii) skips the
  unsupported member; (iv) returns `NoCapableProvider(capability)` (not an empty-chain label).

**AC-REFACTOR-021a — per-provider pacing attribution [INTENDED CHANGE a]** (REQ-REFACTOR-021)
- **Given** a chain where the primary fails and a fallback serves the request,
- **When** `chain_try` runs,
- **Then** the pacer slot is acquired for EACH attempted provider and the serving provider's
  cooldown is signaled — the slot is charged to the provider that actually serves, not to the first
  capability-supporting member (the prior behavior).

**AC-REFACTOR-022a — OHLC regression tests green + unmodified** (REQ-REFACTOR-022)
- **Given** `chain_fetch_ohlc` / `chain_fetch_ohlc_range`,
- **When** the suite runs,
- **Then** their regression tests — including `chain_fetch_ohlc_range_error_not_masked_by_earlier_empty`
  — are GREEN and their source is unmodified (continue-on-empty / error-surfacing preserved).

### M3 — Shared lease-queue scaffold (F-53b)

**AC-REFACTOR-030a — one lease-queue scaffold** (REQ-REFACTOR-030, verbatim anchor)
- **Given** `collection_queue` and `backfill`,
- **When** the claim/heartbeat/complete/release scaffolding is inspected,
- **Then** exactly ONE shared parameterized scaffold implements it (no duplicated scaffold).

**AC-REFACTOR-031a — watch-based heartbeat stop** (REQ-REFACTOR-031)
- **Given** the shared heartbeat task,
- **When** a lease completes/releases,
- **Then** the heartbeat is stopped via a `watch` signal (not `abort()`), verified by a
  characterization test.

**AC-REFACTOR-032a — classification preserved** (REQ-REFACTOR-032)
- **Given** the shared scaffold,
- **When** transient / permanent / soft-skip outcomes occur,
- **Then** the SPEC-SCHED-002 classification holds unchanged (no F-02 reintroduction).

### M4 — Batched writes + NOTIFY policy (F-51, F-52)

**AC-REFACTOR-040a — shared batcher on both hot paths** (REQ-REFACTOR-040)
- **Given** the shared UNNEST batcher in the db layer,
- **When** the candles dispatch path and the backfill page-write path run,
- **Then** both write through the shared batcher (no per-row candle upsert loop remains on the hot
  paths).

**AC-REFACTOR-041a — batch parity + two conflict policies** (REQ-REFACTOR-041, D1) `[DB-gated]`
- **Given** N candles,
- **When** written via the batched upsert vs N single `upsert_coin_candle` calls,
- **Then** the resulting rows are identical (parity); AND a native provider row is NOT overwritten
  by a colliding rollup batch (rollup native-wins guard intact), WHILE a native-write batch DOES
  unconditionally update on conflict (native path has no `rollup:%` guard).

**AC-REFACTOR-042a — NOTIFY policy [INTENDED CHANGE b]** (REQ-REFACTOR-042, verbatim anchor) `[DB-gated, LISTEN]`
- **Given** a `LISTEN coin_candle_updated`,
- **When** a live-poll candle upsert runs and, separately, a backfill run writes a page,
- **Then** the live-poll upsert emits exactly ONE NOTIFY and the backfill run emits ZERO NOTIFYs.

**AC-REFACTOR-043a — cycle overlay batched in one tx** (REQ-REFACTOR-043) `[DB-gated]`
- **Given** `recompute_cycle_overlay`,
- **When** it rebuilds,
- **Then** each model group is inserted via a single UNNEST inside the one DELETE+INSERT
  transaction, and the resulting rows equal the prior per-row result (idempotent-rebuild parity).

### M5 — API dedup (F-53c, F-55)

**AC-REFACTOR-050a — one ensure_coin_exists** (REQ-REFACTOR-050, verbatim anchor)
- **Given** the API layer,
- **When** `grep -rn "fn ensure_coin_exists" src/api/` runs,
- **Then** exactly ONE definition remains.

**AC-REFACTOR-051a — one column-list const** (REQ-REFACTOR-051)
- **Given** `coins.rs`,
- **When** the tracked_coins column list is inspected,
- **Then** it is a single `concat!`-assembled const, not inlined 5×.

**AC-REFACTOR-052a — one paginator** (REQ-REFACTOR-052, verbatim anchor)
- **Given** the API layer,
- **When** the paginators are inspected,
- **Then** exactly ONE generic `paginate<T, K: Serialize>` remains; the three prior paginators are
  gone, and each caller's cursor semantics are preserved (existing paginator tests migrated + green).

**AC-REFACTOR-053a — dead AppState fields removed** (REQ-REFACTOR-053)
- **Given** `AppState`,
- **When** the struct is inspected,
- **Then** `http_client` and `coingecko_base_url` are absent and the crate compiles (no handler read
  them).

**AC-REFACTOR-054a — shared AppState::test()** (REQ-REFACTOR-054)
- **Given** the API test modules,
- **When** they build test state,
- **Then** they use one shared `#[cfg(test)] AppState::test()` constructor (no 6+ duplicated
  builders).

### M6 — Domain typing (F-54, F-56)

**AC-REFACTOR-060a — ApiInterval round-trip + total secs() exhaustiveness** (REQ-REFACTOR-060)
- **Given** `ApiInterval` covering the full fixed-duration vocabulary (`{1m, 5m, 15m, 1h, 4h, 1d, 1w}`
  API-facing PLUS `{3m, 30m, 2h, 6h, 8h, 12h, 3d, 4d}` storage-only),
- **When** every variant is parsed and re-serialized and `secs()` is called,
- **Then** `from_str(as_str(x)) == x` for all variants; `secs()` returns an `i64` for EVERY variant
  (total — no `Option`, no `.expect`) with values matching the retired `interval_to_seconds` table
  (e.g. `1m`→60, `3m`→180, `4h`→14400, `1w`→604800); and a non-fixed-duration string (`1M`) fails
  `FromStr` (excluded exactly as `interval_to_seconds("1M")` returned `None`).

**AC-REFACTOR-061a — both tables removed literally + no .expect** (REQ-REFACTOR-061, verbatim anchor)
- **Given** the refactored tree,
- **When** `grep -rn 'SUPPORTED_INTERVALS\|fn interval_to_seconds\|validated interval must have a known second count' src/` runs,
- **Then** it returns no matches — the standalone `SUPPORTED_INTERVALS` const AND the standalone
  `interval_to_seconds` fn are BOTH gone (folded into `ApiInterval`), and the `.expect` interval-sync
  panic path no longer exists. `ApiInterval::secs()` carries the `@MX:ANCHOR` (the 59 former
  `interval_to_seconds` references now resolve through it).

**AC-REFACTOR-062a — API-facing guard predicate** (REQ-REFACTOR-062)
- **Given** `ApiInterval::is_api_facing()` (or `from_api_str`),
- **When** the public API validation runs for an API-facing interval (`4h`) vs a storage-only
  interval (`3m`),
- **Then** `is_api_facing()` is `true` for the API-facing set `{1m, 5m, 15m, 1h, 4h, 1d, 1w}` and
  `false` for the storage-only set — the public handler admits only the API-facing subset.

**AC-REFACTOR-062b — storage-only interval → 400 (characterization, behavior-preserving)** (REQ-REFACTOR-062, verbatim anchor)
- **Given** the candles endpoint after `SUPPORTED_INTERVALS` removal,
- **When** `GET /v1/coins/{id}/candles?interval=3m` (a storage-only interval) is requested via the
  public API,
- **Then** it returns 400 (`BadRequest`) WITHOUT issuing a query — identical to the prior
  `SUPPORTED_INTERVALS.contains` rejection. This is a characterization test asserting the behavior is
  UNCHANGED (NOT an intended behavior change). Any non-API-facing / unparseable string (`2h`, garbage)
  likewise returns 400.

**AC-REFACTOR-063a — dummy market_id unrepresentable** (REQ-REFACTOR-063)
- **Given** the keyed `MarketQuery` enum,
- **When** the 4 prior `market_id: 0 /* dummy */` sites are inspected,
- **Then** none construct a dummy sentinel; the coin-keyed path uses `CoinKeyed { coin_id, symbol }`
  and the sentinel `0` is not expressible.

**AC-REFACTOR-064a — stored vocabulary unchanged, one-enum predicate** (REQ-REFACTOR-064, DEC-2)
- **Given** the wider stored interval vocabulary (`3m`, `30m`, `2h`, `6h`, `8h`, `12h`, `3d`, `4d`),
- **When** the aggregation / rollup / overlay / backfill layers map a persisted interval to seconds,
- **Then** they resolve it via `ApiInterval::from_str(..).ok().map(|i| i.secs())` over the full
  vocabulary (preserving the prior exclusion for unparseable/non-fixed intervals), no stored
  `interval` data is migrated, and no migration file is added; the storage-vs-API width difference is
  now expressed as `!is_api_facing()` over the single enum rather than a second table.

**AC-REFACTOR-070a — Arc<[…]> chain** (REQ-REFACTOR-070)
- **Given** the provider chain type,
- **When** inspected,
- **Then** it is `Arc<[Arc<dyn Provider>]>` (not `Arc<Vec<Arc<dyn Provider>>>`).

**AC-REFACTOR-071a — CgMarketItem.vs_currency removed** (REQ-REFACTOR-071, verbatim anchor)
- **Given** `CgMarketItem`,
- **When** inspected,
- **Then** the dead `vs_currency` field is absent.

**AC-REFACTOR-072a — coingecko_days_to_interval removed** (REQ-REFACTOR-072, verbatim anchor)
- **Given** the CoinGecko provider,
- **When** `grep -rn coingecko_days_to_interval src/` runs,
- **Then** the helper is gone and its former tests are migrated (not deleted-with-coverage-loss).

### Cross-cutting

**AC-REFACTOR-080a — behavior preservation** (REQ-REFACTOR-080)
- **Given** the pre-existing non-DB test suite,
- **When** it runs after each milestone,
- **Then** it stays GREEN; the only observably changed behaviors are REQ-REFACTOR-021 (per-provider
  pacing) and REQ-REFACTOR-042 (no backfill NOTIFY), each covered by its own updated/new test.

**AC-REFACTOR-081a — no new deps, Decimal money, env config** (REQ-REFACTOR-081)
- **Given** the change set,
- **When** `Cargo.toml`/`Cargo.lock` and the money/config paths are inspected,
- **Then** no dependency is added, all prices/monetary values use `rust_decimal::Decimal` (no f64),
  and config is env-only.

**AC-REFACTOR-082a — quality gates green** (REQ-REFACTOR-082, verbatim anchor)
- **Given** the final tree,
- **When** `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and
  `cargo test` (non-DB) run,
- **Then** all pass — all quality gates green.

**AC-REFACTOR-083a — docs + @MX updated** (REQ-REFACTOR-083)
- **Given** the new shared helpers,
- **When** the module docs and @MX tags are inspected,
- **Then** `chain_try` / lease-queue scaffold / shared batcher are documented; the pacer `@MX:WARN`
  now sits at `chain_try`; the NOTIFY-policy and pacing-attribution changes are noted at their sites.

## Edge Cases

- **Empty vs all-unsupported chain** — `chain_try` on a genuinely empty chain vs a non-empty
  all-unsupported chain must yield distinct errors (empty-chain vs `NoCapableProvider`), per
  AC-REFACTOR-020b(iv).
- **D1 conflation trap** — a native provider write colliding with an existing rollup row must
  overwrite it (native path, unconditional DO UPDATE); a rollup write colliding with a native row
  must NOT (rollup guard). Both directions asserted (AC-REFACTOR-041a).
- **Backfill NOTIFY removal does not silence live** — live-poll NOTIFY path unaffected
  (AC-REFACTOR-042a).
- **API-boundary rejects stored-only intervals (behavior-preserving)** — after removing
  `SUPPORTED_INTERVALS`, `3m`/`2h` (valid `ApiInterval` variants but `!is_api_facing()`) STILL return
  400 at the public API boundary, while remaining resolvable internally via `ApiInterval::secs()`
  (AC-REFACTOR-062b + AC-REFACTOR-064a). `1M` (not a variant) fails `FromStr` and is excluded exactly
  as `interval_to_seconds` returned `None` (AC-REFACTOR-060a).
- **Object-safety** — no default body may introduce a generic parameter that breaks `dyn Provider`
  (AC-REFACTOR-011a).

## Quality Gate Criteria

- `cargo fmt --check` clean.
- `cargo clippy --all-targets --all-features -- -D warnings` clean.
- `cargo test` (non-DB: `model_serde`, `migration_files`, pure-core, unit) green.
- DB-gated suite (`#[ignore]`, live Postgres, `--test-threads=1`) green — gates `completed`.
- No new dependency in `Cargo.toml`/`Cargo.lock`; no f64 for money; env-only config.

## Definition of Done

- [ ] M1: trait defaults; coinbase.rs/kraken.rs zero `NotSupported` stubs; test doubles shed dead
      methods; object-safety preserved (REQ-REFACTOR-010..014).
- [ ] M2: one `chain_try`; per-provider pacing (intended change a); OHLC regression tests green +
      unmodified; empty-vs-unsupported distinction (REQ-REFACTOR-020..023).
- [ ] M3: one lease-queue scaffold; watch-based heartbeat stop; classification preserved
      (REQ-REFACTOR-030..032).
- [ ] M4: shared batcher on both hot paths; batch/single parity; two conflict policies distinct;
      NOTIFY live=1/backfill=0 (intended change b); cycle-overlay UNNEST in one tx
      (REQ-REFACTOR-040..043).
- [ ] M5: one `ensure_coin_exists`; one paginator; `concat!` column const; dead AppState fields
      removed; shared `AppState::test()` (REQ-REFACTOR-050..054).
- [ ] M6: `ApiInterval` full-vocab SSOT with total `secs()`; BOTH `SUPPORTED_INTERVALS` and
      `interval_to_seconds` removed literally; no `.expect` interval-sync; `is_api_facing()` guard
      keeps storage-only intervals (e.g. `3m`) at 400 (behavior-preserving, AC-REFACTOR-062b);
      keyed `MarketQuery` (dummy unrepresentable); stored vocabulary/data unchanged; `Arc<[…]>`;
      `CgMarketItem.vs_currency` removed; `coingecko_days_to_interval` removed + tests migrated
      (REQ-REFACTOR-060..072).
- [ ] Cross-cutting: behavior-preserving except the two named changes; no new deps / Decimal /
      env-only; all quality gates green; docs + @MX updated (REQ-REFACTOR-080..083).
- [ ] Verbatim anchors all satisfied: zero `NotSupported` stubs in coinbase/kraken; exactly one
      each of fallback loop / lease-queue scaffold / paginator / `ensure_coin_exists`; backfill=0 /
      live=1 NOTIFY; no `.expect` interval-sync and BOTH `SUPPORTED_INTERVALS` AND `interval_to_seconds`
      removed literally in favor of `ApiInterval` (with the `is_api_facing()` 400 guard preserved);
      all quality gates green.
- [ ] Open items status: OR-REFACTOR-1 → Opt A (Opt B deferred to SPEC-COINDIR-001); OR-REFACTOR-2
      → literal-anchor (both tables folded into `ApiInterval`); OR-REFACTOR-3 (batcher shape) chosen
      at run-phase.
- [ ] `completed` transition MAY hold pending the live-Postgres DB-gated run (`--test-threads=1`),
      per SPEC-PROV-002/003, SPEC-API-005, SPEC-OBS-002 close pattern.
