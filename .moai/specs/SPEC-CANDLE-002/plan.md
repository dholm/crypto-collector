# SPEC-CANDLE-002 — Implementation Plan

Development mode: **TDD** (brownfield RED → GREEN → REFACTOR). This plan is ordered by
**decision-reversibility**: the highest-change-likelihood design decisions (collision semantics,
watermark mechanism) lead; mechanical SQL edits and doc/tag updates follow, so human review focuses
on the decisions most likely to change.

## Goal

Harden the rollup materializer and cycle projection per REQ-CANDLE-050..060 — source-filtered
reconcile + native-wins collisions (F-07), source low-watermark history repair (F-08), non-positive
projection guard (F-09) — WITHOUT a new migration, WITHOUT a new dependency, WITHOUT forking the
`candles_agg.rs` folding math, and WITHOUT tuning the backtest-locked projection constants.

---

## 1. Collision policy — native-wins upsert (D1, highest stakes)

Highest change-likelihood decision: it defines what happens to genuine provider data on a PK
collision and is a data-model behavior contract.

**Requirement:** REQ-CANDLE-052. **Site:** `batched_upsert_candles` (`rollup.rs:114-163`).

Current upsert (`:141-147`) unconditionally overwrites on conflict:

```
ON CONFLICT (coin_id, vs_currency, interval, ts) DO UPDATE SET
    open = EXCLUDED.open, ..., source = EXCLUDED.source
```

**Change (native-wins):** gate the `DO UPDATE` on the *existing* row being a rollup row:

```
ON CONFLICT (coin_id, vs_currency, interval, ts) DO UPDATE SET
    open = EXCLUDED.open, high = EXCLUDED.high, low = EXCLUDED.low,
    close = EXCLUDED.close, volume = EXCLUDED.volume, source = EXCLUDED.source
WHERE coin_candles.source LIKE 'rollup:%'
```

Semantics: when the conflicting existing row is native (`coin_candles.source` NOT `rollup:%`), the
`WHERE` is false → the conflict is a no-op → the native row is preserved byte-identical. When the
existing row is a prior rollup row, the update proceeds as before (idempotent re-upsert of the
forming/closed bucket, REQ-CANDLE-040 unaffected). This is a **single-clause** change and applies
uniformly to the forward recompute, the initial backfill, and the new backward-repair pass (all
route through `batched_upsert_candles`).

**Trade-off:** a colliding native row is never upgraded to the rollup value even if the rollup value
were "better". This is intended (D1) — provider data is the source of truth. Reversibility of D1 is
HIGH (one WHERE clause), which is why it leads this plan.

**@MX:ANCHOR** here (see §5).

## 2. Source-filter invariant — reconcile reads/deletes rollup rows only (F-07 core)

**Requirements:** REQ-CANDLE-050/051/053. **Site:** `incremental_recompute_target`
(`rollup.rs:240-327`).

Two SQL edits, mirroring the filter `recompute_start`'s `MAX(ts)` query already carries (`:250-253`):

1. `previously_materialized` SELECT (`:274-284`) — append `AND source LIKE 'rollup:%'`. After this,
   the `previously_materialized` slice handed to the pure `reconcile_window` (`:87-98`) contains ONLY
   rollup rows, so its computed `deletes` can never contain a native `ts`. **`reconcile_window` stays
   pure and unchanged** — the domain narrowing happens at the SQL boundary that feeds it.
2. Per-`ts` DELETE (`:313-324`) — append `AND source LIKE 'rollup:%'` as a belt-and-suspenders guard
   so that even if a `ts` were somehow selected, the DELETE cannot remove a native row.

Combined with §1's native-wins upsert, this is a **three-layer defense**: (a) native rows are never
read as materialized (filtered SELECT); (b) native `ts` are never deleted (filtered DELETE); (c) a
native row colliding with an emitted bucket is never overwritten (native-wins upsert). Set-parity for
the rollup-owned subset (REQ-CANDLE-053) holds because the reconcile's domain is simply narrowed to
rollup rows — within that domain the existing REQ-CANDLE-005/022 logic is untouched.

**@MX:ANCHOR** here (see §5).

## 3. Source low-watermark history repair (F-08)

