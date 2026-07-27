# SPEC-API-005 Progress

- Started: 2026-07-27
- Tier: M (3 artifacts: spec.md + plan.md + acceptance.md, + progress.md skeleton)
- Methodology: per quality.yaml (brownfield TDD/DDD — characterize existing handler behavior first)
- Language: Rust / moai-lang-rust
- Branch strategy: commit to main (no feature branches), Route A (Hybrid Trunk main-direct)
- Scope: API-boundary contract fixes, query bounds & schema truth (F-29..F-36, F-57, F-58, +F-59
  parameter-parity test). No new endpoint, no migration, no new dependency.

## §E.1 Plan-phase Audit-Ready Signal

- plan_status: audit-ready
- plan_complete_at: 2026-07-27
- Artifacts: spec.md + plan.md + acceptance.md + progress.md (4 files; Tier M)
- REQ IDs allocated: REQ-API-400..417 (18 REQs) — quote-read vs_currency (400/401), 48h bounds +
  module-wide ts-bound invariant (402/403/404), aggregation end-bound + cap-cursor (405/406),
  idempotent ON CONFLICT registration + transaction (407/408), FromRequest extractor wrappers +
  live From<*Rejection> (409/410), search 503 mapping (411), as_of semaphore ceiling (412),
  bidirectional WebSocket read loop (413), CoinCandle flat-table anchor (414), stale
  db_integration cleanup (415), migration-presence rename+extend (416), per-operation
  parameter-parity test (417).
- AC IDs: AC-API-400..417 (1:1 with REQs) + G1..G4 global ACs.
- LOCKED decisions restated D1..D10 (48h/404-on-stale, FromRequest-not-WithRejection, search→503,
  Semaphore, vs_currency, end-bound+cap-cursor, ON CONFLICT+txn, WS read loop, schema truth,
  no-new-deps/Decimal/opaque-cursor).
- Milestones (decision-reversibility order): M1 quote contract & bounds (F-29/F-30) → M2 extractor
  error bodies (F-33) → M3 search 503 (F-34) → M4 aggregation reachability (F-31) → M5 idempotent
  registration (F-32) → M6 as_of ceiling (F-35) → M7 WebSocket read loop (F-36) → M8 schema truth &
  test hygiene (F-57/F-58/F-59).
- DB-gated ACs (hold at `implemented` until live Postgres, per SPEC-PROV-002/003 precedent):
  AC-API-400/401/402/403/404/405/406/407/408/415 — run via
  `DATABASE_URL=... cargo test -- --ignored --test-threads=1`.
- Open items for run: OR-API5-1 (list_quotes end-only window anchor), OR-API5-2 (extractor wrapper
  placement), OR-API5-3 (search error-variant mapping beyond the LOCKED set), OR-API5-4 (as_of
  semaphore placement + permit count), OR-API5-5 (F-31 margin + cap-cursor verification),
  OR-API5-6 (F-58 rewrite-vs-delete per scenario).
- @MX targets: quotes.rs ts-bound anchor generalization; models/quote.rs CoinCandle flat-table
  anchor; api/extract.rs wrapper @MX:ANCHOR; cycle_overlay.rs as_of @MX:WARN; websocket.rs @MX:WARN
  update.

## §E.2 Run-phase Evidence

_<pending run-phase — populated by manager-develop>_

## §E.3 Run-phase Audit-Ready Signal

_<pending run-phase — populated by manager-develop>_

## §E.4 Sync-phase Audit-Ready Signal

_<pending sync-phase — populated by manager-docs>_
