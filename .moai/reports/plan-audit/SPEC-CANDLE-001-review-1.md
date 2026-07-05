# SPEC Review Report: SPEC-CANDLE-001
Iteration: 1/3
Verdict: FAIL
Overall Score: 0.60

Reasoning context ignored per M1 Context Isolation. This audit reads only the SPEC
directory (`spec.md`, `plan.md`, `acceptance.md`, `spec-compact.md`) and verifies every
`file:line` code claim against the live tree. Adversarial stance: the SPEC is assumed
defective until proven otherwise with evidence.

## Failure modes checked before reading (M2)
REQ gaps/duplicates; informal ACs; frontmatter field/type errors; implementation detail
leakage; broken traceability; hardcoded language tooling; vague/absent exclusions;
contradictory requirements. Findings below.

## Must-Pass Results
- **[PASS] MP-1 REQ number consistency**: REQ-CANDLE-001..005, -010..013, -020..024,
  -030..032, -040..043. No duplicates; consistent 3-digit zero-padding. The block gaps
  (005→010, 013→020, …) fall exactly on ×10 module boundaries — a deliberate,
  project-standard block-numbering scheme (mirrors REQ-API-2NN in SPEC-API-003). Not
  accidental omission. spec.md:129-213.
- **[PASS] MP-2 EARS format compliance**: The normative requirements (spec.md:129-213) use
  EARS keywords correctly — Ubiquitous ("shall"), Event-Driven ("When…shall"), State-Driven
  ("While…shall"), Unwanted ("If…then…shall"), Optional ("Where…shall"). The Given/When/Then
  material lives in `acceptance.md` and is *correctly labelled* as scenarios (acceptance.md:33),
  not mislabelled EARS. Minor mislabels noted in defects (D5/D6), not a firewall breach.
- **[PASS] MP-3 YAML frontmatter validity**: `id`, `version`, `status`, `created`, `updated`,
  `author`, `priority`, `issue_number` all present and correctly typed (spec.md:1-10). This
  matches the project's uniform schema verified across five sibling SPECs (SPEC-CYCLE-001,
  SPEC-API-002, SPEC-API-003, SPEC-DB-001, SPEC-SCHED-001 — all use `created`, none use
  `labels`). The generic-rubric fields `created_at`/`labels` are not this project's schema;
  PASS on project convention, with the naming deviation logged as observation D8.
- **[N/A] MP-4 Section 22 language neutrality**: N/A — single-language (Rust) project SPEC.
  No multi-language tooling enumeration is in scope.

## Category Scores (0.0-1.0, rubric-anchored)
| Dimension | Score | Rubric Band | Evidence |
|-----------|-------|-------------|----------|
| Clarity | 0.65 | 0.50-0.75 | Source-interval selection is contradictory/ambiguous: REQ-CANDLE-001 "finest … that evenly divides" (spec.md:129-133) + plan.md:39-40 "finest (smallest seconds) with the deepest coverage" vs the actual read path `select_source_interval` which minimizes coverage-miss then tie-breaks to the *larger* divisor (candles_agg.rs:96-135). "Finest" and "deepest coverage" can point at different intervals. |
| Completeness | 0.70 | 0.50-0.75 | All sections present incl. specific Exclusions (spec.md:232-243) and Edge Cases (spec.md:217-230). But the forming-bucket-closes-incomplete resolution is left NON-normative ("recommended", plan.md:93-99) while REQ-CANDLE-005 *guarantees* set-parity (spec.md:148) — a gap between guarantee and binding requirement (D3). |
| Testability | 0.50 | 0.50 | "byte-for-byte parity with read-time output" (Goal spec.md:52-56; Scenario 4 acceptance.md:58-63) is not binary-testable: read-time output is not invariant — `select_source_interval` depends on `params.start` and `now` (candles_agg.rs:96-101,129-133), so parity target is unpinned (D4). Several ACs are indirect (D7). |
| Traceability | 0.75 | 0.75 | Every AC-referenced REQ exists; no orphan ACs. But REQ-CANDLE-011/021/040/043 have only weak/indirect coverage — no dedicated scenario or repro test (D7). |

## Defects Found

**D1. spec.md:141-143 & spec.md:88 vs candles_agg.rs:230 — CRITICAL.**
`aggregate_candles` hard-codes its output marker: `let agg_source = format!("aggregated:{source_interval_label}")` (candles_agg.rs:230), written to every row's `source` (candles_agg.rs:284). REQ-CANDLE-004 (spec.md:143) and Scope (spec.md:88) require reusing `aggregate_candles` **byte-for-byte / unchanged**, while REQ-CANDLE-003 (spec.md:137-139) requires each materialized row's `source` to be `rollup:<source_interval>`. There is **no parameter** on `aggregate_candles` that yields a `rollup:` prefix — passing `source_interval_label="rollup:5m"` produces the nonsense `source="aggregated:rollup:5m"`. plan.md:44-46 hand-waves this as "with `source = 'rollup:<source_interval>'` semantics", but that semantic does not exist in the reused function. Producing the required marker forces EITHER modifying `aggregate_candles` (violating "unchanged" / the Exclusion "No forked bucketing math", spec.md:242-243) OR an unspecified post-fold rewrite of the `source` field (undocumented; must be proven not to touch OHLCV). This is an internal contradiction between REQ-CANDLE-003 and REQ-CANDLE-004 that blocks the stated implementation path. Note the characterization guard at candles.rs:600-608 asserts native rows must NOT start with `aggregated:`, so the marker collision is load-bearing, not cosmetic.

