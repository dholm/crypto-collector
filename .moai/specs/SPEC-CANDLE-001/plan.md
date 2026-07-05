# SPEC-CANDLE-001 — Implementation Plan

Materialize native `1d`/`1w` OHLCV into `coin_candles` so reads take the endpoint's fast native
path. Brownfield; delta markers per file. This plan is WHAT/WHY-adjacent scaffolding for the Run
phase — it names files and structure only to ground the requirements, not to prescribe code.

## Grounding facts (verified `file:line`)

Every fact below was read from the live tree during planning and is load-bearing.

| # | Fact | Citation |
|---|------|----------|
| 1 | Read path prefers native FIRST via coin-level `EXISTS` probe; aggregation only when `native_exists==false`. No read-path change needed. | `src/api/candles.rs:100-148` (probe `:106-116`, native branch `:118-148`), aggregation `:150-292` |
| 2a | Bucket alignment: epoch truncation; `1d`=86400s→UTC midnight, `1w`=604800s→epoch-**Thursday** (NOT ISO Monday). OR-API3-6. | `src/api/candles_agg.rs:146-158` |
| 2b | `interval_to_seconds` fixed-duration table (`5m`=300, `1d`=86400, `1w`=604800; `1M`→None). | `src/api/candles_agg.rs:28-48` |
| 2c | `aggregate_candles(source, target_secs, source_secs, now, source_label, target_interval)`: open=first, close=last, high=max, low=min; ts DESC out; stamps `aggregated:<label>`. | `src/api/candles_agg.rs:206-291` |
| 2d | `fold_volume`: `Some(sum)` iff all components `Some`, else `None` — never coerces missing to zero. | `src/api/candles_agg.rs:175-181` |
| 2e | Completeness: closed = `bucket_end<=now`; incomplete closed buckets DROPPED; forming bucket (contains `now`) always emitted even if partial. | `src/api/candles_agg.rs:244-286` |
| 2f | `select_source_interval(stored, target_secs, window_start, now)`: source-interval chooser used by the read path — coverage-scored divisor, tie-break to the **larger** divisor. Reuse it (not a hand-rolled "finest") so materializer and read path pick the same source → parity. | `src/api/candles_agg.rs:96-135` |
| 3 | OOM constraint: full multi-year `5m` history ≈1M rows OOM-kills the 256Mi pod; cycle-overlay avoids it with SQL-side `DISTINCT ON`. | `src/collectors/cycle_overlay.rs:488-513` (`@MX:WARN` `:494-498`) |
| 4a | `coin_candles`: `PARTITION BY RANGE (ts)`, PK `(coin_id, vs_currency, interval, ts)`, `volume` NULLABLE. Static partitions only 2024-01→2027-12. | `migrations/0011_remove_markets.sql:107-179` |
| 4b | `upsert_coin_candle`: ON CONFLICT `(coin_id,vs_currency,interval,ts)` DO UPDATE; calls `ensure_candle_partition`; one tx per row + a `pg_notify` each. | `src/db/upserts.rs:105-153` |
| 4c | `ensure_candle_partition`: idempotent monthly partition creation (advisory lock + in-memory cache); creates 2017-2023 partitions dynamically. | `src/db/partitions.rs:39-75` |
| 5a | Dispatch match; network-free precedent arm `("coin","cycle_overlay")` (DB-only derived rebuild, no provider/pacer). | `src/collectors/collection_queue.rs:329-590` (arm `:576-585`) |
| 5b | `enqueue_queue_item(pool, target_kind, target_id, kind)` → `ON CONFLICT DO NOTHING` dedup. | `src/collectors/collection_queue.rs:211-224` |
| 5c | Periodic tick `enqueue_periodic_refresh`; `REFRESH_KINDS=["market","metadata","candles"]`; active-coin query; cycle_overlay enqueued separately for single coin. | `src/main.rs:426-490` (`REFRESH_KINDS` `:430`, active query `:437`, cycle enqueue `:476-489`) |
| 5d | Startup backfill enqueue precedent. | `src/collectors/backfill.rs:381-414` (called `src/main.rs:218`) |
| 5e | Per-coin interval discovery query already exists (reuse it + `interval_to_seconds` to pick source). | `src/api/candles.rs:159-167`, `src/collectors/cycle_overlay.rs:467-474` |
| 6 | `collection_queue.kind` has a CHECK constraint; new kinds need `ALTER TABLE DROP/ADD collection_queue_kind_check`. Current enum: spot, candles, metadata, market, derivatives, cycle_overlay. | `migrations/0014_collection_queue_cycle_overlay_kind.sql` |
| 7 | `CoinCandle`: `interval` plain `String`, `volume: Option<Decimal>`. | `src/models/quote.rs:33-46` |
| 8 | Characterization test asserts native rows do NOT start with `aggregated:` → materialized rows must use a distinct marker (`rollup:<source_interval>`). | `src/api/candles.rs:600-608` |
| 9a | Pure unit tests (bucketing/volume/gap) live here; fixtures `make_candle` / `make_candle_null_vol`. | `src/api/candles_agg.rs:295-1105` (fixtures `:321-361`) |
| 9b | DB-gated `#[ignore]` handler scenarios use `db_test_server()`. | `src/api/candles.rs:505-844` |

