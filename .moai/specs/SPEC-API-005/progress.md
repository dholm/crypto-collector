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

Route A (Hybrid Trunk main-direct), Tier M, TDD (RED-GREEN-REFACTOR; DB-gated ACs written as
`#[ignore]` + correct-by-construction, deferred to live Postgres per SPEC-PROV-002/003 precedent).

### Per-milestone commits

| Milestone | Commit | Findings | Files |
|-----------|--------|----------|-------|
| track | 230ffcb-style chore | plan-phase artifacts tracked unchanged | 4 spec artifacts |
| M1 | `a077df0` | F-29/F-30 vs_currency + 48h quote-read bounds; draft→in-progress | quotes.rs, spec.md |
| M2 | `b01e5c5` | F-33 ApiJson/ApiQuery/ApiPath wrappers + handler migration | extract.rs + 7 files |
| M3 | `eccfba7` | F-34 search 503 for pacer/credit errors | coins.rs |
| M4 | `2e26241` | F-31 aggregation end-bound + cap-cursor | candles.rs |
| M5 | `b6ff21e` | F-32 idempotent ON CONFLICT registration in one tx | coins.rs |
| M6 | `ad9ef7c` | F-35 as_of recompute Semaphore ceiling | cycle_overlay.rs |
| M7 | `32574f2` | F-36 bidirectional WebSocket read loop + pings | websocket.rs, Cargo.toml/lock |
| M8 | `41317f4` | F-57/F-58/F-59 schema truth + test hygiene + parity test | models/quote.rs, db_integration.rs, migration_files.rs, mod.rs |

### AC PASS/FAIL matrix

