---
id: SPEC-CANDLE-001
version: 0.2.2
status: draft
created: 2026-07-05
updated: 2026-07-05
author: dholm
priority: high
issue_number: 0
---

# SPEC-CANDLE-001 — Materialize Native 1d/1w OHLCV Candles (Rollup)

A **data-population** feature that materializes native `1d` and `1w` OHLCV rows into
`coin_candles` per tracked coin, computed from the source interval the read path would pick for an
unbounded read (via `select_source_interval` with `window_start = None`, typically `5m`) by reusing
the existing read-time selection and
bucketing math unchanged. Materialized rows carry the source marker `rollup:<source_interval>`
(e.g. `rollup:5m`) — applied as a post-fold relabel — to distinguish them from the ephemeral
read-time `aggregated:<label>` marker that is never persisted.

This SPEC writes **no new read-path code**. The candle read endpoint
`GET /v1/coins/{coin_id}/candles` (SPEC-API-002 REQ-API-130, SPEC-API-003 aggregation) already
runs a coin-level native `EXISTS` probe FIRST and serves native rows whenever any exist,
falling back to read-time aggregation only when none do. Populating native `1d`/`1w` rows
therefore makes those reads take the fast native path automatically. This is the load-bearing
premise of the whole SPEC — see "Load-Bearing Premise" below.

Data contract / storage: [SPEC-DB-001](../SPEC-DB-001/spec.md) (`coin_candles`, PK
`(coin_id, vs_currency, interval, ts)`, `src/models/quote.rs:33-46`). Read surface and
coverage-aware fallback: [SPEC-API-002](../SPEC-API-002/spec.md) / [SPEC-API-003](../SPEC-API-003/spec.md)
(`src/api/candles.rs`, `src/api/candles_agg.rs`). Scheduling machinery (collection-queue worker,
periodic tick): [SPEC-SCHED-001](../SPEC-SCHED-001/spec.md). The `Decimal`-not-`f64` money
invariant is REQ-PROV-012. The precedent for a network-free DB-only derived rebuild driven by a
new collection-queue kind is [SPEC-CYCLE-001](../SPEC-CYCLE-001/spec.md) (`cycle_overlay`).

## HISTORY

- 2026-07-05 (v0.2.2): Revision after independent plan-audit (iteration 3, 0.72). Resolved residual
  D2 leakage: two positive uses of "finest divisor" (Scope in-scope bullet and Scenario 1 aside) that
  contradicted the authoritative REQ-CANDLE-001 selector were re-aligned to `select_source_interval`
  (coverage-scored, tie-break to the larger divisor). Grep-verified no positive "finest" wording
  remains. All substantive defects (D1–D4, D9) confirmed resolved and not reintroduced.
- 2026-07-05 (v0.2.1): Revision after independent plan-audit (iteration 2, 0.80). Resolved D9 — the
  materializer selects with `window_start = None` (canonical full-history source) while the read path
  selects per request with `window_start = params.start` (`candles.rs:187`); scoped the parity claim to
  **unbounded** reads (REQ-CANDLE-001, Goal, Scenario 4 now pin `start` omitted so both selectors match),
  framing the bounded-read coarser-source case as a pre-materialization optimization that no longer
  applies once the native path serves one canonical `rollup:*` series. Added DoD coverage for
  REQ-CANDLE-021 (periodic backstop) and REQ-CANDLE-043 (batched insert) (D7).
- 2026-07-05 (v0.2.0): Revision after independent plan-audit (iteration 1, FAIL 0.60). Resolved four
  defects: D1 — source marker is a post-fold relabel of `aggregate_candles` output (REQ-CANDLE-003/004),
  removing the "unchanged vs rollup:*" contradiction; D2 — source selection reuses the read path's own
  `select_source_interval` (REQ-CANDLE-001), not a hand-rolled "finest divisor", so parity holds for
  multi-interval coins; D3 — window-reconcile promoted to a normative requirement (REQ-CANDLE-022) so
  a forming bucket that later closes incomplete is deleted, backing REQ-CANDLE-005's set-parity; D4 —
  parity restated as a testable, pinned (interval/start/vs_currency/fixed-`now`) OHLCV+bucket-set
  equality over closed complete buckets, `source` column excluded. Minor: REQ-CANDLE-032 relabeled
  Ubiquitous; REQ-CANDLE-042 enumerates the full widened kind set.
