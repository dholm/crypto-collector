---
id: SPEC-CANDLE-002
title: "Materializer & Projection Data Integrity"
version: "0.1.0"
status: completed
created: 2026-07-24
updated: 2026-07-24
author: manager-spec
priority: High
phase: "v0.2.0"
module: "src/collectors"
lifecycle: spec-anchored
tags: "collectors, rollup, materializer, projection, data-integrity, tdd"
issue_number: null
related_specs: [SPEC-CANDLE-001, SPEC-CYCLE-001, SPEC-DB-001]
tier: M
---

# SPEC-CANDLE-002 — Materializer & Projection Data Integrity

Behavioral-correctness hardening of the rollup candle **materializer** and the cycle-projection
compute path. The rollup materializer defined in [SPEC-CANDLE-001](../SPEC-CANDLE-001/spec.md)
(`src/collectors/rollup.rs`) derives native `1d`/`1w` OHLCV from a finer source interval; its
incremental reconcile path currently lacks the `source LIKE 'rollup:%'` filter that its own
`recompute_start` query carries, so it can **delete or overwrite native provider rows** it does
not own. The projection path (`src/collectors/cycle_projection.rs`) calls `log10` on stored
closes and `current_price`, which **panics** in `rust_decimal` on a non-positive input.

Findings addressed: **F-07** (High, materializer deletes native candles), **F-08** (Medium,
forward-only recompute never repairs history behind the watermark), **F-09** (Low, projection
`log10`/`ln` panic on non-positive input) — Category B of `research/idiomatic-rust.md` §6. This is
**Phase 2** of the 7-phase review-driven improvement roadmap (§7 of that document).

Schema contract: [SPEC-DB-001](../SPEC-DB-001/spec.md) — `coin_candles` is a **flat table** since
migration `0020_coin_candles_departition.sql`, PK `(coin_id, vs_currency, interval, ts)`; rollup
rows carry `source = 'rollup:<interval>'`, native rows carry provider names (`bitstamp`,
`coingecko`, …). Materializer semantics baseline: [SPEC-CANDLE-001](../SPEC-CANDLE-001/spec.md)
(REQ-CANDLE-005 set-parity, REQ-CANDLE-011/012 bounded-window memory discipline, REQ-CANDLE-020/022
window reconcile, REQ-CANDLE-024 network-free dispatch) — this SPEC **extends and hardens** those
requirements for the rollup-owned subset; it does not relax them. Projection constants are
backtest-locked (`tests/backtest_projection.rs`) and are NOT tuned by this SPEC.

## Prerequisites / Sequencing

**Independent of Phase 1** ([SPEC-SCHED-002](../SPEC-SCHED-002/spec.md), completed — worker retry
& backpressure). This SPEC touches the materializer and projection write paths only; SPEC-SCHED-002
touches the worker retry/backpressure policy. The two share no code and may land in **either
order**. Both MUST land **before Phase 7** (structural / batching refactor), which batches these
same write paths and relies on the corrected behavior as its regression baseline (`research/idiomatic-rust.md`
§7 rationale: "pure refactors and batching land on top of corrected behavior").

## HISTORY

- 2026-07-24 (v0.1.0): Plan-audit delta (independent plan-auditor: PASS-WITH-DEBT 0.86, Tier M
  cleared). Fixed one MAJOR + two MINOR plan defects with no scope change and no new REQs: (MAJOR)
  corrected the F-09 guard ordering in plan.md §4 + acceptance Scenario 4 — `current_price` is
  derived from the series, so the REQ-CANDLE-059 guard must run BEFORE the interior `close <= 0`
  filter or it becomes dead code; (MINOR) relabeled REQ-CANDLE-052 / REQ-CANDLE-059 from the
  non-canonical `(Event-detected)` to GEARS `(Event-driven)`; (MINOR) stated the backward-repair
  window end-bound handling explicitly in plan.md §3 (accept the idempotent overshoot or clamp).
