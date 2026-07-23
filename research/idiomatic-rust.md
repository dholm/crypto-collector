# Crypto Collector — Architecture & Idiomatic Rust Review

Date: 2026-07-23
Toolchain evidence: `rustc 1.97.1`, edition 2021. `cargo clippy --all-targets --all-features -- -D warnings`: **clean**. `cargo fmt --check`: **clean**. `cargo test`: **all suites pass** (DB-bound tests correctly gated behind `#[ignore]` + `DATABASE_URL`). No `unsafe` anywhere in `src/`.

Scope: all of `src/` (~27.5K lines across providers, pacer, collectors, alarm, db, api, models, health, metrics, telemetry, config, listener, main), `tests/`, `migrations/` layout, `build.rs`, `Makefile`, both Dockerfiles, `api/crypto-collector.yaml`, and the alarm-center OpenAPI contract (sibling repo) for the alarm client.

---

## 1. Executive Summary

The service is in **good overall shape**: layered architecture matches the documented module map, the mechanical quality bar is high (clippy-clean under `-D warnings`, formatted, tested, zero `unsafe`), and several hard invariants — Decimal-only money (REQ-PROV-012), parameterized SQL everywhere, short-transaction claim/lease/fencing discipline, Kubernetes-correct startup/shutdown ordering — are not just documented but structurally enforced and regression-tested. There is no SQL injection surface and no `f64` in any monetary path.

The defects that do exist cluster in four systemic themes rather than being randomly scattered:

1. **Retry/backpressure policy is inconsistent and partly wrong** in the worker layer. The backfill worker conflates "pages processed" with "failures" (a long chunk is permanently failed by its *first* real error), treats normal pacer cooldown as a hard failure, and — like the collection-queue worker — busy-loops against the database with no sleep between claim cycles during cooldowns. These interlock into the single highest-operational-risk cluster: **a routine CoinGecko cooldown can silently and permanently kill deep backfill** (F-01/F-02/F-03).
2. **Duplication-bred drift.** The per-endpoint request/429/parse scaffolding is copy-pasted 6× in `coingecko.rs` alone, and the two newest endpoints (search/tickers) were written outside the frame — bypassing the pacer entirely and swallowing 429s without signaling cooldown (F-10). The same drift mechanism produced the backfill pacer-classification gap (fixed in one sibling worker, not the other) and the metric-name drift.
3. **Contract drift at boundaries.** Documented-vs-emitted metric names disagree (documented series never receive data, F-38); the OpenAPI spec documents `vs_currency` on quote reads that the handlers silently ignore (F-29); the spec documents 503s the search handler never returns (F-34); non-canonical `"daily"`/`"hourly"` interval stamps from the CoinGecko range path are invisible to every downstream interval table (F-20).
4. **Missing outer bounds.** No HTTP timeouts on any provider client (F-11), unbounded `coin_quotes` reads violating the codebase's own partition-pruning invariant (F-30), an unbounded shutdown `supervisor.await` (F-41), an unbounded live-poller claim batch vs. a fixed claim TTL (F-05).

Nothing found rises to Critical (no exploitable security hole, no data loss occurring in the currently-deployed configuration). Ten findings are High — each is either latent data loss/corruption (F-07, F-20, F-22), silent pipeline death (F-01/F-02/F-03), or an invariant violation with real operational consequences (F-10, F-11, F-21, F-29, F-30).

Seven implementation phases are recommended (Section 8); phases 1–2 (worker retry semantics, materializer data integrity) deliver the most risk reduction per line changed.

---

## 2. Overall Architectural Assessment

**Verdict: sound layering with well-chosen boundaries; weaknesses are policy gaps and duplication, not structural misdesign.**

- **Separation of concerns** is real: providers (upstream I/O) → collectors (scheduling/claiming) → db (persistence) → api (read side), with alarm/health/metrics/telemetry as cross-cutting concerns. Dependency direction is clean; no api→collectors or providers→api back-edges were found.
- **Startup/shutdown are carefully engineered** (`src/main.rs` steps 1–13): health listener binds and the shutdown orchestrator starts *before* the DB retry loop, so liveness stays answerable during a DB outage; `migrate_with_retry` (`src/db/pool.rs:82-126`) races capped-backoff migration attempts against the shutdown channel and is tested without a database. Shutdown ordering (readiness-503 → grace → broadcast → drain → `pool.close()` → trace flush) matches zero-drop rollout practice and is `@MX:ANCHOR`-documented. Residual gaps: the drain is an unconditional sleep plus an unbounded await (F-41), readiness flips ready slightly before the API socket binds (F-43), and the two PG LISTEN relay tasks sit outside the supervision umbrella (F-39).
- **Worker supervision** is correct in shape — per-worker `tokio::spawn` for panic isolation, restart gated on shutdown, restarts fed to the crash-loop alarm — but the restart policy is a fixed 5 s with no backoff, and the supervisor body is copy-pasted four times (F-46).
- **Transactional discipline in the worker layer is exemplary**: `FOR UPDATE SKIP LOCKED` claims committed before any network I/O, zombie fencing (`AND claimed_by = $self`) on every post-claim mutation, lease + heartbeat + self-expiring markers. The *policy* layered on top of that machinery (attempt counting, backpressure classification, inter-cycle pacing) is where the High defects live (F-01..F-05).
- **Dependency injection / testability** is above average for a service binary: pure decision cores extracted everywhere (`pacer_decision`, `is_market_due`, `reconcile_window`, `compute_sweep_actions`, the entire `candles_agg` module with injected `now`), `HealthState::for_test()`, wiremock for HTTP, `axum-test` for router-level tests, local metrics recorders. Components can genuinely be tested without infrastructure.
- **Resilience**: DB-down at startup is handled excellently; provider-down is handled by the fallback chain + alarm registry. The gaps are missing HTTP timeouts (a hung upstream wedges a worker forever, F-11), pacer-vs-fallback accounting (fallback traffic is unpaced, F-16), and the sampled `all_providers_down` flag making the one Critical alarm flappy (F-42).
- **Configuration** is env-only per convention, with pure helpers extracted for testability. Weaknesses: silent fallback-to-default on unparseable values (F-45), no single validated snapshot (F-56), credentials interpolated un-escaped into the DB URL (F-40), and the CoinGecko tier tri-state decision (capability/header/base-URL) split across two files with two different tier sets (F-21).
- **Observability** wiring is thorough (OTel propagation, route-template metric labels with a cardinality regression test, health caching) but has name drift between described and emitted metrics (F-38), a ghost gauge (F-49), and uninstrumented provider calls outside the queue worker.
- **Security posture** is good: distroless nonroot runtime, no secrets in code, parameterized SQL throughout, no `unsafe`. The API has no authentication/rate limiting — acceptable in-cluster, worth an explicit note before any external exposure (F-35 is the concrete resource-exhaustion vector).

## 3. Overall Idiomatic-Rust Assessment

**Verdict: high baseline; the gaps are typed-domain-modeling and error-classification, not mechanics.**

