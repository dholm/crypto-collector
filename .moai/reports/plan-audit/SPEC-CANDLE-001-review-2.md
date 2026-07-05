# SPEC Review Report: SPEC-CANDLE-001
Iteration: 2/3
Verdict: FAIL
Overall Score: 0.80

Reasoning context ignored per M1 Context Isolation. This audit reads only the SPEC directory
(`spec.md`, `plan.md`, `acceptance.md`, `spec-compact.md`) and verifies every `file:line` code
claim against the live tree. Adversarial stance: the SPEC is assumed defective until proven
otherwise with evidence. Iteration-1 verdict was FAIL 0.60 (defects D1–D8); this pass performs a
full re-audit PLUS regression check of the four iteration-1 blocking/major defects.

## Failure modes checked before reading (M2)
REQ gaps/duplicates; informal ACs; frontmatter field/type errors; implementation-detail leakage;
broken traceability; hardcoded language tooling; vague/absent exclusions; contradictory
requirements; NEW contradictions introduced by the v0.2.0 edits; cross-document divergence
(spec/plan/acceptance/compact). Findings below.

## Must-Pass Results
- **[PASS] MP-1 REQ number consistency**: REQ-CANDLE-001..005, -010..013, -020..024, -030..032,
  -040..043 (spec.md:142-242). No duplicates; consistent 3-digit zero-padding; block gaps fall on
  ×10 module boundaries (deliberate project scheme, unchanged from iteration 1). No new gaps/dupes
  introduced by the revision.
- **[PASS] MP-2 EARS format compliance**: Every normative REQ uses correct EARS keywords —
  Ubiquitous "shall" (001/002/003/004/011/013/024/032/040/041), Event-Driven "When…shall"
  (010/020/021/030), State-Driven "While…shall" (022), Unwanted "If…then…shall" (012/031/042),
  Optional "Where…shall" (023/043), Complex State+Event (005). REQ-CANDLE-032 is now correctly
  relabeled Ubiquitous (spec.md:221) — iteration-1 D5 fixed. Given/When/Then material remains
  correctly quarantined in acceptance.md as scenarios (acceptance.md:33), not mislabeled EARS.
- **[PASS] MP-3 YAML frontmatter validity**: `id`, `version` (bumped 0.1.0→0.2.0, spec.md:3),
  `status`, `created`, `updated`, `author`, `priority`, `issue_number` all present and correctly
  typed (spec.md:1-10). Project uniform schema (verified identical across sibling SPECs in
  iteration 1); `created`/no-`labels` is the project convention, logged as observation, not a
  validity failure.
- **[N/A] MP-4 Section 22 language neutrality**: N/A — single-language (Rust) project SPEC; no
  multi-language tooling enumeration in scope.

## Category Scores (0.0-1.0, rubric-anchored)
| Dimension | Score | Rubric Band | Evidence |
|-----------|-------|-------------|----------|
| Clarity | 0.75 | 0.75 | Marker mechanism now unambiguous (post-fold relabel, REQ-CANDLE-003 spec.md:153-158). Residual ambiguity: REQ-CANDLE-001's claim that reusing `select_source_interval` "makes the materialized series match read-time output for coins with multiple stored intervals" (spec.md:147-149) is unqualified, but the materializer fixes `window_start=None` while the read path uses `window_start=params.start` (candles.rs:187) — the two diverge for bounded reads on multi-interval coins (D9). |
| Completeness | 0.90 | 0.75-1.0 | All sections present; Exclusions specific (spec.md:261-272); Edge Cases specific (spec.md:246-259). D3 gap closed — window-reconcile promoted to normative REQ-CANDLE-022 (spec.md:198-205). Minor: REQ-CANDLE-021/043 lack a dedicated observable AC (D7 carryover). |
| Testability | 0.75 | 0.75 | Parity criterion now pinned (interval/start/vs_currency/fixed `now`), scoped to OHLCV+volume-null+emitted-bucket-set over closed complete buckets, `source` excluded (Goal spec.md:62-67; Scenario 4 acceptance.md:58-67; DoD acceptance.md:114-116) — iteration-1 D4 addressed. One residual: Scenario 4 asserts "source interval chosen by `select_source_interval` for both the materializer and the read path" (acceptance.md:59-63) but does not pin `start=None`, so for a multi-interval coin the two sides can select different sources (D9), making the parity assertion not strictly binary for that class. |
| Traceability | 0.80 | 0.75 | Every AC references an existing REQ; no orphan ACs; D3's acceptance condition now has a backing normative REQ (022). REQ-CANDLE-021 (periodic backstop) and -043 (batched insert) retain only weak/indirect coverage — no dedicated scenario (D7 carryover). |