- 2026-07-24 (v0.1.0): Initial draft. Establishes **REQ-CANDLE-050..060** — the materializer &
  projection data-integrity block: (Module 1) source-filtered rollup reconcile + native-wins
  collision policy (F-07); (Module 2) source low-watermark history repair via a bounded backward
  pass (F-08); (Module 3) non-positive projection guard (F-09). Brownfield — extends SPEC-CANDLE-001
  REQ-CANDLE-0xx. Two files: `src/collectors/rollup.rs`, `src/collectors/cycle_projection.rs`. No
  new migration (default), no new dependency, no API/schema change.

---

## Goal

Make the rollup materializer **incapable of destroying the native provider rows it derives from**,
make it **repair history that arrives behind its watermark** without breaking the 256 Mi memory
bound, and make the cycle projection **panic-free on non-positive prices**. After this SPEC:

1. A recompute window containing both native `1d` rows and `rollup:*` rows leaves the native rows
   byte-identical after a full reconcile cycle, while the rollup rows still reconcile to set-parity
   with a full rebuild.
2. A collision between an emitted rollup bucket and a native row at the same PK resolves **native-wins**.
3. Source rows arriving behind the materialized watermark (deep-backfill-after-rollup) trigger a
   bounded backward pass that materializes the newly-available history — end state: the `1d`/`1w`
   materialized series covers the backfilled history.
4. A projection series containing a zero or negative close produces a fit over the positive subset
   without panicking; `current_price = 0` yields a graceful skip/error, never a panic.

## Problem (Why)

- **F-07 — the materializer can delete/overwrite source rows (High).** `incremental_recompute_target`
  (`src/collectors/rollup.rs:240-327`) reads `previously_materialized` (`:274-284`) and issues a
  per-`ts` DELETE (`:313-324`) with **no `source` filter**, even though `recompute_start`'s
  `MAX(ts)` query (`:250-253`) is correctly scoped to `source LIKE 'rollup:%'`. Any native
  provider-sourced `1d` row in the recompute window `[recompute_start, now]` is treated as
  previously materialized — `reconcile_window` upserts overwrite its OHLCV and relabel its `source`
  to `rollup:*`, and the unfiltered DELETE removes unreproduced native `ts`. A derived materializer
  destroying the source rows it derives from is unrecoverable without a re-fetch. Latent today only
  because the deep-backfill overlap that produces mixed-source windows is rare — but silent when it
  occurs.
- **F-08 — forward-only recompute never repairs history behind the watermark (Medium).** The
  incremental recompute starts at `MAX(ts)` of existing rollup rows and walks forward. Source
  candles arriving **behind** that point — a deep backfill completing after the rollup already ran
  (see the `bitstamp-deep-backfill` / `backfill-range-tier` memory context) — are never
  re-materialized; only the read-time coverage-aware fallback hides the divergence, and once native
  rollup rows exist for the coin the read path stops falling back, so the gap becomes user-visible.
- **F-09 — projection `log10`/`ln` panic on non-positive input (Low).** `fit_model` /
  `project_composite` call `.log10()` on every stored close (`cycle_projection.rs:293-294, 324`) and
  on `current_price` (`:513`). `rust_decimal`'s `MathematicalOps::log10`/`ln` **panic** on a
  non-positive argument. One zero or negative close turns every `cycle_overlay` dispatch into
  panic → worker restart → crash-loop alarm → permanent queue-item failure.

## Scope

In scope:
- **Source-filtered rollup reconcile** — scope the `previously_materialized` read AND the per-`ts`
  DELETE in `incremental_recompute_target` to `source LIKE 'rollup:%'`, so native rows are never
  read as materialized and never deleted (REQ-CANDLE-050/051).
- **Native-wins collision policy** — when an emitted rollup bucket collides with a native row at the
  same PK, the native row is preserved unchanged (REQ-CANDLE-052).
- **Source low-watermark history repair** — a query-derived `MIN(ts)` comparison that triggers a
  bounded, week-aligned backward materialization pass when source history precedes the earliest
  materialized rollup bucket (REQ-CANDLE-054/055/056/057).
- **Non-positive projection guard** — filter `close <= 0` rows out of the projection series and guard
  `current_price <= 0`, logging skips at `warn!`, so `log10`/`ln` is never reached with a
  non-positive argument (REQ-CANDLE-058/059/060).