- **Ownership/borrowing/async mechanics are clean.** No await-while-holding-lock hazards anywhere (all `std::sync::Mutex` scopes are synchronous); `tokio::select!` used correctly with `biased` where drain priority matters; no blocking calls in async contexts found; clippy is clean at `-D warnings`.
- **The Decimal invariant is airtight**: `serde_json/arbitrary_precision` → `Number::to_string()` → `Decimal::from_str` with no f64 intermediary; `rust_decimal::serde::str` in DTOs; a migration-file test statically rejects `DOUBLE PRECISION`/`REAL` in DDL. The one latent gap is scientific-notation input (F-23) and `MathematicalOps::ln` panics on non-positive input (F-09).
- **Error handling** is principled at the boundaries — `thiserror`-style `ProviderError` enum in providers, a hand-rolled `ApiError` with correct 500-detail-hiding at the HTTP boundary, `anyhow` for startup plumbing — but degrades to `Result<_, String>` inside the queue dispatch path, erasing the transient/permanent distinction the retry semantics need (F-04), and `is_transient` misclassifies all HTTP errors as transient (F-12).
- **Type-system leverage is inconsistent.** `ProjectionModel` (enum + `FromStr` + single source of truth for validation/discovery/SQL) is the pattern done right; against it, candle intervals are stringly-typed with two hand-synced tables coupled by a runtime `.expect` (F-54), the CoinGecko tier is a raw `String` driving three decisions in two files (F-21), and `MarketQuery { market_id: 0 /* dummy */ }` appears four times (F-54).
- **Trait design**: the `Provider` trait is object-safe and appropriately `async-trait`/`Arc<dyn>`-based (dynamic dispatch is the right call for a runtime-configured chain), but 8 of 9 methods lack default bodies, producing ~450 lines of `NotSupported` stub boilerplate across implementors and test doubles, and the two search methods are lopsided directory concerns implemented by one provider (F-50).
- **Duplication** is the dominant idiomatic debt: request/parse/429 scaffolding (F-14), four chain-fetch helpers, four supervisors, two claim/heartbeat/release scaffolds, duplicated `ensure_coin_exists`, three paginators, a duplicated `HeaderExtractor` (F-53). Every confirmed drift bug in this review maps onto one of these duplication sites.
- **Allocation hygiene** is fine for the service's scale; the flagged sites (`as_array().cloned()`, per-scan `to_uppercase()`, `emitted.to_vec()`) are craft nits, not hot-path problems (F-28).
- Test code is extensive and mostly high-value (see Section 4), with a minority of vacuous "exists"-style tests (F-59).

---

## 4. Strengths (verified, keep as-is)

- Startup sequence resilient to DB outage; `migrate_with_retry` with capped backoff raced against shutdown, tested DB-free (`src/main.rs`, `src/db/pool.rs:82-183`).
- Kubernetes-correct shutdown ordering with `@MX:ANCHOR` rationale (`src/main.rs:149-166`).
- Claim/lease/fencing discipline: short transactions, `FOR UPDATE SKIP LOCKED`, `AND claimed_by = $self` fencing, SQL-shape tests (`src/collectors/live_poller.rs:169-188,487-556`; `collection_queue.rs:85-108`; `backfill.rs:194-233`).
- Decimal-only money end-to-end with exactness tests (`Cargo.toml` feature comments; `coingecko.rs:73-76`; `tests/migration_files.rs:44`; `tests/model_serde.rs`).
- Zero SQL injection surface: every query is a static string with `$n` binds, including dynamic model dispatch (`src/api/cycle_overlay.rs:213,226`).
- Pure-core extraction with injected clocks: `candles_agg.rs` (wall-clock-free aggregation), `pacer_decision`, `reconcile_window`, `compute_sweep_actions`, projection model — all densely unit-tested without infrastructure.
- Chain range semantics distinguish continue-on-empty from error-must-surface, with the motivating production bug documented and regression-tested (`providers/mod.rs:475-559,1179-1202`).
- Bitstamp paging quirk neutralized by a pure, tested `page_end_secs` (`bitstamp.rs:205-209,449-475`).
- Pacer core: atomic single-UPDATE slot reservation with `GREATEST(now(), next_allowed_at) + gap`; reserve-then-release-lock-then-sleep local throttle (`pacer/mod.rs:90-107,166-179`).
- Alarm subsystem: TTL-refresh desired-state sweeps make "raised but never cleared" structurally impossible; `AlarmClient` has timeout + bounded retry + swallow-error so alarm delivery can never wedge a collector; wiremock tests verified against the actual alarm-center OpenAPI contract (`src/alarm/`).
- `open_24h` baseline `ts < q.ts` guard (prevents fabricated 0 % change) with dedicated DB test and an `EXPLAIN (ANALYZE, BUFFERS)` plan-shape regression test asserting partition pruning (`src/api/quotes.rs:139-159,379-521`).
- Keyset pagination: opaque base64url cursors, decode-failure→400, tamper can only alter bind values (`src/api/cursor.rs`).
- Metric cardinality discipline via `MatchedPath` with a regression test (`src/metrics/mod.rs:140-170,241-267`).
- `build.rs` migration-embed guard closing a real stale-deploy failure mode (`build.rs:1-10`).
- Distroless nonroot runtime images; env-only secrets.
- Memory discipline for the 256 Mi pod: SQL-side daily aggregation with an `@MX:WARN` against fetching the ~1M-row 5m series; bounded 4-week rollup windows; UNNEST batch insert helper (`collectors/cycle_overlay.rs:561-604`; `rollup.rs:114-229`).
- Backtest-locked projection constants (`tests/backtest_projection.rs`) and tri-state `Option<Option<String>>` PATCH semantics correctly implemented and tested (`api/dto.rs:103-121`).

---

## 5. Issues & Improvement Opportunities — Overview

| Severity | Count | Themes |
|---|---|---|
| Critical | 0 | — |
| High | 10 | Worker retry semantics (3), materializer data loss (1), provider egress (3), API contract/query bounds (2), tier config (1) |
| Medium | 24 | Contract drift, missing bounds, error classification, batching, duplication-as-architecture |
| Low | 20 | Edge-case defects, doc/code drift, non-idiomatic patterns, hygiene |
| Informational | ~12 | Dead code/fields, cosmetic typing, build nits, optional modernization |

Classification legend per finding: **[defect]** actual defect · **[architecture]** architectural weakness · **[non-idiomatic]** non-idiomatic Rust · **[future-risk]** potential future risk · **[optional]** optional improvement. Change-nature: *behavioral / performance / maintainability / stylistic* noted inline.

---

## 6. Detailed Findings

### Category A — Worker retry & backpressure (collectors)

#### F-01 [High] [defect, behavioral] Backfill `attempts` inflates on every page release → chunk permanently failed by its first real error
- **Description**: `CLAIM_BACKFILL_SQL` increments `attempts` at claim time (`src/collectors/backfill.rs:178`). Multi-page chunks are released back to `pending` after every page via `fail_or_retry_backfill_chunk(..., i32::MAX, "partial")` (`backfill.rs:795-812`), which avoids *failing* the chunk but never resets `attempts` — so `attempts` counts pages, not failures. When a genuine error later occurs, the `CASE WHEN attempts >= $3 THEN 'failed'` in `FAIL_OR_RETRY_BACKFILL_SQL` (`backfill.rs:225-233`) fires immediately once more than `max_attempts` (5) pages have been walked.
- **Why it matters**: A 10-year 5m backfill is ~1000 pages. One transient blip past page 5 permanently fails the chunk, and `enqueue_backfill_job`'s `ON CONFLICT DO NOTHING` (`backfill.rs:238-243`) never recreates it — the historical pipeline halts silently (only the `backfill-failed` alarm notices). REQ-SCHED-027 intends to bound *retries*, not *pages*. The collection-queue soft-skip path has the identical structural problem (`collection_queue.rs:810-828`).
- **Recommendation**: Dedicated release SQL that resets (or does not count) `attempts` on partial-release/skip; or stop incrementing at claim time and increment only in the failure path. Regression test: claim → partial-release N>max times → one failure → assert `pending`, not `failed`.

#### F-02 [High] [defect, behavioral] Backfill treats pacer cooldown/credit exhaustion as hard failure
- **Description**: `process_chunk` maps any `acquire_slot` error to `Err` (`backfill.rs:622-624`), unlike collection_queue (`pacer_should_skip_queue`, `collection_queue.rs:413-419`) and live_poller (`pacer_should_skip`, `live_poller.rs:73-78`) which classify `Cooldown`/`CreditExhausted` as soft skips.
- **Why it matters**: Routine pacer backpressure consumes attempts and (combined with F-01) permanently fails chunks. A cooldown outlasting 5 claim cycles is fatal to the chunk.
- **Recommendation**: Mirror the queue worker's classification: release without counting an attempt, then idle.