**D2. spec.md:129-133 & plan.md:38-40 vs candles_agg.rs:96-135 — CRITICAL.**
The rollup selects its source as the "finest available … interval that evenly divides the target". The read path it must match selects via `select_source_interval`, which minimizes `(deep_miss + stale_miss)` and tie-breaks to the **larger** divisor (`std::cmp::Reverse(*secs)`, candles_agg.rs:129-133). These are different algorithms: on a coverage tie they pick opposite ends (finest vs largest), and when coverage differs the read path may pick a coarser-but-deeper interval that "finest" would reject. Because different source intervals yield different gap-dropping (N = target/source differs), the materialized bucket set can diverge from read-time output — directly falsifying the byte-for-byte parity guarantee (Goal spec.md:52-56; REQ-CANDLE-004/005; Scenario 4 acceptance.md:58-63; DoD acceptance.md:109). Critically, `select_source_interval` is **omitted** from the reuse list (spec.md:88, plan.md:14-18) even though it is the single decision that determines what feeds `aggregate_candles`. For BTC (only deep `5m`) the two agree, so the headline scenario passes — but the SPEC asserts parity as a *general guarantee across every tracked coin* (spec.md:85-86,129), which is not achievable as specified.

**D3. spec.md:144-148 (REQ-CANDLE-005) vs spec.md:175-178 (REQ-CANDLE-022) — MAJOR.**
REQ-CANDLE-005 states the completeness policy "guaranteeing set-parity" with read-time aggregation. REQ-CANDLE-022 mandates a forward-only, **upsert-only** recompute from `max-materialized bucket_start`. The SPEC itself documents (Edge Cases spec.md:219-222; plan.md:93-99) that a forming partial bucket which later closes *incomplete* will linger under upsert-only recompute — `aggregate_candles` would drop it, the stored partial is not deleted → divergence. The reconcile-delete mitigation is explicitly only "recommended for Run phase" / "Surface to the user if reconcile is deemed out of scope" (plan.md:97-99) — i.e. NOT a binding requirement. Yet acceptance.md:83-85 asserts as an acceptance condition that "after a full rollup pass, the materialized set for the recompute window equals `aggregate_candles` output". An acceptance criterion demands behavior that no normative REQ guarantees, and REQ-CANDLE-022 as written actively contradicts REQ-CANDLE-005's guarantee. Must be resolved by promoting window-reconcile into REQ-CANDLE-022 (or downgrading the -005 "guarantee").

**D4. spec.md:52-56 & acceptance.md:58-63 — MAJOR (testability).**
"byte-for-byte-equal transform of what the current read-time aggregation produces" is not binary-testable because read-time output is not a fixed target: `select_source_interval` is parameterized on `params.start` and `now` (candles_agg.rs:96-101,129-133), and the forming-bucket classifier is `now`-relative (candles_agg.rs:246-252). Parity "against read-time output" must pin the request parameters (interval, start, vs_currency, and a frozen `now`) to be a PASS/FAIL check. As written a tester cannot determine parity unambiguously. (Also: the `source` field is *intentionally* different — `rollup:` vs `aggregated:` per REQ-CANDLE-003 — so "byte-for-byte" is literally false for that column; the claim needs to be scoped to OHLCV + volume + bucket-set.)

**D5. spec.md:194-196 (REQ-CANDLE-032) — MINOR.**
Labelled "(Unwanted)" but contains no `If … then` trigger; it is a Ubiquitous "shall not alter …" constraint. EARS pattern label is inaccurate.

**D6. spec.md:144-148, 155-156, 191-193, 206-209 — MINOR.**
Compound requirements pack multiple behaviors into one REQ, reducing atomicity/testability: REQ-CANDLE-005 (forming-emit AND closed-drop), REQ-CANDLE-011 (window-bound AND load-only), REQ-CANDLE-031 (fallback AND "shall not be removed by this SPEC" — a meta-scope guard, not runtime behavior), REQ-CANDLE-042 (conflates a runtime-failure observation with the migration requirement).

**D7. acceptance.md (scenarios/DoD) — MINOR (traceability).**
REQ-CANDLE-011 (week-aligned window bound), -021 (periodic-tick backstop enqueue), -040 (idempotent upsert), -043 (batched insert path) have only weak/indirect coverage — no dedicated Given/When/Then scenario or reproduction test asserting them specifically.

**D8. spec.md:5 — OBSERVATION (not scored as a defect).**
Frontmatter uses `created` (not the generic-rubric `created_at`) and omits `labels`. This is the project's uniform convention (verified identical across 5 sibling SPECs), so it is NOT a validity failure — logged only to document the deviation from the generic MP-3 field names.