- **Keeping the pure decision cores pure** — `reconcile_window` stays pure/DB-free; the new watermark
  decision and the projection input-filter are extracted as pure, unit-testable cores in the
  existing style.
- **Module documentation + @MX tags** — update the rollup module docs and `@MX:ANCHOR`/`@MX:WARN`
  tags to encode the source-filter invariant, the native-wins collision contract, and the
  watermark/history-repair behavior.

Out of scope: see Exclusions.

## Decisions Restated (authoritative)

Confirmed in the plan-phase task; encoded here verbatim in intent. Not to be re-litigated.

- **D1 — Collision policy: native wins.** When an emitted rollup bucket collides with an existing
  native-sourced row at the same `(coin_id, vs_currency, interval, ts)`, the native row wins: the
  rollup upsert MUST NOT overwrite its OHLCV or `source`. This is a normative requirement
  (REQ-CANDLE-052), documented at an `@MX:ANCHOR` in the batched upsert path. Rationale: a derived
  series must defer to genuine provider data; provider rows are the source of truth a rollup can only
  approximate. (The three-layer defense — filtered SELECT, filtered DELETE, and the native-wins
  upsert guard — is detailed in plan.md.)
- **D2 — Watermark mechanism: query-derived, no migration.** The source low-watermark is derived by
  comparing the source interval's `MIN(ts)` against the earliest materialized rollup bucket at
  recompute time (REQ-CANDLE-057). NO new migration, state column, or table is added by default.
  Only if querying proves too expensive in practice may a state column be introduced — and only with
  an explicit justification recorded in plan.md. Default: query-derived.
- **D3 — Projection guard: filter at series construction + guard `current_price` before the fit.**
  Non-positive closes are removed when the daily series is built (so both the spine samples and the
  residual bins only ever see positive closes); `current_price <= 0` is guarded before its `log10`
  is taken. The guard filters inputs only — the backtest-locked fit math over the positive subset is
  unchanged (REQ-CANDLE-058/059/060).

## Domain Model — affected sites (delta markers)

Delta markers: **[EXISTING]** relied upon unchanged, **[MODIFY]** changed, **[NEW]** net-new logic.

| Marker | Path | Role |
|--------|------|------|
| [MODIFY] | `src/collectors/rollup.rs:240-327` (`incremental_recompute_target`) | Add `AND source LIKE 'rollup:%'` to the `previously_materialized` SELECT (`:274-284`) and the per-`ts` DELETE (`:313-324`); add the source low-watermark comparison + bounded backward-repair trigger |
| [MODIFY] | `src/collectors/rollup.rs:114-163` (`batched_upsert_candles`) | Native-wins `ON CONFLICT ... DO UPDATE ... WHERE coin_candles.source LIKE 'rollup:%'` collision guard |
| [EXISTING] | `src/collectors/rollup.rs:87-98` (`reconcile_window`) | Pure reconcile core — **unchanged**; the source filter is applied at the SQL SELECT that feeds it, so its `previously_materialized` input is already rollup-only |
| [EXISTING] | `src/collectors/rollup.rs:171-230` (`backfill_target`) | Week-aligned bounded-window walk — reused by the backward-repair pass (REQ-CANDLE-011/012 memory discipline) |
| [NEW] | `src/collectors/rollup.rs` (proposed pure fn) | Pure watermark decision core: given `(source_min_ts, earliest_materialized_ts)`, return the bounded backward window to repair (or none) |
| [MODIFY] | `src/collectors/cycle_projection.rs:284-324` (`fit_model`), `:500-515` (`project_composite`) | Filter `close <= 0` from the series; guard `current_price <= 0` before `log10` |
| [EXISTING] | `src/collectors/cycle_projection.rs:110-113` (`pow10`) | `.ln()` here is on the literal `dec!(10)` (constant, always positive) — **not** a panic path; unchanged |

---

## Requirements (GEARS)

### Module 1 — Source-filtered reconcile & native-wins collision (F-07) [MODIFY/NEW]