**Requirements:** REQ-CANDLE-054/055/056/057. **Site:** `incremental_recompute_target`
(after the forward recompute), reusing `backfill_target`'s window walk (`rollup.rs:171-230`).

**Watermark derivation (query-derived, D2 — no migration):** at recompute time, compare two
already-cheap aggregates:
- `source_min = SELECT MIN(ts) FROM coin_candles WHERE coin_id=$1 AND vs_currency=$2 AND interval=<source_interval>`
- `earliest_materialized = SELECT MIN(ts) FROM coin_candles WHERE coin_id=$1 AND vs_currency=$2 AND interval=<target_interval> AND source LIKE 'rollup:%'`

Both are indexed `MIN(ts)` lookups on the PK-fronted `(coin_id, vs_currency, interval, ts)` shape —
cheap, no new index, no new column. If querying ever proves too expensive (it should not at this
scale), a `rollup_low_watermark` state column would be the fallback — NOT taken by default, and it
would require an explicit justification amendment here (D2).

**Pure decision core (unit-testable, no DB):** extract a pure function, e.g.

```
fn backward_repair_window(
    source_min_ts: DateTime<Utc>,
    earliest_materialized_ts: DateTime<Utc>,
    target_secs: i64,
) -> Option<(DateTime<Utc> /* start, week-aligned */, DateTime<Utc> /* end-exclusive */)>
```

The trigger and the walk-start use **two distinct alignments** — this is load-bearing for
self-termination. Returns `Some((bucket_start(source_min, WEEK_SECS), earliest_materialized))`
when the **target-interval bucket** of the source low-watermark precedes the earliest
materialized bucket — i.e. when `bucket_start(source_min, target_secs) < earliest_materialized`
(day bucket for `1d`, week bucket for `1w`) — else `None`. The returned window START stays
**week-aligned** (`bucket_start(source_min, WEEK_SECS)`) so the walk chunk boundary never splits a
`1d`/`1w` bucket (memory bound). Using a WEEK-aligned *trigger* for a `1d` target would be a bug:
after a repair the earliest `1d` bucket is DAY-aligned, and `week_bucket(source_min) <
day_bucket(source_min)` holds for ~6/7 of coins (any non-epoch-Thursday day), so the pass would
re-fire on every recompute. The target-interval-aware trigger is what makes it terminate. This
mirrors the existing pure-core style (`reconcile_window`, `page_end_secs`, `pacer_decision`) and is
the AC-CANDLE-055 pure test target.

**Execution:** when the pure core returns `Some(window)`, walk `[start, earliest_materialized)` in
the SAME week-aligned `WEEK_SECS * BACKFILL_CHUNK_WEEKS` chunks `backfill_target` already uses
(`rollup.rs:194-199`), folding each window via `candles_agg.rs` and upserting via
`batched_upsert_candles` (so native-wins §1 applies here too). Per-window memory is bounded
(REQ-CANDLE-056). **Idempotent + self-terminating (target-interval-aware trigger):** after the
first repair the earliest materialized bucket moves back to the TARGET-interval bucket of
`source_min` — the DAY bucket for `1d`, the WEEK bucket for `1w`. Because the trigger compares
`bucket_start(source_min, target_secs)` (NOT the week-aligned walk start) against
`earliest_materialized`, the next run's core returns `None` for BOTH `1d` and `1w` — no repeat.
(A week-aligned trigger would have re-fired the `1d` pass every recompute for any non-Thursday
day; the target-aware trigger is the fix. Any residual overshoot within a single fired pass
re-materializes identical `rollup:*` buckets idempotently, native-wins protects any native row.)

**End-bound handling (`backfill_target` loop reuse — explicit implementer note):** `backfill_target`'s
loop is `while window_start <= ceiling { window_end = window_start + chunk; … }` (`rollup.rs:198-199`).
Reusing it with `ceiling = earliest_materialized` means the LAST window can **overshoot** — `window_end`
may exceed `earliest_materialized`, re-folding buckets at/after the exclusive end. This overshoot is
**provably safe**: those buckets are already `rollup:*` rows, so re-materializing them is idempotent,
and the native-wins upsert (§1) protects any native row regardless — no data loss. The implementer MUST
pick ONE and note it in code: **(a) accept the idempotent overshoot** (recommended — reuses
`backfill_target` verbatim; the overshoot re-writes existing rollup buckets to identical values), or
**(b) honor the exclusive end** by clamping the final `window_end` to `earliest_materialized` (or
skipping emitted buckets whose `ts >= earliest_materialized`). Default: (a).