## Regression Check (iteration-1 blocking/major defects)

- **D1 (CRITICAL) — source-marker contradiction (REQ-003 `rollup:*` vs REQ-004 "unchanged" +
  `aggregated:` at candles_agg.rs:230): [RESOLVED].** REQ-CANDLE-003 now specifies the marker as a
  **post-fold relabel** of `aggregate_candles` output that "**shall** overwrite only the `source`
  field; `ts`, `interval`, and all OHLCV fields are left exactly as `aggregate_candles` produced
  them" (spec.md:153-158). REQ-CANDLE-004 restates reuse as "numeric and bucketing logic
  **unchanged**… The only post-processing… is the `source`-field relabel of REQ-CANDLE-003; no
  OHLCV / `ts` / `interval` value is recomputed" (spec.md:159-164). Scope line encodes the same
  ("the sole post-processing is relabeling the output `source` field to `rollup:*`", spec.md:101).
  Exclusion forbids "**No forked bucketing math**" (spec.md:271-272) — which the field-only relabel
  does not violate, so the Exclusion permits the relabel while still forbidding forked math, exactly
  as required. plan.md:48-53 and spec-compact.md:20-21 agree. Verified `aggregated:{label}` is
  stamped in-memory at candles_agg.rs:230 and the native-source guard at candles.rs:600-608 asserts
  native rows must NOT start with `aggregated:` → `rollup:5m` satisfies it. Not merely reworded:
  the contradiction is structurally removed by naming the relabel as the single override point.

- **D2 (CRITICAL) — hand-rolled "finest divisor" vs read path's `select_source_interval`:
  [RESOLVED at the algorithm level; NEW narrower issue D9 introduced].** REQ-CANDLE-001 now selects
  source "via the **same function the read path uses** — `select_source_interval`
  (`src/api/candles_agg.rs:96-135`… scored by coverage-miss, tie-broken to the **larger** divisor)…
  (not a hand-rolled 'finest divisor')" (spec.md:142-149). `select_source_interval` is added to the
  Scope reuse list (spec.md:99-100), to plan.md grounding fact 2f (plan.md:19) and technical
  approach step 1 (plan.md:39-44), to the Traceability table (spec.md:279), and to spec-compact.md
  (line 18). `params.start`/`now` fixing is stated (`window_start = None`, injected `now`). Selector
  signature verified accurate against candles_agg.rs:96-135. Core defect resolved. **However** the
  chosen `window_start=None` differs from the read path's `window_start=params.start` (candles.rs:187)
  — see D9; this is a new consequence, not a failure to resolve D2.

- **D3 (MAJOR) — forming-bucket-closes-incomplete reconcile left non-normative vs REQ-005
  set-parity guarantee: [RESOLVED].** REQ-CANDLE-022 is now normative and binding: "**shall
  reconcile that bounded window**: it **shall** re-upsert every bucket `aggregate_candles` emits…
  AND **shall** delete any previously-materialized bucket in `[recompute_start, now]` that
  `aggregate_candles` no longer emits… This reconcile is what makes REQ-CANDLE-005's set-parity hold
  across runs" (spec.md:198-205). REQ-CANDLE-005 cross-references it ("cross-run set-parity enforced
  by REQ-CANDLE-022", spec.md:170-171). plan.md:100-105 states it "is no longer a deferred
  'recommended' mitigation but a binding requirement." acceptance.md:87-90 (edge case) and Scenario 5
  (acceptance.md:69-73) reference REQ-CANDLE-022 — the acceptance condition now has a backing
  normative REQ. spec-compact.md:22,33 agree. Not merely reworded — the delete step is now a `shall`.

- **D4 (MAJOR) — "byte-for-byte parity" untestable/unpinned: [RESOLVED].** Goal restated as
  "OHLCV-identical… for the same closed, complete buckets at a fixed injected `now` — identical
  `ts`, `interval`, open/high/low/close, and volume (including `NULL` propagation), and identical
  emitted-bucket set… Only the `source` column differs by design" (spec.md:62-67). Scenario 4 pins
  "(interval, `start`, `vs_currency`, same frozen `now`), restricted to **closed, complete**
  buckets… The `source` column is **excluded**" (acceptance.md:58-67). DoD mirrors it
  (acceptance.md:114-116). spec-compact.md:65 agrees. Core testability defect resolved; one residual
  precision gap remains (D9/D10).