- **REQ-CANDLE-050** [MODIFY] (Ubiquitous): The rollup incremental reconcile path
  (`incremental_recompute_target`) **shall** scope BOTH its `previously_materialized` read and its
  per-`ts` DELETE to rollup-sourced rows only, by carrying `AND source LIKE 'rollup:%'` — the same
  filter the `recompute_start` `MAX(ts)` query already carries.
- **REQ-CANDLE-051** [MODIFY] (Unwanted): A native provider-sourced row (any `source` NOT matching
  `rollup:%`) that falls within the recompute window `[recompute_start, now]` **shall not** be
  deleted by the reconcile and **shall not** be counted as a previously-materialized bucket. The
  materializer **shall not** destroy the source rows it derives from.
- **REQ-CANDLE-052** [NEW] (Event-driven): When an emitted rollup bucket's
  `(coin_id, vs_currency, interval, ts)` collides with an existing native-sourced row, the native
  row **shall** win — the rollup upsert **shall not** overwrite the native row's OHLCV or its
  `source` field (Decision D1). This contract **shall** be documented at an `@MX:ANCHOR`.
- **REQ-CANDLE-053** [MODIFY] (Ubiquitous): For the rollup-sourced subset of the recompute window,
  the incremental reconcile **shall** still converge to what a full rebuild would produce —
  REQ-CANDLE-005 set-parity is preserved for rollup-owned rows. The source filter narrows the
  reconcile's domain to rollup rows; it **shall not** weaken parity within that domain.

### Module 2 — Source low-watermark history repair (F-08) [MODIFY/NEW]

- **REQ-CANDLE-054** [MODIFY] (State-driven): While processing a `rollup` work item, the materializer
  **shall no longer be strictly forward-only**: in addition to the forward recompute from the
  max-materialized bucket, it **shall** compare the source interval's low-watermark (`MIN(ts)`)
  against the earliest materialized rollup bucket for the target to detect source history preceding
  existing materialization.
- **REQ-CANDLE-055** [NEW] (Event-driven): When the source low-watermark bucket precedes the earliest
  materialized rollup bucket (source rows arrived behind the materialized history — e.g. a deep
  backfill completing after the rollup already ran), the materializer **shall** run a bounded backward
  materialization pass over `[source_low_watermark_bucket, earliest_materialized_bucket)`,
  materializing the previously-unmaterialized history.
- **REQ-CANDLE-056** [NEW] (Unwanted): The backward repair pass **shall not** load the coin's full
  source series into memory at once; it **shall** reuse the existing week-aligned bounded-window
  walking discipline (REQ-CANDLE-011/012) so per-window memory stays bounded on the 256 Mi pod
  regardless of how deep the repaired history reaches.
- **REQ-CANDLE-057** [NEW] (Ubiquitous): The source low-watermark **shall** be query-derived (a
  `MIN(ts)` comparison), requiring no new migration, no new state column, and no new table
  (Decision D2).

### Module 3 — Non-positive projection guard (F-09) [MODIFY]

- **REQ-CANDLE-058** [MODIFY] (Ubiquitous): Projection series construction (`fit_model` /
  `project_composite`) **shall** filter out any daily row whose `close <= 0` before it is used in a
  `log10`-based fit, and **shall** log each skipped row at `warn!`.
- **REQ-CANDLE-059** [MODIFY] (Event-driven): When `current_price <= 0`, the projection **shall**
  degrade gracefully (skip / empty projection) and log at `warn!`; it **shall not** panic.
- **REQ-CANDLE-060** [MODIFY] (Unwanted): `rust_decimal::MathematicalOps::ln` / `log10` **shall not**
  be reachable with a non-positive argument from the projection path — there **shall** be no panic
  path on a zero or negative stored close or `current_price`. Projection constants remain
  backtest-locked and unchanged; the guard filters inputs only and **shall not** alter the fit math
  over the positive subset.

---

## Edge Cases (summarized; full behavior in acceptance.md)

- **Mixed-source window, no collision.** A window holds native `1d` rows at some `ts` and
  rollup-derived buckets at other `ts`. Native rows are neither read as materialized nor deleted;
  rollup rows reconcile to set-parity. (REQ-CANDLE-050/051/053)