## Technical approach

### D1 — Chunked backfill (reuse, do not fork)

1. Select the source interval with `select_source_interval` (fact 2f) — the **same selector the read
   path uses** (coverage-scored, tie-break to the larger divisor) — called per target (`1d`, `1w` may
   pick differently) with `window_start = None` (full history) and the injected `now`. Do NOT hand-roll
   a "finest divisor" rule; matching the read path's selector is what preserves parity for
   multi-interval coins. (The coverage query of fact 5e still feeds the `IntervalCoverage` slice that
   `select_source_interval` scores.)
2. Walk `[earliest_ts .. now]` in **week-aligned windows** — `[w, w + K*604800)` where `w` is a
   `bucket_start(_, 604800)` boundary. Because `604800 % 86400 == 0`, whole `1d` and `1w` buckets
   are fully contained in each window; none straddle a boundary.
3. For each window, `SELECT`-only that window's source rows, call `aggregate_candles(...)` twice
   (target `1d` and `1w`), then **relabel** each returned row's `source` field to
   `rollup:<source_interval>` (a post-fold map — `aggregate_candles` itself stamps `aggregated:*` at
   `candles_agg.rs:230`, which the relabel overwrites; OHLCV/`ts`/`interval` untouched), and upsert the
   relabeled rows. Per-window row count is bounded (e.g. K weeks × 2016 `5m`/week), so memory stays
   flat regardless of total history length.
4. Insert via a **batched** partition-safe path (REQ-CANDLE-043) rather than looping
   `upsert_coin_candle` (which is one tx + one `pg_notify` per row — pathological for a historical
   backfill). The batched path preserves the same conflict target and calls `ensure_candle_partition`
   for each distinct month touched.

### D2 — Incremental maintenance (`kind='rollup'`)

1. Migration widens `collection_queue_kind_check` to admit `rollup` (fact 6, template 0014). This
   ships and runs at startup **before** any enqueue path is activated (ordering matters — see Risks).
2. New dispatch arm `("coin","rollup")` mirrors `("coin","cycle_overlay")` (fact 5a): DB-only, no
   provider, no pacer.
3. Enqueue triggers: (a) after `("coin","candles")` completes, enqueue `("coin", coin_id, "rollup")`;
   (b) periodic tick backstop in `enqueue_periodic_refresh` for each active coin. Duplicates are
   dedup-absorbed (fact 5b, REQ-CANDLE-023).
4. The `rollup` task, when materialized rows already exist, recomputes **forward-only**: find
   `MAX(ts)` (= max materialized `bucket_start`) per target interval, reload source from the earliest
   still-forming target `bucket_start` (≤ ~1 week of source for `1w`), re-run `aggregate_candles`, and
   **reconcile the window**: upsert the emitted buckets AND delete any previously-materialized bucket in
   `[recompute_start, now]` that `aggregate_candles` no longer emits (REQ-CANDLE-022). No full-history
   rescan. First run for a coin with no rows → full backfill (D1, REQ-CANDLE-010).

## Milestones (priority-ordered, no time estimates)

