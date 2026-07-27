# Implementation Plan — SPEC-REFACTOR-001 (Phase 7: Batching & Structural Debt Reduction)

Tier L. Single SPEC, six milestones (M1–M6), **one reviewable commit per milestone**, direct
to `main` (trunk workflow, no feature branches). Behavior-preserving except two named changes.

> **Review-focus ordering note.** Milestones execute in dependency order M1→M6 (M2 builds on M1's
> trait defaults; M4's batcher builds on the M1/M2 provider surface; etc.). But §D below leads with
> the **highest-change-likelihood decisions** (the open trait-split decision, the two new domain
> types, and the two intended behavior changes) so human review at Implementation Kickoff Approval
> focuses on what is most likely to change, before the mechanical milestone walkthrough in §F.

## §A Context

The review (`research/idiomatic-rust.md` §3, §6) names duplication "the dominant idiomatic debt"
and states **every confirmed drift bug in the review maps onto a duplication site**. Phase 7 is
the consolidation phase, deliberately last so refactors land on a corrected baseline (§7 rationale).
The affected surfaces, grounded in cited code:

- **Provider trait** (`src/providers/mod.rs:269-355`): 9 methods, only `fetch_ohlc_range` has a
  default → ~450 lines of `NotSupported` stubs across Coinbase/Kraken (7 each) + 6 test doubles.
- **Four non-OHLC fallback loops**: `chain_fetch_spot_local` / `chain_fetch_coin_metadata` /
  `chain_fetch_coin_market` (`collection_queue.rs:355-468`), plus `chain_fetch_spot` in
  `live_poller.rs` (defined ~L476, called ~L404). Each repeats identical registry bookkeeping.
  Pacing is keyed on the *first* capable provider BEFORE the loop, so fallback traffic is
  mis-charged (F-16): `collection_queue.rs` calls `first_provider_for_cap` (defined
  `collection_queue.rs:471`; called at 516/624/696/760) and paces on it; `live_poller.rs` paces via
  `acquire_slot(pool, &provider_name)` (~L378) where `provider_name` is the first `Spot`-capable
  member (`chain.iter().find(|p| p.supports(Capability::Spot))`, ~L351), then runs `chain_fetch_spot`
  (~L404). `first_provider_for_cap` itself lives ONLY in `collection_queue.rs` (not in live_poller).
- **Two lease-queue scaffolds**: claim/heartbeat/complete/release duplicated between
  `collection_queue` and `backfill`; heartbeat stopped via `abort()`.
- **Per-row candle writes + per-row NOTIFY** (`db/upserts.rs:44-150`) in dispatch
  (`collection_queue`, ~2016 rows/refresh) and backfill (`backfill.rs`, ~1000 rows/page). The
  UNNEST model already exists and is proven: `rollup::batched_upsert_candles`
  (`rollup.rs:175-225`) — but it carries a rollup-only native-wins WHERE guard the native path
  must NOT inherit (D1). `recompute_cycle_overlay` (`cycle_overlay.rs:397-439`) issues 4000–8000
  single-row INSERTs in one tx.
- **API dedup**: `ensure_coin_exists` in both `quotes.rs:231` and `metadata.rs:86`; three
  paginators (`paginate_coins` coins.rs:350, `paginate_ts` quotes.rs:244, `paginate_cycle_overlay`
  cycle_overlay.rs:400); tracked_coins column list inlined 5× in `coins.rs`; two dead `AppState`
  fields (`http_client`, `coingecko_base_url`, `api/mod.rs:56/58`) populated in 12 test builders.
- **Domain typing**: `SUPPORTED_INTERVALS` (7 API values, candles.rs:35) + `interval_to_seconds`
  (15 stored values, candles_agg.rs:28) coupled by `.expect` at candles.rs:164/193;
  `MarketQuery { market_id: 0 }` at 4 sites.

## §B Known Issues & Risks

> Both plan-phase open decisions were **resolved at Implementation Kickoff Approval** — DEC-3 → Opt A
> (Opt B deferred to SPEC-COINDIR-001); OR-REFACTOR-2 → literal-anchor (see §D). No open
> clarification markers remain.

- **Risk — API-boundary interval guard (literal-anchor path).** Removing the `SUPPORTED_INTERVALS`
  allow-list must NOT let a public API client request storage-only intervals. `ApiInterval` covers
  the full vocabulary, so the public handler MUST validate against `is_api_facing()` (or use
  `from_api_str`) and return the existing 400 for a storage-only interval (e.g. `3m`). This is
  behavior-preserving. Mitigation: REQ-REFACTOR-062 + AC-REFACTOR-062a/062b (a characterization test
  asserting `GET candles?interval=3m → 400`).
- **Risk — `ApiInterval::secs()` totality.** `secs()` must be total (no `Option`, no `.expect`).
  Non-fixed-duration strings (`1M`) MUST NOT be variants — they fail `FromStr` and are excluded
  exactly as `interval_to_seconds` returned `None`. Mitigation: REQ-REFACTOR-060 + AC-REFACTOR-060a
  (round-trip + exhaustiveness).
- **Risk — conflict-policy conflation (D1).** The single biggest correctness trap: if the
  generalized native-write batcher inherits the rollup `WHERE source LIKE 'rollup:%'` guard, native
  provider writes silently become no-ops on conflict. Mitigation: REQ-REFACTOR-041 + AC-REFACTOR-041a
  assert both policies explicitly; the two paths are never merged into one unconditional function.
- **Risk — pacing behavior change (F-16) altering test expectations.** Moving `acquire_slot` inside
  the loop changes which provider is charged. Mitigation: characterization test AC-REFACTOR-021a
  asserts the NEW attribution; the OHLC regression tests (AC-REFACTOR-022a) pin the unchanged path.
- **Risk — NOTIFY removal breaking live consumers.** Backfill NOTIFY removal must not affect the
  live-poll NOTIFY path. Mitigation: AC-REFACTOR-042a asserts live=1 NOTIFY, backfill=0 via LISTEN
  (DB-gated).
- **Risk — object-safety regression (M1).** Adding defaults could accidentally introduce a generic
  method and break `dyn Provider`. Mitigation: `build_chain` + `Arc<dyn Provider>` construction
  compile-check is the guard (AC-REFACTOR-011a).
- **Risk — refactor/fix merge conflict.** Phase 7 is last precisely to avoid this; ensure Phases
  1–6 are merged before starting (§C).

## §C Pre-flight

1. Confirm Phases 1–6 are merged to `main` and the tree is green: `git log --oneline -8`,
   `cargo test`, `cargo clippy --all-targets --all-features -- -D warnings`.
2. Capture the behavior-preservation baseline: run the full pre-existing test suite and record the
   pass count — every M-commit must keep it green (minus the two intended changes' updated tests).
3. OR-REFACTOR-1 (→ Opt A) and OR-REFACTOR-2 (→ literal-anchor) are already resolved at Kickoff;
   OR-REFACTOR-3 (batcher shape) is a run-phase implementation choice, not a plan blocker.
4. Verify no in-flight parallel session on this SPEC (shared `main`, single-checkout).

## §D Constraints & Decisions (review-priority — read first)

### DEC-3 (RESOLVED at Kickoff → Opt A) — F-50 CoinDirectory split

The `search_coins` / `fetch_coin_tickers` pair are directory concerns implemented by exactly one
provider (CoinGecko). **Resolved to Opt A**: keep the single `Provider` trait with the search pair
defaulting to `Ok(vec![])`. **Opt B** (extract a CoinGecko-only `CoinDirectory` trait + rewire the
API search path) is OUT OF SCOPE and filed as future **SPEC-COINDIR-001**. Rationale table retained
for the deferral record:

| Axis | **Opt A — ADOPTED (single `Provider` trait, search pair → `Ok(vec![])` default)** | **Opt B — DEFERRED to SPEC-COINDIR-001** |
|------|-----------------------------------------------------------------------------------|-------------------------------------------|
| Change size | Smaller — pure default-body addition; no call-site rewire | Larger — new trait, CoinGecko impl move, API search path rewire |
| Trait honesty | Search stays a lopsided member every non-CoinGecko provider "supports" as empty | Search modeled as a distinct capability; non-directory providers never see it |
| Object-safety | Unchanged | Two object-safe traits; API holds an `Arc<dyn CoinDirectory>` |
| Risk | Lowest — no behavior surface touched | Medium — API search path externally visible; needs its own regression cover |
| Future correctness | Leaves the "no `Capability::Search`" smell noted in F-50 (deferred) | Resolves it |

**Why Opt A now**: it is the minimal, lowest-risk change that satisfies the F-50 intent (kill the
stub boilerplate) and keeps Phase 7 a clean behavior-preserving refactor. Opt B's externally-visible
API-search rewire is better scoped as its own SPEC (SPEC-COINDIR-001). REQ-REFACTOR-014 encodes the
Opt-A form; M1 acceptance is the Opt-A form.

### New domain types (M6) — highest type-shape change likelihood

- **`ApiInterval`** — one enum covering the **full fixed-duration vocabulary**: the API-facing set
  `{1m, 5m, 15m, 1h, 4h, 1d, 1w}` PLUS the storage-only set `{3m, 30m, 2h, 6h, 8h, 12h, 3d, 4d}`
  (15 variants total). Exposes `FromStr` (parse-fail excludes non-fixed strings like `1M`, exactly as
  `interval_to_seconds` returned `None`), a **total** `secs()` (→ `i64`, one value per variant, no
  `.expect`, no `Option`), `as_str()`, and an `is_api_facing()` boundary predicate. **Replaces BOTH**
  `SUPPORTED_INTERVALS` and `interval_to_seconds` literally (OR-REFACTOR-2 → literal-anchor).
  `ApiInterval::secs()` becomes the new `@MX:ANCHOR`, inheriting the 59 references measured on
  `interval_to_seconds` (2026-07-27) across `candles_agg`/`rollup`/`cycle_overlay`/`backfill`/
  `coingecko` + tests.
- **API-boundary guard (critical, characterization-preserving)** — the public API handler
  (`validate_interval` today) MUST restrict to the API-facing subset via `is_api_facing()` (or a
  `from_api_str` that rejects storage-only variants). A public request for a storage-only interval
  (e.g. `3m`) STILL returns the existing 400 — identical to the prior `SUPPORTED_INTERVALS.contains`
  behavior. Internal callers (`candles_agg`, `rollup`, `cycle_overlay`, `backfill`) use
  `ApiInterval::from_str(..).ok().map(|i| i.secs())` over the full vocabulary.
- **Keyed `MarketQuery`** — replace the `market_id: 0 /* dummy */` sentinel with
  `CoinKeyed { coin_id, symbol } | MarketKeyed { market_id }` (or equivalent) so the dummy is
  unrepresentable. Touches the 4 collector construction sites + the provider consumers.

### Two intended behavior changes (everything else is characterization-equivalent)

- **(a) F-16 — per-provider chain-fallback pacing** (REQ-REFACTOR-021): `acquire_slot` +
  `signal_cooldown` move INSIDE `chain_try`, charged to the attempted/serving provider.
- **(b) F-51 — no NOTIFY on backfill** (REQ-REFACTOR-042): live path keeps per-event NOTIFY;
  backfill path emits none.

### Hard constraints (D1–D5, restated in spec.md)

Decimal-only money; env-only config; **no new dependencies**; trait object-safety preserved;
D1 two-policy batcher split; D2 declaration-order fallback; D5 OHLC continue-on-empty preserved;
`cargo clippy -D warnings` + `cargo fmt --check` + `cargo test` green.

## §E Self-Verification (plan-phase)

- [x] Full Tier-L **5-artifact set** created under `.moai/specs/SPEC-REFACTOR-001/`:
      **spec.md + plan.md + acceptance.md + design.md + research.md** (+ progress.md for the §E
      lifecycle skeleton). Matches the Tier-L artifact count (schema-frontmatter § SPEC Complexity
      Tier: Tier L = 5 files).
- [x] 12-field frontmatter, GEARS notation, `tier: L`.
- [x] Two intended behavior changes isolated to REQ-REFACTOR-021 and REQ-REFACTOR-042; all other
      requirements tagged behavior-preserving (REQ-REFACTOR-080) — including the literal-anchor
      interval work (REQ-REFACTOR-060..064), which is behavior-preserving, NOT an intended change.
- [x] Trait object-safety (REQ-REFACTOR-011), no-new-deps (REQ-REFACTOR-081) encoded.
- [x] DEC-3 resolved → Opt A (Opt B out of scope → SPEC-COINDIR-001); M1 acceptance is the Opt-A
      form (REQ-REFACTOR-014).
- [x] OR-REFACTOR-2 resolved → literal-anchor: both `SUPPORTED_INTERVALS` and `interval_to_seconds`
      fold into `ApiInterval`; API-facing guard (`is_api_facing()`) preserves the 400 for
      storage-only intervals (REQ-REFACTOR-060..064, AC-REFACTOR-062a/062b).
- [x] Both plan-phase clarification markers removed from §B (decisions resolved at Kickoff — no
      unresolved clarification markers remain in any artifact).

## §F Milestones (execution order — one commit each)

Each milestone is annotated with its **change-likelihood** (review priority) so the reviewer knows
where the reversible decisions concentrate.

### M1 — Provider trait capability-derived defaults `[change-likelihood: LOW — Opt A resolved]`
- Add capability-derived default bodies to every fetch method; only `name()`/`supports()` mandatory
  (REQ-REFACTOR-010). Search pair → `Ok(vec![])` default on the single `Provider` trait (Opt A per
  DEC-3; Opt B `CoinDirectory` is out of scope → SPEC-COINDIR-001).
- Shrink Coinbase/Kraken to real content; shed dead test-double methods (REQ-REFACTOR-012/013).
- Guard: `Arc<dyn Provider>` + `build_chain` compile unchanged (REQ-REFACTOR-011).
- Commit: `refactor(SPEC-REFACTOR-001): M1 provider trait capability-derived defaults (F-50)`.

### M2 — Generic chain fallback + honest per-provider pacing `[change-likelihood: HIGH — behavior change a]`
- Extract `chain_try<T>(chain, capability, registry, f)` owning registry bookkeeping; replace the
  four loops (REQ-REFACTOR-020). Preserve empty-vs-unsupported distinction (REQ-REFACTOR-023).
- **Intended change (a)**: move `acquire_slot` + `signal_cooldown` inside the loop, per attempted
  provider (REQ-REFACTOR-021). Move the pacer `@MX:WARN` into `chain_try`.
- Leave `chain_fetch_ohlc{,_range}` untouched; their regression tests stay green + unmodified
  (REQ-REFACTOR-022).
- New characterization tests: primary success, fallback-on-error, unsupported-skip, registry
  bookkeeping, per-provider pacing/cooldown attribution.
- Commit: `refactor(SPEC-REFACTOR-001): M2 chain_try + per-provider fallback pacing (F-53a, F-16)`.

### M3 — Shared lease-queue scaffold `[change-likelihood: MEDIUM]`
- Extract the parameterized claim/heartbeat/complete/release scaffold + heartbeat task shared by
  `collection_queue` and `backfill` (REQ-REFACTOR-030); prefer a `watch`-based heartbeat stop over
  `abort()` (REQ-REFACTOR-031); preserve SPEC-SCHED-002 classification (REQ-REFACTOR-032).
- Characterization tests for the scaffold + heartbeat stop.
- Commit: `refactor(SPEC-REFACTOR-001): M3 shared lease-queue scaffold (F-53b)`.

### M4 — Batched writes + NOTIFY policy `[change-likelihood: HIGH — behavior change b + D1 trap]`
- Generalize the UNNEST batcher into the shared db layer; dispatch + backfill page writes use it
  (REQ-REFACTOR-040). Keep the two conflict policies distinct (REQ-REFACTOR-041 / D1 / OR-REFACTOR-3).
- **Intended change (b)**: live path keeps per-event NOTIFY; backfill path emits none
  (REQ-REFACTOR-042).
- Batch `recompute_cycle_overlay` inserts via UNNEST per model group inside the existing single
  transaction (REQ-REFACTOR-043).
- DB-gated tests: N-row batch == N single upserts; NOTIFY live=1 / backfill=0 via LISTEN.
- Commit: `refactor(SPEC-REFACTOR-001): M4 batched candle writes + NOTIFY policy (F-51, F-52)`.

### M5 — API deduplication `[change-likelihood: LOW — mechanical]`
- Single `ensure_coin_exists` (REQ-REFACTOR-050); `concat!` column-list const (REQ-REFACTOR-051);
  generic `paginate<T, K: Serialize>` replacing the three paginators (REQ-REFACTOR-052); remove the
  two dead `AppState` fields (REQ-REFACTOR-053); shared `AppState::test()` (REQ-REFACTOR-054).
- Commit: `refactor(SPEC-REFACTOR-001): M5 API dedup — paginator/ensure_coin_exists/AppState (F-53c, F-55)`.

### M6 — Domain typing `[change-likelihood: HIGH — new types + literal-anchor guard]`
- `ApiInterval` enum covering the FULL fixed-duration vocabulary; total `secs()` (no `.expect`, no
  `Option`); remove BOTH `SUPPORTED_INTERVALS` and `interval_to_seconds` literally
  (REQ-REFACTOR-060/061). `ApiInterval::secs()` becomes the new `@MX:ANCHOR` (inherits the 59
  `interval_to_seconds` refs). **API-boundary guard**: `is_api_facing()` (or `from_api_str`) so a
  public request for a storage-only interval (e.g. `3m`) still returns the existing 400 —
  behavior-preserving (REQ-REFACTOR-062). Keyed `MarketQuery` enum removes the dummy sentinel at 4
  sites (REQ-REFACTOR-063). Stored `interval` strings unchanged; no data migration
  (REQ-REFACTOR-064). F-56 selections: `Arc<[…]>` (REQ-REFACTOR-070), remove `CgMarketItem.vs_currency`
  (REQ-REFACTOR-071), remove `coingecko_days_to_interval` + migrate tests (REQ-REFACTOR-072).
- Tests: `ApiInterval` round-trip + exhaustiveness (every variant has `secs()`); **storage-only
  interval `3m` via public API → 400 characterization** (AC-REFACTOR-062b); keyed-query dummy
  unrepresentable; internal callers map any stored interval via `from_str().ok().map(secs)`.
- Commit: `refactor(SPEC-REFACTOR-001): M6 domain typing — ApiInterval fold + keyed query + F-56 (F-54)`.

### Close — sync + DB-gated close
- `/moai sync`; @MX + module docs (REQ-REFACTOR-083); quality gates (REQ-REFACTOR-082).
- Per repo convention (SPEC-PROV-002/003, SPEC-API-005, SPEC-OBS-002), the final `completed`
  transition MAY hold pending a live-Postgres DB-gated run (`--test-threads=1`); non-DB tests gate
  the merge, DB-gated tests gate `completed`.

## §G Anti-Patterns (do NOT do)

- Do NOT fold `chain_fetch_ohlc{,_range}` into `chain_try` (D5).
- Do NOT let the native-write batcher inherit the rollup `WHERE source LIKE 'rollup:%'` guard (D1).
- Do NOT let `ApiInterval::secs()` become partial (`Option`) or introduce an `.expect` — it MUST be
  total over the variant set; exclude non-fixed strings (`1M`) at `FromStr`, not at `secs()`.
- Do NOT let the public API accept storage-only intervals — the `is_api_facing()` guard MUST keep
  the existing 400 for `3m` etc. (removing `SUPPORTED_INTERVALS` without the guard is a regression).
- Do NOT migrate stored `interval` data (DEC-2); the stored vocabulary stays wider than the
  API-accepted subset, now expressed as `!is_api_facing()` over one enum.
- Do NOT add a dependency, introduce f64 for money, or read config from anything but env.
- Do NOT drive-by-fix any F-01..F-49 behavioral item — Phase 7 is behavior-preserving.
- Do NOT change observable behavior anywhere except REQ-REFACTOR-021 (a) and REQ-REFACTOR-042 (b).

## §H Cross-References

- `.moai/specs/SPEC-REFACTOR-001/design.md` — adopted design (Opt A, ApiInterval full-vocab +
  `is_api_facing()` guard, keyed `MarketQuery`, the two intended behavior changes, shared-helper
  shapes, D1 two-policy split).
- `.moai/specs/SPEC-REFACTOR-001/research.md` — F-16/F-50..F-56 rationale (cited/summarized from
  `research/idiomatic-rust.md` §§3/6/7/8) + the codebase grounding measured 2026-07-27.
- `research/idiomatic-rust.md` §3, §6 (F-16, F-50..F-56), §7 (Phase 7 row), §8 (F-51 NOTIFY risk).
- SPEC-PROV-001 (Provider trait, build_chain, chain semantics), SPEC-SCHED-001/002 (workers, lease
  queue, retry classification), SPEC-CANDLE-001/002 (batched_upsert_candles, native-wins D1),
  SPEC-CYCLE-001 (recompute_cycle_overlay), SPEC-API-001/003 (candle intervals, aggregation).
- CLAUDE.md Key Invariants (Decimal-only money, env-only config, declaration-order fallback).