- 2026-07-05 (v0.1.0): Initial draft. Materialize native `1d`/`1w` OHLCV into `coin_candles`
  via a new network-free collection-queue `kind='rollup'`, computed by chunked (week-aligned)
  reuse of `src/api/candles_agg.rs` bucketing, upserted through the existing
  `(coin_id, vs_currency, interval, ts)` idempotent path. One-time full-history backfill plus
  forward-only incremental maintenance on each candle refresh. Read-time coverage-aware
  aggregation is preserved as the fallback for any coin/interval lacking materialized rows. New
  `REQ-CANDLE-0NN` range. Brownfield — see delta markers.

---

## Goal

Given the deep finer-interval (e.g. `5m`) candle history the collector already stores for a coin,
**materialize** the equivalent native `1d` and `1w` OHLCV series into `coin_candles` so that
`GET /v1/coins/{coin_id}/candles?interval={1d|1w}` is served by the endpoint's fast native path
instead of re-aggregating on every page. The materialized series must be **OHLCV-identical** to what
the current read-time aggregation produces for an **unbounded** read over the same closed, complete
buckets at a fixed injected `now` — identical `ts`, `interval`, open/high/low/close, and volume
(including `NULL` propagation),
and identical emitted-bucket set (incomplete closed buckets dropped, forming bucket present). Only the
`source` column differs by design (`rollup:*` vs `aggregated:*`). Correctness is thereby preserved
while read latency collapses from ~19 slow aggregation pages (~15s) to a few fast native pages (well
within the downstream chart's 3s fetch budget).

## Problem (Why)

The candle read endpoint is coverage-aware: when zero native `1d` rows exist it aggregates `1d`
from the deep `5m` history. That fixed a correctness gap (BTC daily bars must span 2017→today)
but made reads pathologically slow — each `1d` page re-aggregates ~50k `5m` rows (~800ms), and a
full walk of ~3,210 daily bars is ~19 pages ≈ 15.4s. The downstream chart wraps its fetch in a
3s timeout and aborts ("Failed to load candles"). The correct fix is to materialize the native
`1d`/`1w` rows once (and keep them current incrementally) so the read path serves them natively.

## Load-Bearing Premise (No read-path change required)

`list_candles` (`src/api/candles.rs:69-293`) runs a coin-level native precedence probe **first**:

- `native_exists` = `EXISTS(SELECT 1 FROM coin_candles WHERE coin_id=$1 AND interval=$2 AND vs_currency=$3)`
  (`src/api/candles.rs:106-116`). The probe is scoped to `(coin_id, interval, vs_currency)`, NOT
  to the current page window, so a deep cursor never wrongly flips to aggregation on page 2+.
- If `native_exists` is true → the native branch (`src/api/candles.rs:118-148`) returns exact-interval
  rows and **never aggregates**.
- Read-time aggregation (`src/api/candles.rs:150-292`) fires **only** when `native_exists` is false.

Therefore: inserting native `1d`/`1w` rows for a coin makes every subsequent read of that
coin/interval take the native path with zero read-path code changes. This premise is what the
entire SPEC leans on and MUST hold; the acceptance suite verifies it directly (Scenario 3).

## Scope

In scope:
- **Materializing** native `1d` and `1w` OHLCV rows per tracked coin / `vs_currency` from the source
  interval chosen by `select_source_interval` (the read path's own coverage-scored selector,
  tie-broken to the larger divisor; see REQ-CANDLE-001) among the stored fixed-duration intervals that
  evenly divide the target bucket.
- **Reusing** `src/api/candles_agg.rs` (`select_source_interval` for source selection, `bucket_start`,
  `interval_to_seconds`, `aggregate_candles`, `fold_volume`) for all selection / alignment / folding —
  no forked math; the sole post-processing is relabeling the output `source` field to `rollup:*`.
- **One-time full-history backfill** (chunked, week-aligned, bounded memory) for coins with no
  materialized rows.
- **Forward-only incremental maintenance** via a new network-free collection-queue `kind='rollup'`,
  enqueued after each candle refresh and from the periodic tick, dedup-absorbed by the queue.
- **A migration** widening the `collection_queue.kind` CHECK constraint to admit `rollup`.
- **Preserving** read-time coverage-aware aggregation as the fallback for any coin/interval that
  still lacks materialized rows.

Out of scope: see Exclusions. No HTTP schema change, no cursor-format change, no new provider,
no new scheduler, no removal of the read-time aggregation fallback, no downstream client change.

## Decisions Restated (authoritative)

Confirmed with the user; encoded here verbatim in intent. Not to be re-litigated.

- **D1 — Backfill compute: chunked reuse of `candles_agg.rs`.** Iterate a coin's source history
  in **week-aligned windows** (multiples of 604800s, which are also day-aligned, so whole `1d`
  AND `1w` buckets never straddle a chunk boundary). Load only each window's source (`5m`) rows,
  run the existing pure `aggregate_candles` / `bucket_start`, and upsert the results. This gives
  bounded memory + byte-for-byte parity + zero forked math. **Rejected:** SQL-side `DISTINCT ON`
  materialization (as used for cycle-overlay daily close), because it forks the bucketing math and
  risks parity drift on volume-null propagation and incomplete-bucket dropping. Rationale recorded
  in `plan.md` Risks.
- **D2 — Incremental maintenance: new collection-queue `kind='rollup'`.** Mirror the network-free
  `("coin","cycle_overlay")` dispatch arm (`src/collectors/collection_queue.rs:576-585`). Enqueue
  a `rollup` item after each candle refresh AND from the periodic refresh tick; duplicate items are
  absorbed by the queue's `ON CONFLICT DO NOTHING` dedup. The `rollup` task recomputes only buckets
  from the max-materialized `bucket_start` forward (re-upserting the forming day/week plus any newly
  closed buckets) — never a full-history rescan. Requires the `collection_queue_kind_check` widening
  migration (template `migrations/0014_collection_queue_cycle_overlay_kind.sql`).

---

## Requirements (EARS)

Delta markers: **[EXISTING]** current behavior relied upon (no change), **[MODIFY]** existing code
changed, **[NEW]** net-new behavior. Grounding `file:line` citations are tabulated in `plan.md`.

### Module 1 — Rollup materialization semantics (WHAT is produced) [NEW]

- **REQ-CANDLE-001** [NEW] (Ubiquitous): The rollup materializer **shall** produce native `1d` and
  `1w` OHLCV rows for each tracked coin / `vs_currency`, selecting the source interval that feeds
  aggregation via the **same function the read path uses** — `select_source_interval`
  (`src/api/candles_agg.rs:96-135`: candidates are stored fixed-duration intervals that evenly divide
  the target, scored by coverage-miss, tie-broken to the **larger** divisor) — called per target with
  `window_start = None` (materialize the full-history **canonical** series) and the injected `now` —
  selecting the deepest-coverage divisor for the whole series (for BTC, `5m`). Reusing this exact
  selector (not a hand-rolled "finest divisor") is what makes the materialized series match read-time
  output for an **unbounded** read (`start` absent → the read path also passes `window_start = None`,
  identical selector inputs). Note the read path selects per request with `window_start = params.start`
  (`src/api/candles.rs:187`): before materialization, a *bounded* read could pick a coarser recent
  source (e.g. `4h` over `5m`) for a multi-interval coin — but once native rows exist, the native path
  serves the single canonical `rollup:*` series for **every** request (bounded or not), which is the
  intended consistency improvement, not a divergence. For a single deep-`5m` coin like BTC the
  question is moot — there is only one divisor.
- **REQ-CANDLE-002** [NEW] (Ubiquitous): Materialized bucket timestamps **shall** be UTC-epoch-aligned
  via `bucket_start` — `1d` truncated to UTC midnight (86400s), `1w` anchored to epoch-Thursday
  (604800s, 1970-01-01), NOT ISO Monday — matching the OR-API3-6 alignment decision exactly.
- **REQ-CANDLE-003** [NEW] (Ubiquitous): Each materialized row's `source` **shall** be set — by a
  **post-fold relabel** of the rows returned by `aggregate_candles` — to `rollup:<source_interval>`
  (e.g. `rollup:5m`), a marker distinct from the read-time `aggregated:<label>` marker (which
  `aggregate_candles` stamps in-memory at `candles_agg.rs:230` and is never persisted). The relabel
  **shall** overwrite only the `source` field; `ts`, `interval`, and all OHLCV fields are left exactly
  as `aggregate_candles` produced them.
- **REQ-CANDLE-004** [NEW] (Ubiquitous): OHLCV folding **shall** be open=first, close=last,
  high=max, low=min, and volume=sum-with-null-propagation (volume is `NULL` if and only if any
  contributing source candle's volume is `NULL`), all computed in `rust_decimal::Decimal`, by reusing
  the `aggregate_candles` / `fold_volume` **numeric and bucketing logic unchanged**. The only
  post-processing applied to `aggregate_candles`' output is the `source`-field relabel of
  REQ-CANDLE-003; no OHLCV / `ts` / `interval` value is recomputed or altered.
- **REQ-CANDLE-005** [NEW] (Complex, State + Event): While a target bucket is the forming bucket
  (its window contains `now`), when the materializer runs it **shall** emit and re-upsert that
  bucket even if partial; while a target bucket is closed (`bucket_end <= now`) but missing any of
  its expected source candles, the materializer **shall** drop it (fabricate/interpolate nothing) —
  the same completeness policy as read-time aggregation. Set-parity with read-time output *across
  recompute runs* is enforced by the bounded window-reconcile in REQ-CANDLE-022 (which deletes stored
  buckets that later become non-emitted).

### Module 2 — Full-history backfill (chunked, bounded memory) [NEW]

- **REQ-CANDLE-010** [NEW] (Event-Driven): When the `rollup` task runs for a coin/interval that has
  no materialized rows, the system **shall** perform a full-history backfill by iterating the source
  history from its earliest stored `ts` to `now` in week-aligned windows.
- **REQ-CANDLE-011** [NEW] (Ubiquitous): Each backfill window **shall** be a bounded multiple of
  604800s (week-aligned, hence day-aligned) so that no `1d` or `1w` bucket straddles a chunk
  boundary, and **shall** load only that window's source rows into memory before folding.
- **REQ-CANDLE-012** [NEW] (Unwanted): If materializing a coin's full history would require loading
  its entire multi-year finer series into memory at once, then the system **shall not** do so — the
  week-aligned chunk bound caps per-window memory to keep the 256Mi pod from OOM-killing (the prior
  incident documented at `cycle_overlay.rs:488-498`).
- **REQ-CANDLE-013** [NEW] (Ubiquitous): Backfill **shall** write results through a partition-safe
  insert path that ensures the covering monthly partition exists (reusing `ensure_candle_partition`)
  for every `ts` across 2017→today, since the static partitions in migration 0011 cover only
  2024-01→2027-12.

### Module 3 — Incremental maintenance (queue kind `rollup`) [NEW/MODIFY]

- **REQ-CANDLE-020** [NEW] (Event-Driven): When new source candles are persisted for a coin during a
  candle refresh (the `("coin","candles")` dispatch completing), the system **shall** enqueue a
  `("coin", <coin_id>, "rollup")` work item.
- **REQ-CANDLE-021** [MODIFY] (Event-Driven): When the periodic refresh tick fires
  (`enqueue_periodic_refresh`, `src/main.rs:435-490`), the system **shall** additionally enqueue a
  `rollup` work item for each tracked (active) coin, as a backstop to the post-refresh trigger.
- **REQ-CANDLE-022** [NEW] (State-Driven): While processing a `rollup` work item, the system **shall**
  recompute only buckets from the max-materialized `bucket_start` forward (reloading source from the
  earliest still-forming target bucket — at most ~one week of source for `1w`) and **shall reconcile
  that bounded window**: it **shall** re-upsert every bucket `aggregate_candles` emits for the window
  AND **shall** delete any previously-materialized bucket in `[recompute_start, now]` that
  `aggregate_candles` no longer emits (e.g. a forming partial that later closed incomplete), and
  **shall not** perform a full-history rescan. This reconcile is what makes REQ-CANDLE-005's set-parity
  hold across runs.
- **REQ-CANDLE-023** [NEW] (Optional): Where multiple `rollup` items for the same coin are enqueued
  before the worker processes them, the queue's `ON CONFLICT DO NOTHING` dedup index
  (`enqueue_queue_item`) **shall** absorb the duplicates so at most one pending item exists.
- **REQ-CANDLE-024** [NEW] (Ubiquitous): The `rollup` task **shall** be network-free (DB-only),
  invoking no provider and no pacer, mirroring the `("coin","cycle_overlay")` dispatch arm.

### Module 4 — Read-path integration & fallback preservation [EXISTING/NEW]

- **REQ-CANDLE-030** [EXISTING] (Event-Driven): When a client requests
  `GET /v1/coins/{coin_id}/candles?interval={1d|1w}` and native rows exist for that
  `(coin_id, interval, vs_currency)`, the endpoint **shall** serve them via the native path
  (indexed, no aggregation) — relying on the unchanged native precedence probe.
- **REQ-CANDLE-031** [EXISTING] (Unwanted): If no native rows exist for a requested coin/interval,
  then the endpoint **shall** fall back to the existing coverage-aware read-time aggregation; this
  fallback **shall not** be removed or weakened by this SPEC.
- **REQ-CANDLE-032** [NEW] (Ubiquitous): The materialization **shall not** alter the HTTP response
  schema, the `TsKey` keyset cursor format (`src/api/cursor.rs:39-43`), or any part of the client
  contract — it is strictly a data-population change.

### Module 5 — Data integrity, idempotency & migration [EXISTING/NEW/MODIFY]

- **REQ-CANDLE-040** [EXISTING] (Ubiquitous): Materialized rows **shall** be idempotent via the
  existing `(coin_id, vs_currency, interval, ts)` upsert conflict target; a re-run updates the
  forming bucket in place rather than inserting a duplicate.
- **REQ-CANDLE-041** [EXISTING] (Ubiquitous): All materialized OHLCV values **shall** be
  `rust_decimal::Decimal`, never `f64` (REQ-PROV-012), preserved by reusing the `CoinCandle` model
  and `aggregate_candles` unchanged.
- **REQ-CANDLE-042** [MODIFY] (Unwanted): If a `rollup` item is enqueued while the
  `collection_queue.kind` CHECK constraint does not permit `rollup`, then the enqueue fails at
  runtime; therefore a migration **shall** widen `collection_queue_kind_check` to include `rollup`
  (following the `migrations/0014` DROP/ADD template) before the enqueue paths are activated. The
  re-added constraint **shall** enumerate the full kind set:
  `spot, candles, metadata, market, derivatives, cycle_overlay, rollup`.
- **REQ-CANDLE-043** [NEW] (Optional): Where a batched historical insert path is introduced to avoid
  the per-row transaction + `pg_notify` overhead of `upsert_coin_candle`, it **shall** preserve the
  identical `(coin_id, vs_currency, interval, ts)` conflict semantics and the same
  partition-ensure behavior, so parity and idempotency are unaffected.

---

## Edge Cases (summarized; full behavior in acceptance.md)

- **Forming bucket that later closes incomplete.** A partial forming day/week is upserted; if that
  bucket later closes while still missing source candles, `aggregate_candles` drops it.
  REQ-CANDLE-022's bounded window-reconcile deletes the now-non-emitted stored bucket on the next
  recompute, so it does not linger — parity with read-time output is restored without a full rescan.
- **No divisible source interval.** A coin whose only stored interval does not evenly divide 86400s
  (e.g. only `1M`) yields zero materialized rows; the read path then still falls back to read-time
  aggregation (REQ-CANDLE-031).
- **Sparse-history coin.** Incomplete closed buckets are dropped, producing intentional gaps that
  match read-time aggregation output — not an error.
- **Coin with pre-existing native `1d`/`1w` rows from a provider.** Rollup upserts by the same key;
  it overwrites with `rollup:*` source. (Provider-native daily rows are not currently produced for
  these coins; if that changes, precedence is a future decision, out of scope here.)

## Exclusions (What NOT to Build)

- **No change to the HTTP response schema**, the `TsKey` keyset cursor format
  (`src/api/cursor.rs:39-43`), or the client contract — this is a data-population change only.
- **Do NOT remove the coverage-aware read-time aggregation fallback** — it remains the safety net
  for any coin/interval lacking materialized rows.
- **No parallel scheduler** — reuse the existing `collection_queue` worker loop and periodic tick.
- **No downstream client change** — fast large-paged native reads must complete within the client's
  existing 3s fetch budget without client modification.
- **No new provider, no new pacer usage, no network calls** in the rollup path.
- **No forked bucketing math** — `candles_agg.rs` is the single source of truth for OHLC folding,
  volume null-propagation, alignment, and completeness.

## Traceability — affected files (delta markers)

| Marker | Path | Role |
|--------|------|------|
| [EXISTING] | `src/api/candles.rs:69-293` | Native precedence probe + fallback; relied upon, unchanged |
| [EXISTING] | `src/api/candles_agg.rs` | `select_source_interval`, `bucket_start`, `interval_to_seconds`, `aggregate_candles`, `fold_volume`; reused (source-field relabel is the only override) |
| [EXISTING] | `src/db/upserts.rs:105-153` | `upsert_coin_candle` (ON CONFLICT + `ensure_candle_partition` + notify); reused / templated |
| [EXISTING] | `src/db/partitions.rs:39-75` | `ensure_candle_partition`; reused by backfill/batched insert |
| [EXISTING] | `src/models/quote.rs:33-46` | `CoinCandle` model; reused unchanged |
| [MODIFY] | `src/collectors/collection_queue.rs:329-590` | Add `("coin","rollup")` dispatch arm; enqueue after `("coin","candles")` |
| [MODIFY] | `src/main.rs:426-490` | Add `rollup` enqueue to periodic refresh (backstop) |
| [NEW] | `src/collectors/rollup.rs` (proposed) | Rollup materializer: chunked backfill + forward-only incremental + batched insert |
| [NEW] | `migrations/00NN_collection_queue_rollup_kind.sql` | Widen `collection_queue_kind_check` to admit `rollup` |
| [NEW] | tests in `src/api/candles_agg.rs` / `src/collectors/rollup.rs` / `src/api/candles.rs` | Rollup unit + incremental + read-path (#[ignore]) tests |
