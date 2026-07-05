# SPEC Review Report: SPEC-CANDLE-001
Iteration: 3/3 (final)
Verdict: FAIL
Overall Score: 0.72

Reasoning context ignored per M1 Context Isolation. This audit reads only the SPEC directory
(`spec.md`, `plan.md`, `acceptance.md`, `spec-compact.md`) and re-verifies every `file:line` code
claim against the live tree under `/home/dholm/Projects/crypto-collector`. Adversarial stance: the
SPEC is assumed defective until proven otherwise. Prior verdicts FAIL 0.60 (iter 1) and FAIL 0.80
(iter 2). This pass performs a full re-audit PLUS regression check of D1/D2/D3/D4/D9/D10/D7, and a
fresh, from-scratch contradiction sweep that does NOT assume prior PASS items still pass.

## Failure modes checked before reading (M2)
REQ gaps/duplicates; informal ACs; frontmatter field/type errors; implementation-detail leakage;
broken traceability; hardcoded language tooling; vague/absent exclusions; contradictory
requirements; NEW contradictions introduced by the v0.2.1 edits; cross-document divergence
(spec/plan/acceptance/compact); incomplete resolution of prior defects (fix applied to one section
but not its mirrors). Findings below — one incomplete-resolution contradiction found.

## Must-Pass Results

- **[PASS] MP-1 REQ number consistency**: REQ-CANDLE-001..005, -010..013, -020..024, -030..032,
  -040..043 (spec.md:151-258). No duplicates; uniform 3-digit zero-padding; block gaps fall on ×10
  module boundaries (project scheme, unchanged). The v0.2.1 revision added no new REQ numbers — the
  edits were prose-scoping only. Sequencing re-checked end-to-end (5/4/5/3/4 per block).

- **[PASS] MP-2 EARS format compliance**: Every normative REQ carries a correct EARS keyword —
  Ubiquitous "shall" (001/002/003/004/011/013/024/032/040/041), Event-Driven "When…shall"
  (010/020/021/030), State-Driven "While…shall" (022), Unwanted "If…then…shall" (012/031/042),
  Optional "Where…shall" (023/043), Complex State+Event (005, honestly labeled). REQ-CANDLE-001
  (spec.md:151-165) is verbose with embedded explanatory prose about `window_start`, but its
  normative core is a single Ubiquitous "The rollup materializer **shall** produce…" — the trailing
  prose is descriptive (present-tense "serves"), not a second unguarded `shall`. Given/When/Then
  material stays quarantined in acceptance.md as scenarios, not mislabeled EARS.

- **[PASS] MP-3 YAML frontmatter validity**: `id`, `version` (0.2.1, spec.md:3), `status` (draft),
  `created`, `updated`, `author`, `priority` (high), `issue_number` all present and correctly typed
  (spec.md:1-10). Project uniform schema uses `created`/no-`labels` (verified as convention across
  sibling SPECs in iterations 1–2); logged as observation, not a validity failure. **Version is
  0.2.1 as required.**

- **[N/A] MP-4 Section 22 language neutrality**: N/A — single-language (Rust) SPEC; no multi-language
  tooling enumeration in scope.

## Category Scores (0.0-1.0, rubric-anchored)
| Dimension | Score | Rubric Band | Evidence |
|-----------|-------|-------------|----------|
| Clarity | 0.60 | 0.50 | D9/D10 parity scoping now unambiguous and consistent (spec.md:71,156-165; acceptance.md:58-70). BUT the core source-selection algorithm is described two contradictory ways: Scope (spec.md:107-108) says materialize "from the **finest** stored fixed-duration interval that evenly divides the target bucket", while authoritative REQ-CANDLE-001 (spec.md:151-165) says `select_source_interval` — coverage-scored, tie-broken to the **larger** divisor. A reasonable engineer reading Scope would implement the opposite selection to REQ-001 for multi-interval coins (D11). |
| Completeness | 0.90 | 0.75-1.0 | All sections present; Exclusions specific (spec.md:277-288); Edge Cases specific (spec.md:262-275). D7 gap closed — REQ-CANDLE-021 and -043 now have dedicated DoD lines (acceptance.md:117,119-121). |
| Testability | 0.75 | 0.75 | Parity criterion pinned and correctly scoped to **unbounded** reads over closed complete buckets, `source` excluded (Goal spec.md:71-77; Scenario 4 acceptance.md:58-70; DoD acceptance.md:122-124) — D9/D10 resolved. Residual: Scenario 1 (acceptance.md:39) asserts `source` is `rollup:5m` "(or the chosen **finest** divisor)", so a multi-interval test cannot derive the expected source unambiguously from the SPEC (D12). |
| Traceability | 0.90 | 0.75-1.0 | Every REQ maps to at least one AC; no orphan ACs; every AC references an existing REQ. 021 and 043 now have dedicated DoD lines (D7 closed). Reverse map verified for all 22 REQs. |