#### F-03 [High] [defect, behavioral/performance] Queue & backfill workers busy-loop against the DB during cooldown — no sleep on soft-skip/failure release
- **Description**: On soft skip, `run_collection_queue_worker` releases the item and immediately `continue`s (`collection_queue.rs:810-828` → loop top 745); `claim_queue_item` re-finds the same oldest item instantly (claim UPDATE + context SELECT + pacer roundtrips + release UPDATE + a spawned heartbeat task per iteration). Idle sleep triggers only on empty queue. Backfill has the same shape (`backfill.rs:712-831`). Retryable failures likewise get all `max_attempts` retries back-to-back within seconds — "retry" has no temporal spreading.
- **Why it matters**: Thousands of pointless DB round trips during a multi-minute cooldown from a pod with a small pool; interacts with F-01/F-02 to burn attempts.
- **Recommendation**: Sleep (raced against shutdown) after any soft-skip/failure release; better, defer the item (`lease_expires_at = cooldown_until`-style) or per-item exponential backoff.

#### F-04 [Medium] [non-idiomatic + architecture, behavioral] `dispatch_item`'s `Result<bool, String>` erases the transient/permanent distinction
- **Description**: Every failure is stringified (`collection_queue.rs:392-397`, e.g. `.map_err(|e| e.to_string())?` at 461/557/616). Permanent conditions (coin not found, no provider supports capability) and transient network failures are retried identically. live_poller *does* classify (`is_transient_provider_error`, `live_poller.rs:333-347`) but only changes the log level — a permanently erroring coin is re-fetched every tick forever.
- **Recommendation**: Small `DispatchError { kind: Transient|Permanent, msg }` (thiserror); fail-fast on Permanent; back off on Transient; per-coin failure streak in live_poller.

#### F-05 [Medium] [future-risk, behavioral] Live-poller batch claim is unbounded and can outlive the claim TTL; no shutdown check mid-batch
- **Description**: `poll_cycle` claims *all* due coins in one statement (no `LIMIT`, `live_poller.rs:102-115`) then processes serially (pacer + fetch + writes per coin, `live_poller.rs:259-349`). Markers self-expire at `claim_ttl` (120 s); a batch tail processed after expiry can be double-claimed, and the late `mark_coin_poll_success` overwrites unconditionally (no fencing, `live_poller.rs:126-129`). No shutdown check inside the loop, so graceful shutdown waits for the whole batch.
- **Recommendation**: `LIMIT` the claim to a TTL-safe batch size; check shutdown between coins.

#### F-06 [Low] [defect, maintainability] `last_error` overwritten by administrative releases; silent `let _ =` on marker clears
- **Description**: Both queue workers write `last_error = "pacer_skip"` / `"partial"` on non-failing releases (`collection_queue.rs:819`, `backfill.rs:804`), destroying the real prior error an operator may be diagnosing. Marker-clear failures are discarded unlogged (`let _ = clear_coin_poll_marker(...)`, `live_poller.rs:285-346`, 6 sites).
- **Recommendation**: `NULL` (or a dedicated column) for administrative releases; `warn!` on clear failures.

### Category B — Materializer & projection data integrity

#### F-07 [High] [defect, behavioral] Rollup window-reconcile can delete or overwrite *native* 1d candles
- **Description**: `incremental_recompute_target`'s `previously_materialized` SELECT has **no `source LIKE 'rollup:%'` filter** (`src/collectors/rollup.rs:274-284`), though `recompute_start` itself is correctly rollup-filtered (`rollup.rs:250-253`). Any native provider-sourced 1d row in the window is treated as "previously materialized": `reconcile_window` upserts overwrite its OHLCV and relabel `source` to `rollup:5m`, and the DELETE for unreproduced timestamps also carries no source filter (`rollup.rs:313-324`).
- **Why it matters**: A derived materializer must never destroy the source rows it derives from. If a coin ever has both fine-interval and native 1d candles in the forward window (deep backfill overlapping recent history, provider granularity changes), genuine provider data is silently deleted or clobbered — unrecoverable without re-fetch.
- **Recommendation**: Add `AND source LIKE 'rollup:%'` to both the SELECT and the DELETE; decide explicitly whether rollup upserts may overwrite colliding native rows (arguably native wins). Regression test with a mixed-source window.

#### F-08 [Medium] [future-risk, behavioral] Forward-only rollup recompute never repairs history behind the max materialized bucket
- **Description**: Recompute starts at `MAX(ts)` of existing rollup rows (`rollup.rs:250-296`); source candles arriving *behind* that point (deep backfill completing later, gap re-fetches) are never rematerialized — only the read-time coverage-aware fallback hides the divergence.
- **Recommendation**: Track a source low-watermark and trigger a bounded backfill pass when new source rows precede the earliest materialized bucket.

#### F-09 [Low] [future-risk, behavioral] `Decimal::log10()`/`ln()` panic on non-positive closes can crash-loop the queue worker
- **Description**: `fit_model`/`project_composite` call `.log10()` on every stored close and `current_price` (`cycle_projection.rs:289-313,513-514`); `pow10` uses `ln()` (`:111-113`). `rust_decimal`'s `ln` panics for ≤ 0. One zero/negative close row turns every `cycle_overlay` dispatch into panic → restart → crash-loop alarm → permanent item failure.
- **Recommendation**: Filter `p > 0` when building the series; skip and log offending rows.

### Category C — Provider transport & pacing

#### F-10 [High] [defect, behavioral] CoinGecko `search_coins` / `fetch_coin_tickers` bypass the pacer entirely; 429s swallowed without cooldown
- **Description**: Both trait methods delegate straight to the client with no `local_throttle.acquire()` and no `pacer::acquire_slot` (`coingecko.rs:1092-1106`), violating the layer's own `@MX:WARN` invariant that every outbound call routes through the fleet-wide egress governor (`pacer/mod.rs:150-153`, REQ-PROV-040/045). Worse, a 429 on these endpoints is degraded to `Ok(vec![])` (`coingecko.rs:398-411,462-475`) without `signal_cooldown` — the fleet never backs off.
- **Why it matters**: These serve user-facing search endpoints, so request volume is externally driven; a burst produces unmetered CoinGecko egress and can trip the very 429s the pacer exists to prevent.
- **Recommendation**: Route both through the throttle + `acquire_slot` prelude; on 429, `signal_cooldown` before degrading to empty (degrading the *result* is fine per REQ-PROV-005; skipping the signal is not).

#### F-11 [High] [defect, behavioral] No HTTP timeouts on any provider client
- **Description**: All three real clients are `reqwest::Client::builder().gzip(true).build()` with no `.timeout()`/`.connect_timeout()` (`coingecko.rs:160-163`, `binance.rs:33-36`, `bitstamp.rs:69-72`); reqwest's default is no total-request timeout. `grep -rn timeout src/providers/ src/pacer/` → zero matches (verified).
- **Why it matters**: A black-holed upstream hangs the calling worker indefinitely — after the pacer has already charged a credit — with health probes green while collection silently stops. On a single-replica pod this is a real availability risk.
- **Recommendation**: `.timeout(~30 s)` + shorter `.connect_timeout` via one shared client-construction helper. A timeout surfaces as `ProviderError::Network`, which chain + alarm already classify correctly.

#### F-12 [Medium] [defect, behavioral] `is_transient()` classifies all `Http{..}` — including 4xx — as transient
- **Description**: `ProviderError::is_transient` matches `RateLimited | Network(_) | Http { .. }` with no status discrimination (`providers/mod.rs:198-203`); consumed by live-poller retry classification.
- **Recommendation**: `Http { status, .. } => matches!(status, 408 | 425 | 429 | 500..=599)`.

#### F-13 [Medium] [defect, behavioral] `acquire_slot` silently clamps the reserved wait to 60 s — pacing violated under backlog
- **Description**: After the atomic reservation, the caller sleeps `wait.clamp(0, 60_000)` ms (`pacer/mod.rs:184-189`). Under backlog (each reservation advances `next_allowed_at` by `min_gap_ms`), waits beyond 60 s are truncated and requests fire *before* their reserved slots — converting overload into exactly the burst the pacer prevents, silently (31+ queued acquirers at CoinGecko-demo 2 s gap suffices).
- **Recommendation**: Sleep the full computed wait (the reservation is already made), or return a distinct `Backlogged` error / warn when the clamp engages.