**Ordering vs forward recompute:** the forward recompute (max-bucket → now) and the backward repair
(source_min → earliest) touch disjoint ranges; order between them is immaterial. Do the forward
recompute first (unchanged path), then the backward check — keeps the existing hot path first.

**@MX:WARN** on the backward pass (memory bound) (see §5).

## 4. Non-positive projection guard (F-09) — ordering is load-bearing

**Requirements:** REQ-CANDLE-058/059/060. **Site:** `cycle_projection.rs` `project_composite`
(`:488-515`), `fit_model` (`:284-324`).

**Ordering constraint (do NOT reorder — this is the fix for the plan-audit MAJOR defect):** in
`project_composite`, `today = series.keys().next_back()` (`:497`) and `current_price = series[&today]`
(`:499`) — **`current_price` is derived FROM the series**. Filtering `close <= 0` out of the series
*before* deriving `current_price` would let a series with positive history and a non-positive LAST
(today) close silently shift `today` to the last positive day and make `current_price` positive —
turning the REQ-CANDLE-059 guard (`:513`) into unreachable dead code and making AC-CANDLE-058's
`current_price = 0` sub-case unsatisfiable. The three steps MUST run in this order:

1. **Derive from the UNFILTERED series (unchanged, `:496-499`).** Build `series`, then `today` /
   `earliest` / `current_price` from it. Do NOT filter first.
2. **Guard `current_price` BEFORE the fit (REQ-CANDLE-059).** Immediately after `:499` (ahead of the
   `current_price.log10()` at `:513`) add `if current_price <= Decimal::ZERO { warn!(...); return
   vec![]; }` — a graceful empty return, consistent with the existing `< CYCLE_DAYS → vec![]` and
   `fit_model → None → vec![]` degradations (`:501-508`). No panic. This guard is reachable ONLY
   because it precedes the interior filter.
3. **Filter interior `close <= 0` for the fit ONLY (REQ-CANDLE-058).** Build a filtered `fit_series`
   (drop entries with `close <= 0`, `warn!` each `(date, close)`) and feed it to `fit_model` (so the
   spine `p.log10()` at `:294` and the residual bins `p.log10()` at `:324` only ever see positive
   closes) and to `build_band_grid` (`:509`). `today` / `current_price` remain the unfiltered values
   from step 1, so continuity (`g0`, `:515`) still anchors at today's real price.

- **Pure-testable (REQ-CANDLE-060, AC-CANDLE-058):** both the interior filter and the `current_price`
  guard are pure and exercised without a DB — (a) a series with a positive last close but an interior
  `0` and interior negative close → a fit over the positive subset, no panic, guard NOT triggered;
  (b) a series whose last/today close is `0` → the guard fires and returns empty gracefully, no panic.
  Sub-case (b) is satisfiable ONLY under the step-1→2→3 ordering above.
- **Not a panic path — leave unchanged:** `pow10` (`:110-113`) calls `.ln()` on the literal
  `dec!(10)` (constant, always positive). Do NOT add a guard there. `days.log10()` (`:293/:307/:324`)
  is days-since-genesis (always positive for real 2011+ data) — out of scope per F-09 (close /
  current_price only).
- **Constants untouched:** no change to `CALIBRATION_ANCHORS`, weights, or any fit constant;
  `tests/backtest_projection.rs` MUST pass unchanged.

## 5. MX Tag Targets

| Tag | Site | Text (intent) | Sub-lines |
|-----|------|---------------|-----------|
| `@MX:ANCHOR` | `incremental_recompute_target` | rollup reconcile reads/deletes ONLY `source LIKE 'rollup:%'`; never touches native provider rows | `@MX:REASON` (data-loss prevention), `@MX:SPEC SPEC-CANDLE-002 REQ-CANDLE-050 REQ-CANDLE-051` |
| `@MX:ANCHOR` | `batched_upsert_candles` | native-wins: `ON CONFLICT DO UPDATE ... WHERE coin_candles.source LIKE 'rollup:%'` — a colliding native row is never overwritten | `@MX:REASON` (collision policy D1), `@MX:SPEC SPEC-CANDLE-002 REQ-CANDLE-052` |
| `@MX:WARN` | backward-repair pass | memory-bounded: walk week-aligned windows; never fetch the full source series (256 Mi pod) | `@MX:REASON` (OOM prevention), `@MX:SPEC SPEC-CANDLE-002 REQ-CANDLE-055 REQ-CANDLE-056` |