1. **Migration + queue kind (Priority High).** Add `migrations/00NN_collection_queue_rollup_kind.sql`;
   add `("coin","rollup")` dispatch arm as a stub. Verify enqueue no longer violates the CHECK
   constraint. Foundation — everything else depends on it.
2. **Pure rollup unit tests + materializer core (Priority High).** New `src/collectors/rollup.rs`
   folding `1d`/`1w` from a supplied source slice via `aggregate_candles`, stamping `rollup:*`.
   Reproduction test 1 (unit) written to fail first. Byte-for-byte parity is proven here.
3. **Chunked full-history backfill (Priority High).** Week-aligned window walk + batched
   partition-safe insert. Backfill BTC `1d`/`1w`. Verify bounded memory (no OOM) and 2017→today span.
4. **Forward-only incremental + enqueue wiring (Priority Medium).** Post-candles enqueue + periodic
   backstop; forward-only recompute. Reproduction test 2 (incremental) written to fail first.
5. **Read-path characterization (Priority Medium).** DB `#[ignore]` test proving native serve after
   materialization (reproduction test 3). No read-path code change — this is a guardrail test.
6. **Quality gate (Priority High).** `cargo fmt --check`, `cargo clippy --all-targets --all-features
   -- -D warnings`, `cargo test` all green, including all prior coverage-aware tests.

## Risks & mitigations

- **Parity drift if math is forked (why SQL-side `DISTINCT ON` was rejected).** Cycle-overlay uses
  `DISTINCT ON (day)` for daily close — but that only needs `close`, not full OHLCV with
  volume-null-propagation and incomplete-bucket dropping. Reimplementing those in SQL risks silent
  divergence from `candles_agg.rs` (e.g. `SUM(volume)` coerces NULL differently than `fold_volume`;
  a partial closed day would not be dropped). Mitigation: reuse `aggregate_candles` in-process on
  bounded windows (D1). The chunk bound is what makes in-process reuse memory-safe.
- **Forming bucket that later closes incomplete (staleness).** A partial forming row that later
  becomes an incomplete closed bucket must not linger (read-time output would drop it). REQ-CANDLE-022
  makes the window-reconcile **normative**: the forward-only recompute deletes any materialized bucket
  in `[recompute_start, now]` that `aggregate_candles` no longer emits, then upserts the emitted set.
  This stays bounded (forward-only, ≤ ~1 week of source for `1w`) while guaranteeing exact set-parity —
  it is no longer a deferred "recommended" mitigation but a binding requirement.
- **Per-row upsert overhead on backfill.** `upsert_coin_candle` does one tx + one `pg_notify` per row;
  a full BTC backfill is thousands of `1d` + hundreds of `1w` rows. Mitigation: batched insert path
  (REQ-CANDLE-043) with the same conflict target and partition-ensure, one notify per batch (or none
  for historical backfill).
- **Migration ordering.** The enqueue paths must not activate before the CHECK-constraint migration
  has run. Since migrations run at startup via `sqlx::migrate!()` and the new enqueue code ships in
  the same binary, the migration precedes the first enqueue by construction — but note the
  `sqlx-migrate-embed-rebuild` gotcha: a migrations-only change needs the binary rebuilt or the deploy
  silently ships stale (guarded by `build.rs`).
- **Partition coverage.** Static partitions cover only 2024-2027; 2017-2023 partitions exist only
  because the `5m` backfill created them dynamically. The batched insert MUST call
  `ensure_candle_partition` defensively (REQ-CANDLE-013) rather than assume they exist.

## @MX tag targets (Run phase)

- `@MX:ANCHOR` on the rollup materializer entry (fan_in from dispatch arm + backfill + incremental).
- `@MX:WARN` + `@MX:REASON` on the batched insert path: "must call `ensure_candle_partition` per
  distinct month; must not fork `candles_agg.rs` folding; must preserve volume null-propagation."
- `@MX:NOTE` on the `rollup:<source_interval>` marker convention (distinct from `aggregated:`).
- `@MX:SPEC: SPEC-CANDLE-001 REQ-CANDLE-0NN` on new functions.

## Delegation

- Backend/DB implementation (Rust, sqlx, collectors): expert-backend during Run phase.
- Migration + queue widening: same, guided by the `migrations/0014` template.
- No frontend, no DevOps, no new provider work.
