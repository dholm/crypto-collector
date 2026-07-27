---
name: req-decade-numbering
description: crypto-collector SPECs use milestone-decade REQ numbering (010/020/030...); do NOT fail MP-1 on the intra-block jumps
metadata:
  type: feedback
---

crypto-collector SPECs number requirements in **milestone-decade blocks** (M1→REQ-*-010..019, M2→020..029, ... plus a cross-cutting 080-block). Intra-block jumps (e.g. 064→070 when a milestone has two sub-groups, or 072→080) are INTENTIONAL namespacing, not lost requirements. Grade MP-1 PASS when: zero-padding is consistent (3-digit), no duplicates, each ID maps to a real requirement, and every REQ has AC coverage.

**Why:** MP-1's "no gaps" rule targets *accidental* sequence breaks that signal a dropped/duplicated requirement. This project (and its siblings — REQ-CANDLE-052, REQ-CYCLE-041/042/043, REQ-PROV-003/012) uses decade-block grouping as a deliberate convention. A strict literal "015-019 missing = FAIL" reading would spuriously fail nearly every well-formed SPEC here. SPEC-REFACTOR-001 iter-1 and iter-2 both passed MP-1 under this reading.

**How to apply:** When auditing any crypto-collector SPEC, treat contiguous-within-block + consistent-padding + no-dupes + full-AC-coverage as MP-1 PASS. Surface the decade convention transparently in the evidence line, but do not classify the intra-milestone jumps as gaps. See [[spec-refactor-001-audit]].