## Chain-of-Verification Pass
Second-look, re-reading each section:
- **Re-read every REQ-CANDLE entry end-to-end** (spec.md:129-213), not a sample. The
  aggregate_candles marker contradiction (D1) was found on the second pass by reading
  candles_agg.rs:230 against REQ-CANDLE-003/004 — the first pass had accepted "reuse
  unchanged" at face value. This is the most serious finding; promoted to CRITICAL.
- **Re-checked REQ sequencing** end-to-end: block scheme confirmed deliberate (×10
  boundaries), no dupes, consistent padding — MP-1 holds.
- **Re-verified traceability for every REQ** (not a sample): all AC-referenced REQs exist;
  reverse map surfaced the weak-coverage set in D7.
- **Re-read Exclusions** (spec.md:232-243): specific and non-vague (6 concrete entries,
  each with a file/behavior anchor) — no defect there.
- **Cross-requirement contradiction sweep**: found D1 (003 vs 004+Exclusion), D2 (001 vs
  parity claim vs select_source_interval), D3 (005 vs 022). All three are genuine internal
  contradictions, not single-REQ ambiguity.
- **Code-claim audit (all citations verified against live tree):** candles.rs:106-116 probe
  ✔, 118-148 native branch ✔, 150-292 aggregation ✔, 159-167 coverage query ✔, 600-608
  native-source assertion ✔; candles_agg.rs:28-48 interval_to_seconds ✔, 153-158
  bucket_start Thursday-anchored ✔, 175-181 fold_volume ✔, 206-291 aggregate_candles ✔,
  230 hard-coded `aggregated:` prefix ✔ (basis of D1); collection_queue.rs:211-224
  enqueue_queue_item ✔, 330 `("coin","candles")` arm ✔, 576-585 `("coin","cycle_overlay")`
  arm ✔; main.rs:426-490 periodic refresh (REFRESH_KINDS:430, active query:437, cycle
  enqueue:476-489) ✔; partitions.rs:39-75 dynamic monthly partition ✔; cycle_overlay.rs
  ~488-513 OOM `@MX:WARN` ✔, ~467-474 interval-coverage query ✔; migrations/0014 DROP/ADD
  template ✔ (current enum: spot,candles,metadata,market,derivatives,cycle_overlay). **All
  `file:line` claims in the SPEC are accurate.**
- **collection_queue kind CHECK-constraint migration requirement**: PRESENT and correct —
  REQ-CANDLE-042 (spec.md:206-209), Scope (spec.md:93), plan.md:55-56, DoD (acceptance.md:102).
  Note for Run phase: the new migration must re-list ALL existing kinds PLUS `rollup`
  (spot,candles,metadata,market,derivatives,cycle_overlay,rollup), matching the 0014
  full-restate pattern — the SPEC references the template but does not enumerate the full
  target set; recommend making that explicit.
- **Divisor edge** ("finest interval does not evenly divide 86400/604800"): handled
  (spec.md:224-225, acceptance.md:86). Since 86400 | 604800, any 1d divisor also divides
  1w; per-target selection (REQ-CANDLE-001) covers the asymmetric case. Adequate.
- **Volume-null propagation** through the batched insert (REQ-CANDLE-043): relies on
  `CoinCandle.volume: Option<Decimal>` reuse; acceptable but D7-weak on explicit assertion.

## Recommendation (actionable, for manager-spec)
1. **Resolve D1 (blocking).** Decide the marker mechanism explicitly: either (a) parameterize
   the source prefix in `aggregate_candles` and update REQ-CANDLE-004 to say "reused with a
   single added `source_prefix` parameter" (dropping the "unchanged" wording), OR (b) specify
   a post-fold `source`-field rewrite step in REQ-CANDLE-003 that provably leaves OHLCV
   untouched. Update the Exclusion at spec.md:242-243 to permit whichever is chosen.
2. **Resolve D2 (blocking).** Make the rollup reuse `select_source_interval` (candles_agg.rs:96)
   for source selection, or explicitly scope the parity guarantee to coins with a single deep
   divisor and downgrade the general claim. Add `select_source_interval` to the reuse list
   (spec.md:88). State how `params.start`/`now` are fixed for the materializer's selection.
3. **Resolve D3.** Promote the window-reconcile (delete buckets `aggregate_candles` no longer
   emits) into REQ-CANDLE-022 as normative, so REQ-CANDLE-005's set-parity guarantee is
   actually met; otherwise weaken -005.
4. **Fix D4.** Restate the parity criterion (Goal spec.md:52-56, Scenario 4) as "OHLCV +
   volume-null + emitted-bucket-set equal, for a pinned (interval, start, vs_currency, now)",
   and exclude the intentionally-different `source` column.
5. **Minor:** relabel REQ-CANDLE-032 as Ubiquitous (D5); split the compound REQs or note they
   are multi-clause (D6); add dedicated ACs for REQ-CANDLE-011/021/040/043 (D7); enumerate the
   full widened kind list in REQ-CANDLE-042.

Verdict: FAIL