## Regression Check (prior-iteration defects)

- **D1 (CRITICAL, iter-1) — source-marker contradiction: [RESOLVED, still holds].** REQ-CANDLE-003
  (spec.md:169-174) keeps the marker as a **post-fold relabel** that "**shall** overwrite only the
  `source` field; `ts`, `interval`, and all OHLCV fields are left exactly as `aggregate_candles`
  produced them"; REQ-CANDLE-004 (spec.md:175-180) restates reuse "unchanged". Exclusion
  "No forked bucketing math" (spec.md:287-288) does not forbid the field-only relabel. Verified live:
  `format!("aggregated:{source_interval_label}")` stamped in-memory in `aggregate_candles`
  (candles_agg.rs:227); native-source guard asserts native rows must NOT start with `aggregated:`
  (candles.rs:604-607) → `rollup:5m` satisfies it. No reintroduction.

- **D2 (CRITICAL, iter-1) — hand-rolled "finest divisor" vs read path's `select_source_interval`:
  [PARTIALLY RESOLVED — contradiction reintroduced/left standing in Scope; see D11].**
  REQ-CANDLE-001 (spec.md:151-165) correctly uses `select_source_interval` (coverage-scored,
  larger-divisor tie-break, `window_start=None`), and the reuse list (spec.md:108-110), plan.md:19,39-44,
  and spec-compact.md:18 all agree. Selector signature verified accurate against candles_agg.rs:96-135
  (floor logic candles_agg.rs:112-121; `std::cmp::Reverse(*secs)` larger-divisor tie-break :132).
  **However** the **Scope** section (spec.md:107-108) and **Scenario 1** (acceptance.md:39) still
  describe source selection as the "**finest** … interval that evenly divides the target" — the exact
  hand-rolled model D2 was raised to eliminate, and the direct opposite of the selector's larger-divisor
  tie-break. The D2 fix was applied to REQ-001 and the reuse list but NOT propagated to Scope/Scenario 1.
  D2 is therefore **not fully resolved**. This is the primary FAIL driver (D11).

- **D3 (MAJOR, iter-1) — forming-bucket reconcile non-normative: [RESOLVED, still holds].**
  REQ-CANDLE-022 (spec.md:214-221) remains normative: "**shall reconcile that bounded window**… AND
  **shall delete** any previously-materialized bucket in `[recompute_start, now]` that
  `aggregate_candles` no longer emits". REQ-CANDLE-005 cross-references it (spec.md:186-187).
  acceptance.md:90-93 and Scenario 5 (acceptance.md:72-76) reference 022. Consistent across plan.md:100-105
  and spec-compact.md:22,33. No reintroduction.

- **D4 (MAJOR, iter-1) — untestable "byte-for-byte parity": [RESOLVED, still holds].** Goal restated
  as OHLCV-identical over closed complete buckets at a fixed injected `now`, `source` excluded
  (spec.md:71-77); Scenario 4 pins (interval, `start` omitted, `vs_currency`, frozen `now`)
  (acceptance.md:58-70); DoD mirrors it (acceptance.md:122-124). "byte-for-byte" survives only in the
  Decision rationale (spec.md:130) and plan.md:82 as a design descriptor, not as the testable
  criterion — acceptable.

