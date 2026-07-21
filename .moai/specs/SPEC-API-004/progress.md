# SPEC-API-004 Progress

- Started: 2026-07-21
- Tier: S (one endpoint, < 5 files, AC in acceptance.md)
- Methodology: TDD (RED-GREEN-REFACTOR), brownfield (characterize existing quote read first)
- Language: Rust / moai-lang-rust
- Branch strategy: commit to main (no feature branches)
- Scope: add `GET /v1/coins/quotes/latest` (all-coin overview: current price + nullable 24h
  baseline); no migration; reuses `coin_quotes` + `tracked_coins`.

## §E.1 Plan-phase Audit-Ready Signal

- plan_status: audit-ready
- plan_complete_at: 2026-07-21
- Artifacts: spec.md + plan.md + acceptance.md + progress.md (4 files)
- REQ IDs allocated: REQ-API-300..309 (endpoint behaviour, bare-quotes envelope, row schema,
  nullable open_24h, DecimalString serialisation, ts-bound/partition-pruning, active-coin filter +
  absent-on-stale, vs_currency default, literal-before-param routing, OpenAPI doc-parity).
- Crux made an acceptance criterion: Scenario 10 requires `EXPLAIN (ANALYZE, BUFFERS)` on a
  populated DB to show partition pruning + index scans, NOT a parent-wide seq scan (REQ-API-305).
- Open items for run: OR-API4-1 (query placement), OR-API4-2 (baseline = earliest-in-window),
  OR-API4-3 (query shape is a proposal to verify), OR-API4-4 (execution-time pruning with `now()`),
  OR-API4-5 (pre-existing quotes.rs:1 comment drift — note only, do not fix).

## §E.2 Run-phase Evidence

All DB-gated scenarios (4, 5, 8, 10) were executed against a live PostgreSQL
(`DATABASE_URL=... cargo test --lib api::quotes -- --ignored --test-threads=1`) and pass. A
correctness defect surfaced by the orchestrator's live-DB reproduction (baseline reused the current
quote as its own `open_24h` for a newly-tracked coin → fake 0% change) was fixed reproduction-first
by adding `AND ts < q.ts` to the baseline LATERAL (see § Defect fix below).

| AC (DoD / REQ) | Status | Verification | Actual Output |
|----------------|--------|--------------|---------------|
| REQ-API-300/302 — overview row shape (coin_id, vs_currency, ts, price, open_24h, source) | PASS | `CoinQuoteOverviewDto` type + serde tests + DB Scenario 1 | `coin_quote_overview_dto_*` ok; DB row shape ok |
| REQ-API-301 — bare `{"quotes":[...]}` envelope, no items/next_cursor | PASS | `coin_quote_overview_page_is_bare_quotes_envelope` | ok — `{"quotes":[]}`, no items/next_cursor |
| REQ-API-303 — open_24h null (never 0, never omitted); baseline strictly older than current quote | PASS | unit `coin_quote_overview_dto_null_open_24h_serializes_as_null` + DB Scenario 5 (both sub-cases: quote-outside-24h AND recent-only) | ok — recent-only coin `open_24h:null` (not `150`); 30h-only coin `open_24h:null` |
| REQ-API-304 — price string, open_24h string-or-null, Decimal end-to-end (no f64) | PASS | `coin_quote_overview_dto_price_and_open_24h_serialize_as_strings` + no-f64 grep | ok — `"price":"67123.45"`, `"open_24h":"65000.00"`/`null` |
| REQ-API-305 — every coin_quotes read ts-bounded; EXPLAIN prunes, no parent seq scan | PASS | DB `db_latest_quotes_overview_explain_prunes_partitions` (EXPLAIN ANALYZE) | ok — `Subplans Removed` present; no parent seq scan; index scans (pruning unaffected by `ts < q.ts`) |
| REQ-API-306 — active-only; absent-on-stale (48h window) | PASS | `WHERE c.status='active'` + CROSS JOIN LATERAL 48h; DB Scenario 4 | ok — stale-only (>48h) coin omitted |
| REQ-API-307 — vs_currency default usd, no allow-list (unrecognised → 200 empty) | PASS | `.unwrap_or("usd")`; DB Scenario 8 `?vs_currency=zzz → {"quotes":[]}` | ok — default usd; unrecognised → `{"quotes":[]}` |
| REQ-API-308 — literal route before /v1/coins/{coin_id} | PASS | `quotes_latest_route_precedes_coin_id_param_route` + `all_routes_are_under_v1` | ok |
| REQ-API-309 — OpenAPI doc-parity (listLatestCoinQuotes under tags: [quotes]) | PASS | `openapi_yaml_contains_all_operation_ids` | ok |
| Quality gate — fmt/clippy/test green | PASS | `cargo fmt --check` / `cargo clippy --all-targets --all-features -- -D warnings` / `cargo test` | exit 0 / exit 0 / exit 0 |
| No f64 for any price value (REQ-PROV-012) | PASS | `grep -rn 'f64' src/api/dto.rs src/api/quotes.rs` | no matches |

**Defect fix (reproduction-first).** Baseline LATERAL now carries `AND ts < q.ts` so `open_24h` is
the earliest quote **strictly older** than the current quote (REQ-API-303 / acceptance Scenario 5
sub-case b). RED: extended `db_latest_quotes_overview_current_stale_and_null_baseline` with a
recent-only coin (single quote at now-1min) → FAILED against the committed query (`open_24h=150`).
GREEN: after `ts < q.ts`, the same test passes (`open_24h:null`) and all prior assertions +
EXPLAIN pruning still pass. acceptance.md Scenario 5 gained an explicit two-sub-case note (edited at
orchestrator direction; normally manager-spec's domain per REQ-ARR-003).

Open-item resolutions: OR-API4-1 → query inline in `list_latest_quotes` (no dedicated `src/db/`
read fn; mirrors existing quotes.rs). OR-API4-2 → baseline = earliest quote in the 24h window
**strictly older than the current quote** (`ORDER BY ts ASC LIMIT 1` + `ts < q.ts`), refining D4's
literal SQL to satisfy REQ-API-303/Scenario 5. OR-API4-3/4 → `CROSS JOIN LATERAL` + `LEFT JOIN
LATERAL` shape verified by live EXPLAIN (execution-time pruning). OR-API4-5 → NOT fixed (out of
scope, note only).

## §E.3 Run-phase Audit-Ready Signal

```yaml
run_complete_at: 2026-07-21
run_commit_sha: d4640cf                  # single amended commit; SHA backfilled at sync (D3 exemption)
run_status: pass
ac_pass_count: 11                        # all AC verified (unit + live-DB)
ac_fail_count: 0
db_scenarios_executed: [1, 3, 4, 5, 8, 10]  # run against live PG (localhost:55432); all pass
preserve_list_post_run_count: 0
new_warnings_or_lints_introduced: 0
cross_platform_build: n/a-single-target  # Rust; no OS build tags in scope
total_run_phase_files: 4                 # src/api/dto.rs, src/api/quotes.rs, src/api/mod.rs, api/crypto-collector.yaml
m1_to_mN_commit_strategy: single-commit  # Tier S, one endpoint (amended in place, not pushed)
```

## §E.4 Sync-phase Audit-Ready Signal

```yaml
sync_complete_at: 2026-07-21
sync_status: audit-ready
sync_commit_sha: pending-backfill-sync   # this commit cannot name its own hash (D3 self-reference exemption)
doc_surface_scope: none                  # no CHANGELOG.md in this repo (do not create); README has no /v1 endpoint list (do not add); OpenAPI (api/crypto-collector.yaml) already updated by manager-develop in run-phase
```