#### F-14 [Medium] [architecture, maintainability] Request/429-cooldown/parse scaffolding duplicated across providers — the proven drift source
- **Description**: The `Err(RateLimited) => { cooldown; signal; return }` block appears 6× in `coingecko.rs` (899-1073), 3× in `binance.rs`, 2× in `bitstamp.rs` (only Bitstamp factored helpers: `acquire()`/`signal_rate_limit()`, `bitstamp.rs:245-256`). The client-side status/parse epilogue (`429→RateLimited; !success→Http; json→Parse`) is copy-pasted 6× in `coingecko.rs` (220-539) and 2× each elsewhere, with per-site drift already visible (F-10 exists because new endpoints were written outside the frame).
- **Recommendation**: (a) `async fn paced<T>(provider, pool, throttle, fut)` wrapping throttle+slot+429-cooldown; (b) `async fn get_json<T: DeserializeOwned>(req, ctx)` for the epilogue. Mechanical extraction; existing tests cover behavior.

#### F-15 [Medium] [future-risk, behavioral] No startup validation that every chain provider has a pacer row
- **Description**: A missing `upstream_request_pacer` row surfaces as `AcquireSlotError::NotFound` on *every* fetch at runtime (`pacer/mod.rs:203`); `build_chain` validates names only (`providers/mod.rs:349-381`). `providers/mod.rs:477-479` documents that exactly this caused a production incident once.
- **Recommendation**: Post-build startup check `SELECT provider FROM upstream_request_pacer WHERE provider = ANY($1)`; fail readiness on a missing member.

#### F-16 [Medium] [architecture, behavioral] Pacer slot charged to the first capability-supporting provider, not the provider that serves the request
- **Description**: All three workers key `acquire_slot` on the first capable chain member (`live_poller.rs:277-288`, `collection_queue.rs:379-419`, `backfill.rs:541-556`) then run the fallback chain, which may be served by a different provider.
- **Why it matters**: Exactly when the primary fails (when fallback fires), fallback providers receive unpaced traffic while the failing primary's credits are burned. This is a design decision to make explicit, not a quick patch.
- **Recommendation**: Acquire per attempted provider inside the chain loop (pairs naturally with the F-53 `chain_try` consolidation), or at minimum record/charge the serving provider.

#### F-17 [Low] [defect, behavioral] `signal_cooldown` can shorten an existing longer cooldown
- **Description**: `SET cooldown_until = $2` unconditionally (`pacer/mod.rs:226-242`); a later, shorter signal truncates an earlier, longer one (multi-replica races, operator-set cooldowns).
- **Recommendation**: `GREATEST(COALESCE(cooldown_until, 'epoch'), $2)`.

#### F-18 [Low] [defect, maintainability] `acquire_slot` blocked-path race mislabeled `NotFound`
- **Description**: When the gated UPDATE matches no row and the diagnostic re-SELECT finds the block already lapsed, the fallback returns `NotFound(provider)` (`pacer/mod.rs:192-217`) — misleading for "retry would have succeeded".
- **Recommendation**: Distinct `Contended`/retry-once; reserve `NotFound` for the genuinely-absent row.