| AC | REQ | Status | Evidence |
|----|-----|--------|----------|
| AC-API-400 | 400 | COMPILED, DEFERRED (live Postgres) | `db_get_latest_quote_vs_currency_default_explicit_and_unknown` (#[ignore]) |
| AC-API-401 | 401 | COMPILED, DEFERRED | `db_list_quotes_duplicate_ts_across_currencies_no_row_loss` (#[ignore]) |
| AC-API-402 | 402 | COMPILED, DEFERRED | `db_get_latest_quote_stale_returns_404_fresh_returns_200` (#[ignore]) |
| AC-API-403 | 403 | COMPILED, DEFERRED | `db_list_quotes_default_48h_window_and_explicit_start` (#[ignore]) |
| AC-API-404 | 404 | COMPILED, DEFERRED + G2 grep PASS | `db_quote_reads_explain_prune_partitions` (#[ignore]); G2 grep verified |
| AC-API-405 | 405 | COMPILED, DEFERRED | `db_aggregation_far_past_window_is_reachable` (#[ignore]) |
| AC-API-406 | 406 | COMPILED, DEFERRED | `db_cap_hit_gap_dropped_page_continues` (#[ignore]) |
| AC-API-407 | 407 | COMPILED, DEFERRED | `db_register_coin_concurrent_duplicate_no_500` (#[ignore]) |
| AC-API-408 | 408 | COMPILED, DEFERRED | `db_register_coin_insert_and_enqueues_are_atomic` (#[ignore]) |
| AC-API-409 | 409 | PASS | `malformed_json_body_returns_json_error_body`, `malformed_query_value_returns_json_error_body` — ok |
| AC-API-410 | 410 | PASS | handlers migrated to ApiJson/ApiQuery/ApiPath; From<{Json,Query,Path}Rejection> live |
| AC-API-411 | 411 | PASS | `search_pacer_cooldown_returns_503`, `search_credit_exhausted_returns_503`, `search_empty_result_returns_200_empty` — ok |
| AC-API-412 | 412 | PASS | `as_of_recompute_semaphore_is_bounded_and_released_on_drop` — ok (full recompute DB-gated) |
| AC-API-413 | 413 | PASS | `ws_client_close_terminates_stream_task` — ok |
| AC-API-414 | 414 | PASS | CoinCandle @MX:ANCHOR rewritten to flat table; CoinQuote unchanged |
| AC-API-415 | 415 | COMPILED, DEFERRED (G3) | `tests/db_integration.rs` rewritten (16 scenarios #[ignore]); full-suite pass needs live DB |
| AC-API-416 | 416 | PASS | `all_migration_files_exist` renamed+extended to 0001–0021 — ok (cargo test --test migration_files: 21 passed) |
| AC-API-417 | 417 | PASS | `openapi_query_params_have_matching_struct_fields` — ok (verified RED once against a documented-but-unimplemented param, then green) |
| G1 | 417 | PASS | per-operation parameter-parity test green |
| G2 | 404 | PASS | `grep -n "FROM coin_quotes" src/api/*.rs` → every read carries a `ts >=`/`ts <`/`now() - interval` bound |
| G3 | 415 | DEFERRED (live Postgres) | `tests/db_integration.rs` (16 #[ignore] scenarios) |
| G4 | — | PASS | `cargo fmt --check` exit 0; `cargo clippy --all-targets --all-features -- -D warnings` exit 0; `cargo test` exit 0 |

### Final verification (verbatim, non-DB scope)

```
$ cargo fmt --check                                        → exit 0
$ cargo clippy --all-targets --all-features -- -D warnings → exit 0 (no warnings)
$ cargo test                                               → exit 0
  lib:              661 passed; 0 failed; 80 ignored
  model_serde:       12 passed; 0 failed
  migration_files:   21 passed; 0 failed
  db_integration:     0 passed; 0 failed; 16 ignored (DB-gated)
  backtest_projection: 2 passed; 0 failed
  alarm_docs_parity:  8 passed; 0 failed
```

### Resolved-decision confirmations

- **D1 / OR-API5-1 (anchor-on-`end`)**: `list_quotes` default 48h window uses
  `ts >= COALESCE($end, now()) - interval '48 hours'` — anchored on `end` when supplied
  (`[end-48h, end]`), else `now()` (`[now()-48h, now()]`), a bare `ts >=` predicate for
  partition pruning; only applied when neither `start` nor `cursor` is supplied (quotes.rs
  list_quotes default-window branch).
- **F-30 invariant text**: the quotes.rs ts-bound `@MX:ANCHOR`/`@MX:WARN` generalized to cover
  all three coin_quotes readers (get_latest_quote, list_quotes, list_latest_quotes) with no
  exemption (REQ-API-404). Verified by G2 grep.
- **D10 (no new dependency)**: `git diff Cargo.lock` added 0 packages; the axum-test `ws` feature
  (M7) activated only crates already in the tree. No `axum-extra`/`WithRejection`, no memoization
  crate, no `f64` for money.

## §E.3 Run-phase Audit-Ready Signal

```yaml
run_complete_at: 2026-07-27
run_commit_sha: 41317f4   # M8 (final milestone); progress.md evidence commit follows
run_status: implemented-pending-db   # held at `implemented`; 10 DB-gated ACs deferred to live Postgres
ac_pass_count: 12        # AC-409/410/411/412/413/414/416/417 + G1/G2/G4 (+ implicit; DB-gated excluded)
ac_fail_count: 0
ac_deferred_db_count: 10 # AC-400/401/402/403/404/405/406/407/408/415 (compiled #[ignore], correct-by-construction)
preserve_list_post_run_count: 0   # no PRESERVE-list file violations; scope confined to src/api, src/models/quote.rs, tests/
l44_pre_commit_fetch: not-performed-by-agent   # manager-develop did not push; orchestrator owns fetch/push (per feedback memory)
l44_post_push_fetch: not-performed-by-agent
new_warnings_or_lints_introduced: 0   # clippy -D warnings exit 0; fmt --check exit 0
cross_platform_build:
  host_debug: pass          # cargo test compiled + ran (host x86_64)
  aarch64_cross: deferred   # make push-aarch64 is a deploy step (note), not run-phase gate
total_run_phase_files: 12  # extract.rs (new) + quotes/coins/candles/cycle_overlay/metadata/coin_market/mod/websocket.rs + models/quote.rs + db_integration.rs + migration_files.rs (+ Cargo.toml/lock, spec.md frontmatter)
m1_to_mN_commit_strategy: per-milestone (M1..M8, one commit each; direct-to-main, not pushed by agent)
```

## §E.4 Sync-phase Audit-Ready Signal

_<pending sync-phase — populated by manager-docs>_
