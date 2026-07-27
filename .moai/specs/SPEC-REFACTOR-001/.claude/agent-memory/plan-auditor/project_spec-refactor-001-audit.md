---
name: spec-refactor-001-audit
description: SPEC-REFACTOR-001 (Phase 7 refactor) plan-audit outcome + the deferred SPEC-COINDIR-001 forward reference
metadata:
  type: project
---

SPEC-REFACTOR-001 (Phase 7: Batching & Structural Debt Reduction) passed plan-audit at iteration 2 with verdict PASS, aggregate 0.92 (Tier L threshold 0.85). iter-1 FAILed solely on MP-7 (two `[NEEDS CLARIFICATION]` markers in plan.md §B); iter-2 cleared them. The M6 literal-anchor design folds BOTH `SUPPORTED_INTERVALS` and `interval_to_seconds` into one `ApiInterval` enum (15 fixed-duration variants) with a total `secs()` + an `is_api_facing()` boundary predicate that preserves the existing HTTP 400 for storage-only intervals (e.g. `3m`).

**Why:** The SPEC references **SPEC-COINDIR-001** as a FUTURE (not-yet-created) SPEC — it is the deferred Opt B of the F-50 CoinDirectory-trait split (extract a CoinGecko-only `CoinDirectory` trait + rewire the API search path). SPEC-REFACTOR-001 adopted Opt A (search pair stays on the single `Provider` trait with `Ok(vec![])` default). The COINDIR-001 reference is a deliberate forward-reference, not a D7 defect.

**How to apply:** If a future session audits or plans SPEC-COINDIR-001, its scope is the F-50 Opt B carve-out deferred here. When auditing SPEC-REFACTOR-001, the COINDIR-001 "referenced-but-not-found" D7 hit is intended-future-SPEC (SHOULD severity at most), never a BLOCKING/MP-5 failure. See [[req-decade-numbering]].