- **D9 (MAJOR, iter-2) — `window_start=None` (materializer) vs `window_start=params.start` (read
  path, candles.rs:187): [RESOLVED].** The parity claim is now consistently scoped to **unbounded**
  reads and the bounded-read coarser-source case is framed as a pre-materialization optimization
  superseded once native rows serve one canonical `rollup:*` series. Verified consistent across all
  four documents: intro spec.md:14-20; HISTORY spec.md:39-45; REQ-CANDLE-001 spec.md:156-165
  ("Note the read path selects per request with `window_start = params.start` (`candles.rs:187`)…
  once native rows exist, the native path serves the single canonical `rollup:*` series for **every**
  request… intended consistency improvement, not a divergence"); Scenario 4 acceptance.md:58-70
  ("Because the read-time comparison uses an **unbounded** read (`start` omitted → `window_start =
  None`)…"); spec-compact.md:18,65. Live-tree confirmation: read path calls
  `select_source_interval(&coverage, target_secs, params.start, now)` at candles.rs:187 (exact); with
  `window_start=None` the floor becomes the deepest `earliest`, giving the deepest interval a zero
  deep-miss (candles_agg.rs:112-121). No new contradiction introduced by the scoping edit.

- **D10 (MINOR, iter-2) — Goal over-claimed strict equivalence: [RESOLVED].** Goal now reads
  "OHLCV-identical to what the current read-time aggregation produces for an **unbounded** read"
  (spec.md:71). Qualifier present.

- **D7 (MINOR, iter-2) — REQ-CANDLE-021/043 weak coverage: [RESOLVED].** Dedicated DoD lines added:
  "Periodic refresh tick enqueues a `rollup` item per active coin as a backstop (REQ-CANDLE-021),
  dedup-absorbed…" (acceptance.md:117-118) and "Batched historical insert path (if introduced,
  REQ-CANDLE-043) preserves the `(coin_id, vs_currency, interval, ts)` conflict target and calls
  `ensure_candle_partition` per distinct month — verified idempotent…" (acceptance.md:119-121). Each
  now has one observable AC.

**Stagnation check:** No defect appears unchanged across all three iterations. D11 is a *newly
surfaced* facet of the D2 family (the Scope-section mirror that the D2 edits never touched), not an
unchanged carryover — no blocking/stagnation flag. But it does mean D2's iteration-2 RESOLVED status
was granted too generously: the audit sampled REQ-001 and the reuse list without cross-checking the
Scope bullet.

## Defects Found

**D11. spec.md:107-108 (Scope) contradicts REQ-CANDLE-001 (spec.md:151-165) and the D2 resolution —
Severity: MAJOR.** The Scope "In scope" bullet states the feature materializes rows "from the
**finest** stored fixed-duration interval that evenly divides the target bucket." REQ-CANDLE-001 —
labeled authoritative and the target of the D2 retry loop — selects the source via
`select_source_interval`, which is **coverage-scored** and **tie-broken to the LARGER divisor**
(`(deep_miss + stale_miss, std::cmp::Reverse(*secs))`, verified live at candles_agg.rs:130-132). For a
coin holding multiple full-coverage divisors (e.g. `5m` and `1h` both dividing `1d`), "finest" selects
`5m` while `select_source_interval` selects `1h` — the two descriptions specify **opposite** sources.
This is the precise hand-rolled model D2 was raised to eliminate ("not a hand-rolled 'finest divisor'",
spec.md:49,158), left standing in the Scope section because the v0.2.0/v0.2.1 edits patched REQ-001
and the reuse list but not their Scope mirror. An implementer reading Scope first would build the
rejected algorithm. For BTC (single `5m` divisor) the two coincide, which is why it evaded two prior
audits — but the multi-interval case is exactly what `select_source_interval` exists to handle
(candles.rs:661 tests a multi-interval coin; CoinGecko writes `4h`, Binance writes `5m`/`1h`/`4h`).
Fix: replace spec.md:107-108 with the `select_source_interval` characterization (source that feeds an
unbounded read, `window_start=None`), matching REQ-CANDLE-001 and the intro (spec.md:14-18).

**D12. acceptance.md:39 (Scenario 1) — Severity: MINOR.** Scenario 1 asserts the served rows'
`source` is "`rollup:5m` (or the chosen **finest** divisor)". Same "finest" mischaracterization as
D11; contradicts the larger-divisor tie-break of REQ-CANDLE-001 that this scenario references. Moot
for BTC (single divisor) but it makes the expected `source` for any multi-interval coin underivable
from the SPEC and re-uses the rejected terminology. Fix: "`rollup:5m` (or the source chosen by
`select_source_interval` for an unbounded read)".

## Chain-of-Verification Pass
Second-look, re-reading each section end-to-end (not sampled):
- **Re-read every REQ-CANDLE entry** (spec.md:151-258): normative cores intact; EARS keywords correct;
  the D1 relabel wording (003/004) internally consistent; no residual marker contradiction.
- **Re-checked REQ sequencing** end-to-end: no dupes, uniform padding — MP-1 holds.
- **Re-verified traceability for every REQ** (reverse map): all AC-referenced REQs exist; 021/043 now
  covered; no orphan ACs.
- **Re-read Exclusions (spec.md:277-288) and Edge Cases (spec.md:262-275):** specific, each anchored;
  forming-then-incomplete edge points at REQ-CANDLE-022's delete.
- **Fresh contradiction sweep across ALL four docs, not assuming prior PASS:** the marker relabel,
  the normative reconcile (022), and the unbounded parity scoping are mutually consistent across
  spec/plan/acceptance/compact. The one genuine contradiction is the **source-selection description**:
  Scope (spec.md:107-108) and Scenario 1 (acceptance.md:39) say "finest divisor" while REQ-CANDLE-001,
  the intro (spec.md:16), the reuse list (spec.md:108-110), plan.md:19,42, and spec-compact.md:18 all
  say `select_source_interval`/larger-divisor. This is D11/D12 — a fix that was applied to most mirrors
  but missed two. Grepping "finest" across the dir: spec.md:49 and :158 negate it correctly; plan.md:19
  and :42 negate it correctly; the two POSITIVE (wrong) uses are spec.md:107 and acceptance.md:39.
- **Code-claim audit (all citations re-verified live):** `select_source_interval` candles_agg.rs:96-135
  (window_start floor logic :112-121, `Reverse(*secs)` larger-divisor tie-break :132) ✔;
  `aggregated:` stamp candles_agg.rs:227 ✔; read path passes `params.start` to selector candles.rs:187 ✔;
  coin-scoped `EXISTS` probe candles.rs:106-116 ✔; native branch candles.rs:118-148 ✔; native-source
  guard candles.rs:604-607 ✔; `("coin","cycle_overlay")` network-free arm collection_queue.rs:576-585 ✔;
  migration 0014 current enum `spot,candles,metadata,market,derivatives,cycle_overlay` ✔ (REQ-CANDLE-042
  target = that + `rollup`). **All `file:line` claims in the SPEC are accurate** — the defect is a
  self-contradiction between two SPEC sections, not a mis-citation.

Regression summary: D1, D3, D4, D9, D10, D7 RESOLVED and holding. D2 is **only partially resolved** —
its Scope/Scenario-1 mirror still describes the rejected "finest divisor" model (D11/D12). No must-pass
breach. The FAIL is driven by the D11 MAJOR contradiction (incomplete D2 resolution), not by any
frontmatter/EARS/numbering/traceability failure.

## Recommendation (final iteration — actionable for manager-spec)

This is iteration 3 of 3. One MAJOR contradiction blocks PASS; it is a small, mechanical edit:

1. **Fix D11 (blocking).** Rewrite the Scope bullet spec.md:107-108. Replace
   "from the finest stored fixed-duration interval that evenly divides the target bucket" with, e.g.:
   "from the source interval the read path would select for an unbounded read — via
   `select_source_interval` with `window_start = None` (coverage-scored, tie-broken to the larger
   divisor), typically `5m` for BTC." This aligns Scope with REQ-CANDLE-001, the intro (spec.md:16),
   and finally completes the D2 resolution.
2. **Fix D12 (minor, same edit family).** In acceptance.md:39 change "(or the chosen finest divisor)"
   to "(or the source chosen by `select_source_interval` for an unbounded read)".
3. **No other changes needed.** D1/D3/D4/D7/D9/D10 are resolved and consistent across all four
   documents; MP-1..MP-4 pass; traceability and EARS are clean. After edits 1–2, re-grep the SPEC dir
   for the positive use of "finest" (must return zero matches outside the negating "not a hand-rolled
   'finest divisor'" phrases at spec.md:49,158 and plan.md:19,42) to confirm the contradiction is
   fully purged from every mirror.

Escalation note (iteration 3 FAIL): the remaining defect is narrow, mechanical, and localized to two
lines. It does not indicate a design flaw — the design (select_source_interval reuse, unbounded-scoped
parity, normative reconcile) is sound and internally consistent in the authoritative REQ section. It
is a documentation-consistency miss: the D2 fix was not propagated to the Scope summary. Recommend one
more targeted revision (edits 1–2 above) rather than user intervention on substance.

Verdict: FAIL