#### F-19 [Informational] [optional] No `User-Agent` on any client
- One `.user_agent(concat!("crypto-collector/", env!("CARGO_PKG_VERSION")))` per builder is cheap insurance (Bitstamp's WAF intermittently rejects default library UAs). Also: `expect("reqwest client")` at construction is startup-only fail-fast, but `build_chain` already returns `anyhow::Result` and could propagate.

### Category D — Provider data correctness & tier configuration

#### F-20 [High] [defect, behavioral] CoinGecko `/ohlc/range` stamps non-canonical `"daily"`/`"hourly"` interval strings
- **Description**: `coingecko_range_snap_interval` returns the CoinGecko API parameter values and the same string is persisted as the candle's `interval` (`coingecko.rs:649-656`, `normalise_ohlc_item` at 341-343; a test asserts `interval == "daily"` at 1526). Every other provider stamps canonical taxonomy (`"1d"`, `"1h"`) — Bitstamp documents the requirement explicitly and separates API param from stamp (`bitstamp.rs:130-132,185-197`).
- **Why it matters**: `interval_to_seconds` (`api/candles_agg.rs:28-48`) has no `"daily"`/`"hourly"` rows, so range-backfilled rows are invisible to interval resolution, coverage selection, and any `interval = '1d'` query — the same series splits across two keys depending on which provider served the page. Latent only because the range path requires Analyst+ tier; upgrading the tier silently writes orphaned rows.
- **Recommendation**: Mirror Bitstamp: keep `"daily"`/`"hourly"` as request params, stamp `"1d"`/`"1h"` on returned candles. Migration/cleanup consideration for any already-written rows.

#### F-21 [High] [defect, behavioral] Paid tiers (`analyst`/`lite`/`enterprise`) get the demo header and demo base URL
- **Description**: `supports_ohlc_range` accepts `analyst|lite|enterprise|pro` (`coingecko.rs:58-63`), but `key_header_name` returns the pro header only for exactly `"pro"` (`coingecko.rs:49-55`), and `config::coingecko_base_url` defaults every non-`pro` tier to `https://api.coingecko.com` (`config.rs:172-179`). Verified verbatim.
- **Why it matters**: CoinGecko paid plans are all Pro-API plans (`pro-api.coingecko.com` + `x-cg-pro-api-key`). `COINGECKO_TIER=analyst` — the exact configuration deep backfill requires — enables the range capability but sends the key in the demo header to the demo host. Three tier-dependent decisions live in two files with two different tier sets.
- **Recommendation**: A `Tier` enum (`Demo|Analyst|Lite|Pro|Enterprise`) parsed once, fail-fast on unknown; `is_paid()` drives header + base-URL default; `supports_ohlc_range()` drives capability.

#### F-22 [Medium] [defect, behavioral] Binance spot quote stores a 1-minute volume in `volume_24h`
- **Description**: `fetch_spot` fetches one 1m kline and sets `volume_24h: candle.volume` ("best approximation from kline", `binance.rs:305-314`).
- **Why it matters**: Under-reports by ~3 orders of magnitude in the same normalized field CoinGecko fills genuinely — silently corrupted cross-source comparisons whenever the chain falls back. `None` is strictly more honest; `GET /api/v3/ticker/24hr` gives the correct value (plus real bid/ask).
- **Recommendation**: `volume_24h: None` (minimal) or switch spot to the 24hr ticker endpoint.

#### F-23 [Medium] [future-risk, behavioral] `Decimal::from_str` cannot parse scientific notation — one exotic number poisons a whole page
- **Description**: With `arbitrary_precision`, `Number::to_string()` returns JSON text verbatim, including exponent forms; `Decimal::from_str` rejects them (`from_scientific` is a separate constructor). Normalization uses `collect::<Result<…>>` (`coingecko.rs:234-237,279-281`), so one bad field fails the whole batch. The `precision=full` request param (`coingecko.rs:215`) makes extreme representations plausible for micro-caps.
- **Recommendation**: Fall back to `Decimal::from_scientific` in `decimal_from_number` (`coingecko.rs:73-76`); consider degrading unparseable *optional* fields to `None` rather than failing the item.

#### F-24 [Medium] [defect, behavioral] Derivatives lookup: whole-market fetch + case-insensitive *prefix* match can bind the wrong ticker
- **Description**: `fetch_derivatives` pulls the entire `/derivatives/tickers` payload and picks the first symbol `starts_with(base)` (`coingecko.rs:1075-1087`). `"BTC"` also matches `BTCDOM`/`BTCST`/`BTCUP`; "first in response order" is an arbitrary venue choice that can silently change between polls.
- **Recommendation**: Symbol-boundary match, prefer `market.venue` when present, deterministic pick (e.g. highest OI).

#### F-25 [Low] [defect, behavioral] Binance `fetch_ohlc` computes `limit` from the raw interval, not the snapped one
- **Description**: The request uses the snapped kline interval but `limit` divides by unsnapped seconds (`binance.rs:324-327`) — wrong lookback for between-band inputs. Latent (callers pass canonical seconds).
- **Recommendation**: Return `(secs, name)` from the snap (Bitstamp pattern) and divide by snapped seconds.

#### F-26 [Low] [defect, maintainability] `chain_fetch_ohlc` reports "empty provider chain" when the chain was non-empty but all-unsupported
- **Description**: `last_err` seeded with `Other(anyhow!("empty provider chain"))` (`providers/mod.rs:407`) is returned unchanged when all members were skipped as `Unsupported`.
- **Recommendation**: Distinct `NoCapableProvider(Capability)` seed or synthesize from `records`.

#### F-27 [Low] [non-idiomatic, maintainability] Silent normalization degradations
- **Description**: Unparseable `last_updated` silently becomes `Utc::now()` (`coingecko.rs:558-563`) — a malformed upstream timestamp masquerades as a fresh quote time; `max_supply` parse failures swallowed via `.ok()` (`coingecko.rs:770-774`) while sibling fields propagate errors.
- **Recommendation**: Keep degradation, add `debug!`/`warn!`; align optional-field strictness one way.

#### F-28 [Low] [non-idiomatic, stylistic/perf] `serde_json::Value` indexing with array deep-clones in search/tickers paths
- **Description**: `body["coins"].as_array().cloned()` / `body["tickers"]...` (`coingecko.rs:418-421,482`) deep-clone arrays before read-only iteration; per-element `to_uppercase()` in the derivatives scan (`:1082`). Stylistically inconsistent with the typed DTOs used elsewhere in the same file.
- **Recommendation**: Iterate by reference or deserialize into typed DTOs. Not hot paths; craft only.

### Category E — API contract & HTTP boundary

#### F-29 [High] [defect, behavioral] Quote read endpoints ignore `vs_currency` — OpenAPI drift + cross-currency mixing + cursor row loss
- **Description**: `GET /v1/coins/{id}/quotes/latest` and `/quotes` neither accept nor filter `vs_currency`: `ListQuotesParams` has no such field and both queries filter only `WHERE coin_id = $1` (`src/api/quotes.rs:24-29,45-97`; verified). The OpenAPI spec documents the parameter with `default: usd` on both operations (`api/crypto-collector.yaml:361-389`). Every sibling endpoint resolves `vs_currency` with `unwrap_or("usd")`.
- **Why it matters**: (1) clients passing `vs_currency=eur` are silently ignored; (2) multi-currency storage makes "latest" nondeterministic per currency and history pages interleave currencies; (3) the PK is `(coin_id, vs_currency, ts)`, so two currencies sharing a `ts` make the strict `ts < cursor` keyset advance permanently skip a row at page boundaries.
- **Recommendation**: Add `vs_currency: Option<String>` defaulting `"usd"` + `AND vs_currency = $n` to both queries (also fixes the duplicate-ts hazard).

#### F-30 [High] [defect, performance] Unbounded `coin_quotes` reads violate the project's ts-bounded partition-pruning invariant
- **Description**: `coin_quotes` is RANGE-partitioned by `ts` (~48 monthly partitions); `get_latest_quote` runs `WHERE coin_id = $1 ORDER BY ts DESC LIMIT 1` with no ts bound (`quotes.rs:45-54`, verified) and cursor-less `list_quotes` is likewise unbounded (`quotes.rs:81-97`) — directly violating the `@MX:WARN` invariant declared two functions below (`quotes.rs:129-138`), which cites a 41-second unbounded-shape incident.
- **Recommendation**: Bound `get_latest_quote` with a trailing freshness window (matching the overview endpoint's 48 h), give `list_quotes` a default trailing window, or explicitly document the exemption at the anchor.

#### F-31 [Medium] [defect, behavioral] Aggregation path: `end` filter not pushed into the source query — far-past windows unreachable; cap-hit-but-empty pages terminate pagination
- **Description**: `params.end` applies only post-aggregation (`api/candles.rs:260-262`); the source query fetches newest rows bounded only by `cursor_ts` (`candles.rs:225-243`). A far-past `end` with no cursor yields an empty page with `next_cursor: null` (`candles.rs:272-276`) — indistinguishable from "no data" though the window exists. Same branch ends pagination prematurely when the 50 000-row cap is hit and every fetched bucket is gap-dropped.
- **Recommendation**: Bound the source query with `ts < end + target_secs` (one-bucket margin, mirroring the existing `source_start` margin); in the cap-hit-but-empty case derive the cursor from the oldest fetched *source* row's bucket.

#### F-32 [Medium] [defect, behavioral] `register_coin`: check-then-insert race breaks documented idempotency; enqueue fan-out non-transactional
- **Description**: SELECT-then-INSERT (`api/coins.rs:110-136`) — concurrent duplicate POSTs make the loser 500 on the PK violation instead of the documented idempotent 200 (REQ-API-011); the three queue enqueues after insert (`coins.rs:139-146`) run outside any transaction.
- **Recommendation**: `INSERT ... ON CONFLICT (coin_id) DO NOTHING RETURNING ...` (re-select → 200 on zero rows); wrap insert + enqueues in one transaction.

#### F-33 [Medium] [architecture, behavioral] Extractor rejections bypass the uniform JSON error body; `From<JsonRejection>` is dead code
- **Description**: REQ-API-074 mandates `{code, message}`; `ApiError` guarantees it only for handler-body errors. Handlers use bare `Json`/`Query`/`Path` extractors, so malformed bodies and unparsable params produce axum's default `text/plain` rejections. `impl From<JsonRejection> for ApiError` (`api/mod.rs:131-135`) is invoked by nothing.
- **Recommendation**: `ApiJson<T>`/`ApiQuery<T>` via `WithRejection` or `#[derive(FromRequest)]` with `rejection = ApiError` — or delete the dead impl and document the exception. Existing tests assert only status codes, so add body-shape assertions.

#### F-34 [Medium] [architecture, behavioral] `search_coins` degrades provider failures to 200-empty, contradicting the documented 503
- **Description**: The spec documents 503 for pacer/cooldown/credit exhaustion (`api/crypto-collector.yaml:99-121`); the handler returns 200-empty for every provider error (`api/coins.rs:171-181`), reserving 503 for "provider not in chain" only.
- **Recommendation**: Map pacer/cooldown errors to `ServiceUnavailable` per spec (or amend the spec) so clients can distinguish "no matches" from "provider down".

#### F-35 [Medium] [future-risk, performance] `as_of` recompute path is unbounded per-request CPU with no amortization across pages
- **Description**: Any `as_of` request triggers full daily-history load + overlay compute + `daily.clone()` + projection, per request, per page (`api/cycle_overlay.rs:263-288,328-351`). No auth or rate limiting exists on the router.
- **Why it matters**: The most plausible self-inflicted resource-exhaustion vector on the 256 Mi pod. Acceptable in-cluster today; needs a ceiling before external exposure.
- **Recommendation**: Short-TTL memoization keyed `(coin_id, vs_currency, as_of, model)` or a concurrency semaphore.

#### F-36 [Low] [future-risk, behavioral] WebSocket handler never reads from the socket
- **Description**: `handle_stream` only sends (`api/websocket.rs:70-89`): Close frames/pings unprocessed, disconnects detected only on send failure, silent clients hold broadcast receivers; no subscription filtering (every client gets every coin).
- **Recommendation**: `select!` over `rx.recv()` and `socket.recv()`; break on Close/`None`; periodic pings.

#### F-37 [Informational] API-layer minor notes
- `delete_coin` UPDATE-then-probe TOCTOU yields 404-vs-204 ambiguity under concurrent delete — benign (`api/coins.rs:297-314`).
- Three near-identical paginators (`paginate_coins`/`paginate_ts`/`paginate_cycle_overlay`) — see F-53.
- `pub struct Page<T: Serialize>` puts the serde bound on the struct definition (`api/dto.rs:27`); idiomatic serde leaves bounds to the derive. Cosmetic.

### Category F — Lifecycle, observability & configuration

#### F-38 [Medium] [defect, behavioral] Persistence-latency metric names drift: described names never emitted
- **Description**: `describe_all()` registers `quote_insert_duration_seconds`/`candle_insert_duration_seconds` (`src/metrics/mod.rs:73-81`, also the module-header catalogue and REQ-OBS-015), but emitters record `coin_quote_insert_duration_seconds`/`coin_candle_insert_duration_seconds` (`src/db/upserts.rs:78,143`; verified — zero emitters of the described names). The unit tests emit the described names directly, masking the mismatch.
- **Recommendation**: Shared `const` metric names referenced by both `describe_all()` and emitters.

#### F-39 [Medium] [defect, behavioral] PG LISTEN relay tasks unsupervised, never retry initial connection; doc contradicts code
- **Description**: `run_listener` logs and returns permanently if connect/`listen()` fails (`src/listener.rs:63-73`), though its doc claims bounded retry (`listener.rs:54`). Both relays are bare `tokio::spawn`s (`src/main.rs:392-403`) outside the supervision pattern used for workers.
- **Why it matters**: A transient DB hiccup at spawn (or a panic in the loop) permanently disables cross-replica WebSocket delivery (REQ-API-148) with readiness green. (sqlx's `PgListener::recv()` does reconnect after a successful initial connect; only initial-connect and panic paths are exposed.)
- **Recommendation**: Supervise like the workers or add an outer connect/listen retry loop; fix the doc either way.

#### F-40 [Medium] [defect, behavioral] DB credentials not percent-encoded in the assembled URL
- **Description**: `build_database_url` formats username/password directly into `postgres://{u}:{p}@…` (`src/config.rs:44-55`). A password containing `@ / : # %` or spaces yields a malformed or *differently parsed* URL (worst case re-pointing the host); tests cover only alphanumeric credentials.
- **Recommendation**: Build `sqlx::postgres::PgConnectOptions` from parts (also keeps the password out of any loggable string) — no new dependency needed; percent-encoding is the fallback option.

#### F-41 [Medium] [architecture, behavioral] Shutdown drain: unconditional fixed sleep then unbounded supervisor await
- **Description**: After servers exit, `main` sleeps the full `drain_secs` (default 30 s) unconditionally, then `supervisor.await` with no timeout (`src/main.rs:455-458`).
- **Why it matters**: Every shutdown pays 30 s even with zero in-flight work (slowing every `make deploy` rollout); a wedged worker blocks forever until the kubelet SIGKILLs, losing the ordered `pool.close()`/trace-flush steps.
- **Recommendation**: `tokio::time::timeout(drain_secs, supervisor)` — completes as soon as workers finish, bounded above.

#### F-42 [Medium] [architecture, behavioral] `all_providers_down` is a sampled last-outcome flag — the Critical alarm is race-prone/flappy
- **Description**: The flag is set by whichever chain fetch completed last and sampled once per sweep (`alarm/registry.rs:96-117`, `reconciler.rs:78-81`). One coin's failure among hundreds of successes can flip it at sweep time; a lone success mid-outage can suppress it.
- **Recommendation**: Timestamped signals (`last_all_failed_at`/`last_chain_success_at`) gated on a sustained window, reusing the existing `sustained_*` helpers.

#### F-43 [Low] [defect, behavioral] Readiness flips ready before the API listener binds
- **Description**: `set_ready()` at Step 10 (`src/main.rs:372`) precedes the API `TcpListener::bind` at Step 11 (`main.rs:424-426`) — a millisecond window of Ready-with-connection-refused, contradicting REQ-OBS-040.
- **Recommendation**: Bind (and spawn relays) before `set_ready()`; only `axum::serve` needs to follow.

#### F-44 [Low] [defect, behavioral] Readiness cache can serve a stale 200 for up to 2 s after `set_shutting_down()`
- **Description**: `check_readiness` consults the 2 s cache before the `shutting_down`/`ready` atomics (`src/health/mod.rs:90-120`); the 503-on-shutdown guarantee (REQ-OBS-004) can lag. The passing unit test relies on an unpopulated cache.
- **Recommendation**: Check the flags before the cache fast-path; cache only the DB-ping result.

#### F-45 [Low] [architecture, behavioral] All env parsing silently falls back to defaults on invalid values
- **Description**: `parse_env_*` helpers do `.ok().and_then(parse().ok()).unwrap_or(default)` (`src/config.rs:566-599`; same for `DEEP_BACKFILL_START_DATE`). Under env-only config, a typo'd operator override (e.g. mis-typed pacer cooldown) silently reverts to the default with no diagnostic.
- **Recommendation**: `warn!` on present-but-unparseable at minimum; ideally fail fast (precedent: missing `DB_HOST` already fails fast).

#### F-46 [Low] [future-risk + non-idiomatic, maintainability] Four copy-pasted supervisors; fixed 5 s restart with no backoff
- **Description**: `run_supervised_{live_poller,queue_worker,backfill_worker,reconciler}` differ only in name/future/registry poke (~180 duplicated lines, `src/collectors/mod.rs:207-413`); restart delay is a constant 5 s — a deterministic crasher restarts 12×/min forever (log spam, repeated DB/upstream load). `migrate_with_retry` already demonstrates the capped-exponential pattern.
- **Recommendation**: One generic `run_supervised(name, registry, shutdown, make_future)` owning restart policy with capped exponential backoff (reset after healthy runtime).

#### F-47 [Low] [defect, behavioral] Busy-spin on dropped shutdown sender
- **Description**: `listener.rs`'s select arm ignores `changed()`'s `Result` (`src/listener.rs:80-85`): sender dropped without sending `true` → immediate-`Err` hot loop. Worker `select!` loops have the same edge (`live_poller.rs:232-244`, `reconciler.rs:604-620`). Reachable only if the shutdown orchestrator panics before send; every other consumer handles it.
- **Recommendation**: `res = shutdown_rx.changed() => if res.is_err() || *borrow() { break }` at all sites.

#### F-48 [Low] [defect, maintainability] `observe_chain_records` doc misdescribes behavior
- **Description**: Doc claims it "records ANY failure as a network failure for the provider-unreachable signal"; the implementation only sets/clears the chain flag, never touching the per-provider map (`alarm/registry.rs:125-149`). Doc/code drift on an alarm-signal function invites a silent detection gap. Related: the failure streak counts only `Network` errors — repeated 5xx never trips `provider-unreachable` (`providers/mod.rs:434-439`); confirm against REQ-ALARM-020.
- **Recommendation**: Fix the comment (or implement the described recording, per the SPEC).

#### F-49 [Informational] Lifecycle/observability/build nits
- `tracked_markets` gauge described + tested but unemittable (table dropped by migration 0011) — remove describe/doc/test (`src/metrics/mod.rs:84,382-396`).
- `start_api_server` is dead (main builds the router itself); its doc claims otherwise (`src/api/mod.rs:8-13,233`). Delete or make it the single entry point.
- `HeaderExtractor` duplicated: private copy in `main.rs:40-49` shadows the public documented one in `telemetry/mod.rs:110-120`, tests duplicated in both. Consolidate into `telemetry` (move `OtelMakeSpan` too).
- Metrics coverage asymmetry: `collection_requests_total`/duration recorded only in queue dispatch; live-poller and backfill provider calls uninstrumented.
- Reconciler ticker uses default `MissedTickBehavior::Burst` (`reconciler.rs:605`); live_poller correctly uses `Skip`. Serial alarm raises bound shutdown responsiveness by seconds.
- `timeout_seconds: u64` vs alarm-center contract int32 max — startup clamp would fail fast; `AlarmClient` retries have no backoff (fine at 1–3 retries).
- `Dockerfile` never copies `build.rs` into the builder stage (guard parity with host builds is accidental); both Dockerfiles copy `migrations/` into runtime images though they're compile-time embedded; `Makefile` `upgrade` missing from `.PHONY`; `deploy` relies on a fixed mutable `:aarch64` tag (needs `imagePullPolicy: Always`).
- Health server stops serving at drain start, so liveness fails during the drain window — harmless under normal K8s termination.

### Category G — Structure & idiomatic debt

#### F-50 [Medium] [architecture, maintainability] Provider trait: no default method bodies (~450 lines of `NotSupported` stubs); lopsided search methods
- **Description**: Of 9 required methods, only `fetch_ohlc_range` has a default (`providers/mod.rs:241-326`). Coinbase/Kraken are 110-line all-stub files; the five test doubles in `mod.rs` repeat ~55 dead lines each. `supports()` and per-method `NotSupported` encode the same fact twice with nothing tying them together. `search_coins`/`fetch_coin_tickers` are directory concerns implemented by exactly one provider, with no `Capability::Search` to describe them.
- **Recommendation**: Capability-derived default bodies for all fetch methods (only `name()`/`supports()` mandatory); longer-term move the search pair to a `CoinDirectory` trait implemented by CoinGecko.

#### F-51 [Medium] [architecture, performance] Per-row upserts in hot candle paths; per-row `pg_notify`
- **Description**: The candles dispatch upserts each candle individually (`collection_queue.rs:463-491` — ~2016 rows per 7-day/5m refresh), backfill likewise per page (`backfill.rs:662-690`, up to ~1000 rows/page × ~1000 pages), each in its own transaction with its own NOTIFY (`db/upserts.rs:44-147`). `rollup.rs`'s own `@MX:NOTE` (107-113) documents the cost and provides `batched_upsert_candles` — unused by the two heaviest writers. Backfill NOTIFYs also flood the listener→broadcast→WebSocket path with historical data, accelerating `Lagged` drops for live consumers.
- **Recommendation**: Generalize `batched_upsert_candles` (UNNEST) for collection + backfill, without NOTIFY on the backfill path; keep NOTIFY for live polls. Note `upsert_coin_metadata` is also read-then-insert without a transaction (benign with a single claimant).

#### F-52 [Medium] [architecture, performance] `recompute_cycle_overlay` issues ~4000–8000 single-row INSERTs inside one transaction
- **Description**: Full-rebuild deletes all rows then re-inserts every point of three models per-row in one tx (`collectors/cycle_overlay.rs:397-439`) — the longest-held connection in the codebase on a small pool.
- **Recommendation**: UNNEST batches per model group, keeping DELETE + INSERT in one transaction for the idempotent-rebuild semantics.

#### F-53 [Low] [non-idiomatic, maintainability] Systemic duplication: chain-fetch helpers, claim/heartbeat scaffolds, `ensure_coin_exists`, paginators
- **Description**: (a) Four near-identical fallback loops (`chain_fetch_spot`, `chain_fetch_spot_local`, `chain_fetch_coin_metadata`, `chain_fetch_coin_market` — `live_poller.rs:355-391`, `collection_queue.rs:263-376`) with identical registry bookkeeping. (b) Claim/heartbeat/complete/fail-or-retry scaffolding duplicated between the two queue workers, including the spawned heartbeat task (`collection_queue.rs:772-797` vs `backfill.rs:738-760`). (c) `ensure_coin_exists` duplicated verbatim (`api/quotes.rs:167-177`, `api/metadata.rs:85-95`) with cross-imports; the tracked_coins column list inlined 5× in `coins.rs`. (d) Three paginators implementing the same truncate-and-encode heuristic. The F-02 backfill/queue classification drift is a direct product of (b).
- **Recommendation**: Generic `chain_try<T>(chain, cap, registry, f)`; a small shared lease-queue scaffold; single `ensure_coin_exists`; `concat!`-assembled column-list const; generic `paginate<T, K: Serialize>`.

#### F-54 [Low] [non-idiomatic, maintainability] Stringly-typed domain values; expect-coupled tables; dummy fields
- **Description**: API-facing candle intervals are `&str` validated against `SUPPORTED_INTERVALS` then mapped by a *second* hand-synced table, coupled by `.expect("validated interval must have a known second count")` (`api/candles.rs:37,163-164` / `candles_agg.rs:28-48`) — a runtime panic path guarding a compile-time-expressible invariant (`ProjectionModel` in the same codebase shows the enum pattern). `MarketQuery { market_id: 0 /* dummy */ }` repeated in four call sites with hardcoded `"USDT"/"usd"`. `ProviderError::Parse(String)` loses provider/endpoint context.
- **Recommendation**: `enum ApiInterval` (`FromStr` + `secs()`); a keyed enum for coin-vs-market queries. Optional, high-leverage for future correctness.

#### F-55 [Low] [architecture, maintainability] `AppState` carries two dead fields; 6+ duplicated test-state builders
- **Description**: `http_client` and `coingecko_base_url` (`api/mod.rs:57-60`) are populated in `main` and all 12 test constructors but read by no handler (search goes through the provider chain); the struct's own `@MX:REASON` warns that every added field taxes every test module.
- **Recommendation**: Remove both; add a shared `#[cfg(test)] AppState::test()` constructor to collapse the duplicated builders.

#### F-56 [Informational] Structural/idiomatic notes
- `Arc<Vec<Arc<dyn Provider>>>` double indirection — `Arc<[Arc<dyn Provider>]>` expresses immutable-after-build and drops a hop. Cosmetic.
- Config is a bag of per-call env-reading functions rather than a validated `Config::from_env()` snapshot (only `replica_id` is memoized); a snapshot would centralize validation (pairs with F-45) and fix mixed `i64`/`u64`/`u32` signedness across interval getters.
- Chain orchestration exists only for OHLC (`chain_fetch_ohlc{,_range}`); spot/metadata/market chains live in collectors — the CLAUDE.md "fallback order = declaration order" invariant reads broader than the providers layer implements (works as documented via the collectors' helpers; worth one sentence of doc alignment or folding into F-53's `chain_try`).
- `CgMarketItem.vs_currency` is a dead field (endpoint never returns it; fallback always fires). `coingecko_days_to_interval` is self-described legacy kept for tests.
- Heartbeat tasks stopped via `abort()` (safe but a `watch`-based stop is cleaner; a fencing-failed heartbeat could also cancel the dispatch to stop wasted upstream credits). `interval` first tick fires immediately (redundant first heartbeat).
- Edition 2021 on rustc 1.97: an edition-2024 migration is available and mechanical (`cargo fix --edition`), optional.

### Category H — Tests & documentation hygiene

#### F-57 [Low] [defect, maintainability] Stale post-0020 schema claims in `models/quote.rs` `@MX:ANCHOR`
- **Description**: Migration `0020_coin_candles_departition.sql` flattened `coin_candles`, but `CoinCandle`'s doc + anchor still assert monthly RANGE partitioning + BRIN (`src/models/quote.rs:27-32`). `@MX:ANCHOR` content is load-bearing agent context; a stale partition claim steers future query-shape decisions wrongly (in both directions — `coin_quotes` IS still partitioned).
- **Recommendation**: Update the anchor to the flat-table reality.

#### F-58 [Low] [defect, maintainability] `tests/db_integration.rs` asserts a pre-0011 schema; migration-count test name stale
- **Description**: Scenarios still reference `tracked_markets`/`live_quotes` (e.g. `:43,:234,:746-749,:828`) — ~9 tests fail against a fresh DB (confirmed by project memory as pre-existing). `all_fourteen_migration_files_exist` (`tests/migration_files.rs:12`) — 20 migrations exist; 0015–0020 get no presence check. Permanently-red ignored tests train people to ignore red.
- **Recommendation**: Rewrite or delete the stale scenarios against the current schema; extend and rename the migration presence test.

#### F-59 [Informational] Test-suite observations
- Vacuous tests: `*_handler_exists`/`listener_fns_exist` assert nothing a compile doesn't; `tracked_coins_gauge_uses_correct_table` asserts a local literal contains itself (`src/main.rs:619-626`); `monetary_types_use_decimal_not_f64` never references the type its comment claims (`src/main.rs:689-694`).
- Config default tests silently no-op when the env var is set (parallel-test-safety tradeoff; coverage is environment-dependent).
- Several DB-gated candle scenarios depend on pre-existing prod-like fixtures and self-neutralize on empty DBs (`api/candles.rs:581,671-693,764`), unlike the exemplary self-seeding overview/as_of tests (`quotes.rs:272-404`, `cycle_overlay.rs:1166`).
- OpenAPI parity is enforced only at operationId/schema-name level (`api/mod.rs:430-461`) — parameter-level drift (exactly F-29) is invisible to it; a per-operation parameter parity test would have caught it.

---

## 7. Recommended Implementation Order

Seven phases, ordered by operational-risk reduction per unit of change, keeping each phase independently implementable and testable. (These map 1:1 to the MoAI planning prompts `prompt-implementation-improvement-1..7.local.md`.)

| Phase | Theme | Findings | Nature |
|---|---|---|---|
| 1 | Worker retry & backpressure correctness | F-01 F-02 F-03 F-04 F-05 F-06 (+F-47 worker arms) | behavioral defect fixes |
| 2 | Materializer & projection data integrity | F-07 F-08 F-09 | behavioral defect fixes |
| 3 | Provider transport hardening & pacer compliance | F-10 F-11 F-12 F-13 F-14 F-15 F-17 F-18 F-19 | behavioral + shared-helper refactor |
| 4 | Provider data correctness & tier configuration | F-20 F-21 F-22 F-23 F-24 F-25 F-26 F-27 F-28 | behavioral defect fixes |
| 5 | API contract fixes, query bounds & schema truth | F-29 F-30 F-31 F-32 F-33 F-34 F-35 F-36 F-57 F-58 (+parameter-parity test, F-59) | behavioral + hygiene |
| 6 | Lifecycle, shutdown & observability integrity | F-38 F-39 F-40 F-41 F-42 F-43 F-44 F-45 F-46 F-47 F-48 F-49 | behavioral + maintainability |
| 7 | Batching & structural debt reduction | F-50 F-51 F-52 F-53 F-54 F-55 F-16 (+F-56 selections) | performance + maintainability refactor |

Rationale for the order:
- **Phases 1–2 first**: they stop silent pipeline death and latent data destruction — the highest severity-to-effort ratio (mostly SQL/policy changes with clear regression tests).
- **Phase 3 before 4**: the shared `paced()`/`get_json()` helpers (F-14) are the natural landing site for the F-10 fix and reduce the diff of every Phase-4 change.
- **Phase 5** is independent of 1–4 and can run in parallel if desired.
- **Phase 6** touches `main.rs`/lifecycle — isolated from the data-path phases.
- **Phase 7 last**: pure refactors and batching land on top of corrected behavior, so behavior-preservation is verifiable against the fixed baseline; F-16 (per-provider pacing in the chain) belongs here because it rides the `chain_try` consolidation.

## 8. Risks, Trade-offs & Dependencies

- **Phase 4 depends on Phase 3** (helpers, and the `Tier` enum should live where the client construction is consolidated). All other phases are independent; Phase 7 should land last to avoid refactor/fix merge conflicts.
- **F-20 (interval stamps)** may require a data decision: whether to migrate any already-written `"daily"`/`"hourly"` rows (currently none expected in production since the range path is tier-gated — verify with `SELECT DISTINCT interval FROM coin_candles`).
- **F-07 (rollup source filter)**: after the filter, decide the collision policy (native row vs rollup row at the same ts) explicitly — recommended: native wins; document at the `@MX:ANCHOR`.
- **F-01/F-02/F-03** interlock: fixing the busy-loop without fixing attempts-counting still permanently fails chunks; fixing attempts without backoff still hammers the DB. Ship as one change set with one regression suite.
- **F-30 (ts-bounding `get_latest_quote`)** is a behavior change for coins with no quote in the chosen window: decide fallback semantics (empty vs unbounded fallback query) and document; the overview endpoint's 48 h window is the precedent.
- **F-34/F-33** change externally visible error responses — coordinate with any API consumers; the OpenAPI spec is the contract of record.
- **F-41 (drain timeout)** shortens shutdown when workers finish early; verify the 15 s endpoint-removal grace still precedes worker teardown (it does — the grace sleep is upstream of the broadcast).
- **F-51 (batch upserts without NOTIFY on backfill)** intentionally changes WebSocket behavior (historical candles no longer broadcast) — this is the desired behavior but should be stated in the SPEC/acceptance.
- **Phase 7 refactors** are behavior-preserving by definition; rely on the existing pure-core test density plus `cargo clippy -D warnings` and characterization tests where coverage is thin (chain helpers, supervisors).

## 9. Final Prioritized Improvement List

**High (do first)**
1. F-01 Backfill attempts-inflation → permanent chunk failure (Phase 1)
2. F-02 Backfill hard-fails on pacer cooldown (Phase 1)
3. F-03 Busy-loop on soft-skip/failure release (Phase 1)
4. F-07 Rollup can delete/overwrite native candles (Phase 2)
5. F-10 CoinGecko search/tickers pacer bypass + unsignaled 429 (Phase 3)
6. F-11 No HTTP timeouts on provider clients (Phase 3)
7. F-20 Non-canonical `"daily"`/`"hourly"` interval stamps (Phase 4)
8. F-21 Paid tiers get demo header/host (Phase 4)
9. F-29 Quotes endpoints ignore `vs_currency` (Phase 5)
10. F-30 Unbounded `coin_quotes` reads (Phase 5)

**Medium**
11. F-38 Metric name drift (Phase 6)
12. F-39 LISTEN relays unsupervised (Phase 6)
13. F-40 DB credential encoding / PgConnectOptions (Phase 6)
14. F-41 Drain sleep → bounded timeout (Phase 6)
15. F-04 Dispatch error classification (Phase 1)
16. F-05 Live-poller claim LIMIT + shutdown check (Phase 1)
17. F-12 `is_transient` 4xx (Phase 3)
18. F-13 acquire_slot clamp observability (Phase 3)
19. F-14 Shared request/pace/parse helpers (Phase 3)
20. F-15 Startup pacer-row validation (Phase 3)
21. F-22 Binance `volume_24h` (Phase 4)
22. F-23 Scientific-notation Decimal fallback (Phase 4)
23. F-24 Derivatives symbol matching (Phase 4)
24. F-31 Aggregation `end` bound + cap-cursor (Phase 5)
25. F-32 `register_coin` idempotency + transaction (Phase 5)
26. F-33 Extractor-rejection error bodies (Phase 5)
27. F-34 Search 503 mapping (Phase 5)
28. F-35 `as_of` recompute ceiling (Phase 5)
29. F-42 Sustained `all_providers_down` signal (Phase 6)
30. F-08 Rollup history low-watermark repair (Phase 2)
31. F-16 Per-provider pacing in chain fallback (Phase 7)
32. F-50 Provider trait default bodies (Phase 7)
33. F-51 Batch upserts + NOTIFY policy (Phase 7)
34. F-52 Cycle-overlay batch inserts (Phase 7)

**Low**
35. F-06 `last_error` hygiene + logged marker clears (Phase 1)
36. F-09 log10/ln non-positive guard (Phase 2)
37. F-17 `signal_cooldown` GREATEST (Phase 3)
38. F-18 `NotFound` mislabel (Phase 3)
39. F-25 Binance snapped-limit (Phase 4)
40. F-26 Empty-chain error label (Phase 4)
41. F-27 Logged normalization degradations (Phase 4)
42. F-28 Typed DTOs in search paths (Phase 4)
43. F-36 WebSocket read loop (Phase 5)
44. F-57 Stale partition anchor (Phase 5)
45. F-58 Stale db_integration scenarios + test rename (Phase 5)
46. F-43 Ready-before-bind ordering (Phase 6)
47. F-44 Readiness cache flag ordering (Phase 6)
48. F-45 Env-parse diagnostics (Phase 6)
49. F-46 Generic supervisor + restart backoff (Phase 6)
50. F-47 Dropped-sender busy-spin arms (Phases 1/6)
51. F-48 `observe_chain_records` doc/behavior (Phase 6)
52. F-53 Duplication consolidation (Phase 7)
53. F-54 Interval/tier enums, query-key enum (Phases 4/7)
54. F-55 AppState dead fields + test constructor (Phase 7)

**Informational / intentionally not scheduled for implementation**
- F-19 User-Agent (folded into Phase 3's client helper), F-37 API minor notes, F-49 build/observability nits (selected items folded into Phase 6: ghost gauge, dead `start_api_server`, `HeaderExtractor` dedup; Dockerfile/Makefile nits left as optional), F-56 structural notes (config snapshot folded into Phase 6 as optional; `Arc<[…]>`, edition 2024, dead fields in `CgMarketItem` left as optional craft), F-59 test observations (parameter-parity test folded into Phase 5; vacuous-test cleanup left as optional). These are explicitly judged not to warrant standalone implementation work beyond the noted fold-ins.