Also update the rollup module-level doc comment to state the source-filter invariant and the
watermark/history-repair behavior. Existing `@MX:NOTE`/`@MX:SPEC` on `batched_upsert_candles`
(`:107-113`) — extend its `@MX:SPEC` line to include the new REQ IDs; keep the existing text.

## 6. Test Plan (TDD, brownfield RED → GREEN → REFACTOR)

Pure unit tests (colocated in `rollup.rs` / `cycle_projection.rs`, no DB):
- `reconcile_window` with a rollup-only `previously_materialized` slice still yields correct
  deletes (characterization — confirms purity unchanged).
- `backward_repair_window` pure core: returns `Some` when source precedes earliest materialized,
  `None` otherwise, week-aligned start. (AC-CANDLE-055 pure portion)
- Projection guard pure core: series with `{0, -1, positive...}` → fit over positive subset, no
  panic; `current_price = 0` → empty. (AC-CANDLE-058)

DB-gated integration tests (`#[ignore]` + `DATABASE_URL`, run `--test-threads=1` — the DB-gated
suite shares a global claim queue, per CLAUDE.md Integration Tests):
- Mixed-source window: seed native `1d` + rollup rows; run a full reconcile cycle; assert native
  rows byte-identical AND rollup rows set-parity. (AC-CANDLE-050)
- Collision: seed a native row at a `ts` an emitted rollup bucket will land on; run reconcile;
  assert native row unchanged. (AC-CANDLE-052)
- History-repair: materialize, then insert source rows behind the watermark; run `rollup`; assert
  the `1d`/`1w` series now covers the backfilled history; re-run and assert no further change
  (idempotent). (AC-CANDLE-055)

RED-first: the mixed-source regression (AC-CANDLE-050) and the projection zero/negative test
(AC-CANDLE-058) should FAIL against current `main` before the fix, proving they test the defect.

## 7. Risks, Trade-offs & Dependencies

- **F-07 three-layer redundancy is intentional.** Filtered SELECT + filtered DELETE + native-wins
  upsert overlap deliberately; each closes the gap independently so a future refactor touching one
  layer cannot silently reopen the data-loss path.
- **Backward-repair range depth.** For a very deep late backfill the backward range can span years,
  but each week-aligned window is memory-bounded (REQ-CANDLE-056) and the pass runs at most once per
  new watermark (self-terminating). Walking the whole backward range in one `rollup` invocation
  matches the existing `backfill_target` full-range behavior — no per-run window cap is added.
- **F-09 filter changes projection output when bad data exists.** Dropping `close <= 0` rows means a
  coin with corrupt closes gets a fit over fewer points. This is strictly better than a crash-loop;
  the `warn!` surfaces the dropped rows for diagnosis. Backtest (`tests/backtest_projection.rs`) uses
  clean BTC data, so it is unaffected and MUST stay green.
- **No migration / no dependency / no API change** — smallest possible blast radius; all changes are
  internal to `src/collectors/`.
- **Sequencing:** independent of Phase 1 (SPEC-SCHED-002); MUST precede Phase 7. No other blocker.

## 8. PRESERVE list (scope discipline)

Touch ONLY:
- `src/collectors/rollup.rs`
- `src/collectors/cycle_projection.rs`
- their colocated tests (+ any `#[ignore]` DB integration test file the project convention places
  these in — mirror where SPEC-CANDLE-001's rollup DB tests live)

Do NOT modify: `src/api/candles_agg.rs` (folding SSOT), `src/api/candles.rs` (read path), any
migration, `Cargo.toml` (no new dependency), `tests/backtest_projection.rs` (backtest-locked),
`src/collectors/collection_queue.rs` dispatch, or any unrelated file.