- **Collision at a shared `ts`.** An emitted rollup bucket lands on a native row's exact PK. Native
  wins — the native row is byte-identical afterward. (REQ-CANDLE-052)
- **Deep-backfill-after-rollup.** Source rows are inserted behind the earliest materialized rollup
  bucket. The next `rollup` run's watermark comparison triggers a bounded backward pass; the series
  extends backward. On the following run the watermark no longer precedes the earliest bucket, so no
  repair repeats — the pass is idempotent and self-terminating. (REQ-CANDLE-054/055/056/057)
- **Zero / negative close in projection series.** The offending rows are dropped with a `warn!`; the
  fit proceeds over the positive subset without panic. (REQ-CANDLE-058/060)
- **`current_price = 0`.** Guarded before `log10`; the projection returns empty/graceful, no panic.
  (REQ-CANDLE-059)
- **No source history yet.** `MIN(ts)` is `NULL` → no watermark, no backward pass, existing
  first-run full-backfill behavior (REQ-CANDLE-010) is unchanged.

## Exclusions (What NOT to Build)

The following are explicitly **out of scope** for SPEC-CANDLE-002.

### Out of Scope — Mid-history source gap re-materialization
- Re-materializing rollup buckets in the **interior** of the already-materialized range (between the
  earliest and the max materialized bucket) is NOT in scope. F-08 repair is scoped strictly to
  source history that arrives **behind** the earliest materialized bucket (the low-watermark case).
- Interior source gap-fills that change already-materialized mid-range buckets are a separate future
  concern; this SPEC does not add mid-range rescanning.

### Out of Scope — Read-path and HTTP contract changes
- No change to `list_candles` / the candle read endpoint, the native-precedence probe, the coverage-
  aware read-time aggregation fallback, the HTTP response schema, or the `TsKey` cursor format. This
  is a materializer write-path + projection compute change only.

### Out of Scope — New migration (default off)
- No new migration, state column, or table is added. The watermark is query-derived (D2). A state
  column is admissible ONLY if querying proves too expensive, and only with an explicit plan.md
  justification — it is not the default path.

### Out of Scope — Projection constant tuning
- The backtest-locked projection constants (`tests/backtest_projection.rs`) MUST NOT be tuned,
  re-fit, or altered. F-09 adds an input guard only; the fit math over positive inputs is unchanged.

### Out of Scope — Other roadmap phases and adjacent findings
- Worker retry / backpressure (F-01..F-06) is Phase 1 (SPEC-SCHED-002, completed) — not re-addressed
  here. Batching / structural refactor of these write paths (F-51/F-52, per-provider pacing) is
  Phase 7 — not in scope; this SPEC deliberately lands before it as its regression baseline.

### Out of Scope — Forked bucketing / folding math
- `src/api/candles_agg.rs` remains the single source of truth for OHLC folding, volume
  null-propagation, alignment, and completeness. The backward-repair pass reuses it unchanged; no
  bucketing math is forked.

## @MX Annotation Targets (high fan_in / invariant contracts)

- **`@MX:ANCHOR`** on `incremental_recompute_target` — the **source-filter invariant**: the rollup
  reconcile reads and deletes ONLY `source LIKE 'rollup:%'` rows; it must never touch native
  provider rows. (`@MX:REASON` required — data-loss-prevention invariant.)
- **`@MX:ANCHOR`** on `batched_upsert_candles` — the **native-wins collision contract**: the
  `ON CONFLICT` guard must not overwrite a native row. (`@MX:REASON` required — collision-policy
  invariant.)
- **`@MX:WARN`** on the backward-repair pass — **memory-bounded**: the pass must walk week-aligned
  bounded windows and never fetch the full source series (256 Mi pod). (`@MX:REASON` required.)

Full MX placement, tag text, and update/remove policy in plan.md § MX Tag Targets.

## Open Items

**0 unresolved.** The three decisions the task flagged (collision policy, watermark mechanism, guard
placement) are settled as D1/D2/D3 above. No `[NEEDS CLARIFICATION]` markers remain; any residual
implementation micro-decisions (exact backward-window chunk count reuse, pure-core function
signature) are plan.md concerns, not requirement ambiguities.