**Minors:** REQ-CANDLE-032 relabeled Ubiquitous (spec.md:221; compact line 40) — D5 resolved.
REQ-CANDLE-042 enumerates the full widened kind set "`spot, candles, metadata, market, derivatives,
cycle_overlay, rollup`" (spec.md:237-238; compact line 45) — matches migration 0014's current enum
(`spot,candles,metadata,market,derivatives,cycle_overlay`) plus `rollup`. Resolved.

## Defects Found

**D9. spec.md:147-149 (REQ-CANDLE-001) & acceptance.md:59-63 (Scenario 4) vs candles.rs:187 —
MAJOR (new; parity guarantee over-claimed + not tightly testable for multi-interval coins).**
REQ-CANDLE-001 fixes the materializer's source selection to `select_source_interval(…, window_start
= None, now)` (spec.md:147). The read path invokes the **same** selector with the request's `start`
as the window bound: `select_source_interval(&coverage, target_secs, params.start, now)`
(candles.rs:187). `window_start` changes the coverage `floor` (candles_agg.rs:112-121) and thus the
`deep_miss` score (candles_agg.rs:130): with `None` the floor is the deepest `earliest` (favoring
the deepest interval, e.g. `5m`); with a recent `Some(start)` every divisor scores `deep_miss=0` and
the tie-break selects the **larger** divisor (e.g. `4h`, candles_agg.rs:132). For a coin holding
multiple divisor intervals, a bounded read therefore selects a **different, coarser** source than
the materializer's `window_start=None` selection → different OHLC (edge extremes/open/close differ by
source granularity). Multi-interval coins are real, not hypothetical: candles.rs:661 tests
"dogecoin stores 30m… target 1h → source 30m", CoinGecko writes `4h`
(coingecko.rs:1294-1296) and Binance writes `5m`/`1h`/`4h`/`1d` — a coin can hold both a deep `5m`
Binance backfill and a `4h` CoinGecko series. Consequently REQ-CANDLE-001's assertion that reusing
the selector "makes the materialized series match read-time output **for coins with multiple stored
intervals**" (spec.md:147-149) is true only for **unbounded** (full-history, `start=None`) reads.
Scenario 4 pins a `start` and asserts "the source interval chosen by `select_source_interval` for
**both** the materializer and the read path" (acceptance.md:59-63) — an assumption that is not
guaranteed for a multi-interval coin unless `start` is pinned to `None` (or the read-side selection
in the test is invoked with `window_start=None`). As written, the parity test could pick opposite
sources on each side and spuriously fail, or mask a real divergence. Runtime impact is mitigated (once
materialized, the coin-scoped `EXISTS` probe serves the single canonical series natively and never
re-selects — candles.rs:106-116/118-148), so there is no per-request runtime bug; the defect is that
the SPEC's central parity guarantee is over-claimed and its acceptance test is not pinned tightly
enough for the multi-interval case that `select_source_interval` exists to handle. This is the same
D2/D4 territory the retry loop targeted, surfaced one level deeper.

**D10. spec.md:62-68 (Goal) — MINOR (new; imprecise correctness claim).** The Goal states the
series is "OHLCV-identical to what the current read-time aggregation produces" and "Correctness is
thereby preserved." For a bounded request on a multi-interval coin, the materialized (deepest-source,
`window_start=None`) rows now served natively can differ from the *prior* read-time bounded output
(which would have used `window_start=Some(start)` → a coarser source). Materializing from the
deepest source is arguably an improvement, but the Goal presents strict equivalence to prior
read-time output without the "for unbounded/full-history reads" qualifier. Tighten the claim or
acknowledge the intentional deviation.

**D7 (carryover). acceptance.md — MINOR (traceability, partially resolved).** REQ-CANDLE-021
(periodic-tick backstop enqueue) and REQ-CANDLE-043 (batched insert path) still have only
weak/indirect acceptance coverage — no dedicated Given/When/Then scenario asserting them
specifically (021 is folded into Scenario 5's incremental flow; 043 appears only in DoD's `@MX`
line acceptance.md:120 and plan.md). Iteration-1 D6 (compound REQs) is now honestly labeled
("Complex" for 005, and 022 packs recompute+reconcile+delete) — acceptable, not re-raised.

## Chain-of-Verification Pass
Second-look, re-reading each section end-to-end:
- **Re-read every REQ-CANDLE entry** (spec.md:142-242), not a sample. The D1 relabel wording is now
  internally consistent between 003 and 004; the Exclusion (spec.md:271-272) forbids forked
  bucketing math but does not forbid a `source`-field relabel — no residual D1 contradiction.
- **Re-checked REQ sequencing** end-to-end: 5/4/5/3/4 per block, no dupes, consistent padding —
  MP-1 holds; the revision added no new REQ numbers.
- **Re-verified traceability for every REQ** (reverse map): all AC-referenced REQs exist; only 021
  and 043 remain weakly covered (D7).
- **Re-read Exclusions** (spec.md:261-272) and Edge Cases (spec.md:246-259): specific, each anchored
  to a file/behavior; the forming-then-incomplete edge now points at REQ-CANDLE-022's delete.
- **NEW-contradiction sweep across the v0.2.0 edits:** the marker relabel (003/004/Scope/Exclusion)
  is consistent; the reconcile (022) is consistent with 005; the parity restatement (Goal/Scenario
  4/DoD) is consistent within itself. The one genuine new inconsistency is D9 — REQ-CANDLE-001's
  `window_start=None` vs the read path's `window_start=params.start` (candles.rs:187), which the
  edits did not reconcile and which Scenario 4's "same source for both" assumption papers over.
- **Cross-document agreement (spec / plan / acceptance / compact):** the four docs agree on the
  marker relabel, the `select_source_interval` reuse, the normative reconcile, and the pinned parity
  scope. No divergence found other than the shared D9 gap (compact line 18 repeats the unqualified
  multi-interval claim).
- **Code-claim audit (all citations verified against the live tree):** `select_source_interval`
  candles_agg.rs:96-135 (coverage-scored, `Reverse(*secs)` larger-divisor tie-break) ✔;
  `bucket_start` :153-158 (epoch-Thursday `1w`) ✔; `fold_volume` :175-181 (null-propagation) ✔;
  `aggregate_candles` :206-291 with `aggregated:` at :230 ✔; read path passes `params.start` to the
  selector at candles.rs:187 ✔ (basis of D9); coin-scoped `EXISTS` probe :106-116 ✔; native branch
  :118-148 ✔; aggregation fallback :150-292 ✔; native-source guard :600-608 ✔; `enqueue_queue_item`
  collection_queue.rs:211-224 ✔; `("coin","cycle_overlay")` arm :576-585 (network-free precedent) ✔;
  periodic refresh main.rs:426-490 (`REFRESH_KINDS`:430, active query:437, cycle enqueue:476-489) ✔;
  `ensure_candle_partition` partitions.rs:39-75 ✔; `TsKey` cursor cursor.rs:39-43 ✔; `CoinCandle`
  quote.rs:33-46 (`volume: Option<Decimal>`) ✔; `upsert_coin_candle` upserts.rs:105-153 ✔; migration
  0014 current enum `spot,candles,metadata,market,derivatives,cycle_overlay` ✔ (REQ-CANDLE-042's
  target set = that + `rollup`). **All `file:line` claims in the SPEC are accurate.**

Regression summary: iteration-1 defects D1, D2, D3, D4, D5, D6, D8 are RESOLVED; D7 partially
carried (021/043 weak coverage). No prior defect appears unchanged across both iterations (no
stagnation/blocking defect). The FAIL is driven solely by the newly-surfaced MAJOR D9 (plus MINOR
D10/D7), all in the parity-guarantee family — not by any must-pass breach and not by any unresolved
iteration-1 defect.

## Recommendation (actionable, for manager-spec)
1. **Resolve D9 (blocking this iteration).** Reconcile the `window_start` asymmetry. Either:
   (a) Qualify REQ-CANDLE-001's multi-interval parity claim (spec.md:147-149) to **full-history /
   unbounded reads** — state that the materializer builds the canonical series with `window_start =
   None`, and that once materialized every read (bounded or not) is served natively from that single
   series (candles.rs:118-148), so the "match read-time output" guarantee is defined against the
   **unbounded** read-time aggregation; OR
   (b) State that the materializer selects with the same `window_start` semantics the read path would
   use, and pin how that is fixed. Option (a) matches the actual runtime and is the smaller edit.
2. **Pin Scenario 4 (D9).** In acceptance.md:59-63, require the parity comparison to use `start =
   None` (or explicitly invoke the read-side `select_source_interval` with `window_start = None`), so
   both sides provably select the same source for multi-interval coins. Otherwise the "source
   interval chosen by `select_source_interval` for both" assumption is not testable as PASS/FAIL.
3. **Tighten D10.** In the Goal (spec.md:62-68), add the "for unbounded/full-history reads"
   qualifier to "OHLCV-identical…/correctness preserved", or note that bounded reads on
   multi-interval coins are intentionally served from the deepest source post-materialization.
4. **D7 (minor).** Add a dedicated acceptance criterion for REQ-CANDLE-021 (periodic-tick backstop
   actually enqueues a `rollup` item per active coin) and REQ-CANDLE-043 (batched insert preserves
   the `(coin_id,vs_currency,interval,ts)` conflict target + partition-ensure), so each has one
   observable AC.

Verdict: FAIL
