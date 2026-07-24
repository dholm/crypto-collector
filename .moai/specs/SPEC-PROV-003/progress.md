# SPEC-PROV-003 — Progress

Lifecycle: plan → run → sync. Status: **implemented** (held — all 7 milestones implemented +
committed to `main`; sandbox gates green; sync-phase closed; 4 DB-gated tests deferred to a
live-Postgres verify run before a `completed` close).

## §E.1 Plan-phase Audit-Ready Signal

Plan-phase artifacts complete: `spec.md`, `plan.md`, `acceptance.md`, `progress.md`.

- Tier: **M** (standard). Files under change: `src/providers/{coingecko,binance,mod}.rs`,
  `src/config.rs`, `migrations/0021_*.sql` (new) + colocated pure/wiremock tests. Net-new
  logic sits in the M band (300–1000 LOC); the diff is dominated by a data-model change (the
  `Tier` enum) and several localized normalizer fixes. The research role is served by
  `research/idiomatic-rust.md` §6 Category D, so a Tier-L research.md/design.md would
  duplicate it. Consistent with the 7-phase roadmap convention (behavioral-defect phases are
  Tier M; Phase 3 SPEC-PROV-002 was Tier M).
- Requirements: **REQ-PROV-065..081** (17 GEARS requirements), continuing SPEC-PROV-001's
  REQ-PROV-001..045 and SPEC-PROV-002's REQ-PROV-050..064 (non-colliding).
- Findings: F-20 (High), F-21 (High), F-22 (Medium), F-23 (Medium), F-24 (Medium), F-25
  (Low), F-26 (Low), F-27 (Low), F-28 (Low) — Category D of `research/idiomatic-rust.md` §6
  (lines 182–222). Phase 4 of the 7-phase roadmap (§7 line 383); depends on Phase 3
  (SPEC-PROV-002, `implemented`).
- Decisions settled: **D1** F-20 stamp split (mirror Bitstamp `(param, canonical)` tuple);
  **D2** F-20 idempotent guarded cleanup migration (`0021`), plain `UPDATE`, no-op on zero
  rows, PK-collision note in plan.md § Migration Safety; **D3** F-21 typed `Tier` enum +
  fail-fast on unknown + `is_paid()` single authority for header/base-URL/capability;
  **D4** F-22 = `GET /api/v3/ticker/24hr`; **D5** F-23 = optional→None+warn, required
  hard-fail, `Decimal::from_scientific` never `f64`.
- AC count: **11** (AC-PROV-065/067/068/072/074/076/077/079/080/081 + AC-PROV-QG). SSOT =
  acceptance.md.
- Open items: **0** — no `[NEEDS CLARIFICATION]` markers remain.
- Development mode: TDD (brownfield). Route: A (Hybrid Trunk main-direct, Tier M default) —
  commit-direct-to-main per CLAUDE.local.md; no feature branch, no per-phase PR.
- plan_complete_at: 2026-07-24
- plan_status: audit-ready

## §E.2 Run-phase Evidence

Development mode: **TDD** (brownfield). Route A (Hybrid Trunk main-direct). One conventional
commit per milestone, direct to `main` (not pushed — orchestrator owns push + live-DB verify).

### Milestone → commit map

| Milestone | Finding(s) | Commit SHA | Summary |
|-----------|-----------|-----------|---------|
| M1 | F-21 | `e1a77df` | Typed `Tier` enum authority; `is_paid()` drives header + base URL + capability; fail-fast on unknown; `draft → in-progress` |
| M2 | F-20 | `a9a659c` | CoinGecko range snap returns `(param, canonical)`; candles stamped `"1d"`/`"1h"`; cross-module vocabulary test |
| M3 | F-20 mig | `3483947` | Collision-safe `0021_*.sql` (DELETE-shadowed-then-UPDATE); no-DB shape test + 3 DB-gated behaviour tests |
| M4 | F-22 | `cda2788` | Binance spot from `/api/v3/ticker/24hr` (price=lastPrice, real volume, bid/ask); no 1m volume in a 24h field |
| M5 | F-23 (+F-27 pt1) | `0565f46` | `Decimal::from_scientific` fallback; optional→None+warn, required hard-fail; `last_updated` warn |
| M6 | F-24 | `a1503cd` | Boundary-aware + venue-preferring + deterministic derivatives match (`select_deriv_ticker`) |
| M7 | F-25/26/27/28 | `befe322` | Snapped kline limit; `NoCapableProvider`; `max_supply` debug; by-reference DTO iteration |

### Per-AC PASS/FAIL matrix (SSOT = acceptance.md; 11 AC)

| AC | REQ | Status | Evidence (verification) | Actual output |
|----|-----|--------|-------------------------|---------------|
| AC-PROV-065 | 065/066 | PASS | `cargo test --lib providers::coingecko` range_snap + cross-module vocabulary + wiremock stamp | `test result: ok. 65 passed` (range_stamp_resolves_through_interval_to_seconds_vocabulary ok) |
| AC-PROV-067 | 067 | PASS-WITH-DEBT | no-DB shape test PASS (`migration_files`); 3 DB-gated behaviour tests deferred | `coingecko_range_interval_migration_is_collision_safe ... ok`; 3 `#[ignore]` (live-DB) |
| AC-PROV-068 | 068/069/070/071 | PASS | `cargo test --lib config::` tier matrix + fail-fast + analyst wiremock header | `tier_matrix_… ok`, `tier_parse_unknown_fails_fast_naming_the_value ok`, `analyst_tier_sends_pro_key_header… ok` |
| AC-PROV-072 | 072/073 | PASS | pure `normalise_ticker_24hr` + client wiremock (no-DB); DB-gated provider test deferred | `ticker_24hr_normalises_price_from_last_price… ok`, `http_ticker_24hr_… ok` |
| AC-PROV-074 | 074/075 | PASS | `decimal_from_number` plain/high-precision/scientific + optional degrade + required hard-fail | `decimal_from_number_parses_plain_high_precision_and_scientific_exactly ok` |
| AC-PROV-076 | 076 | PASS | behaviour-preserving: last_updated Utc::now() + max_supply None, both logged | `coin_detail_unparseable_max_supply_degrades_to_none_item_survives ok` |
| AC-PROV-077 | 077/078 | PASS | `symbol_matches_base` + `select_deriv_ticker` boundary/venue/tie-break | `symbol_boundary_match_excludes_dominance_and_leveraged_tokens ok`, 4 select_deriv_ticker tests ok |
| AC-PROV-079 | 079 | PASS | `secs_to_kline_interval` tuple + `kline_limit` between-band | `snap_between_band_limit_divides_by_snapped_secs_not_raw ok` |
| AC-PROV-080 | 080 | PASS | `NoCapableProvider` for non-empty all-unsupported; empty chain preserved | `chain_records_unsupported_outcome ok`, `chain_fetch_ohlc_empty_chain_still_reports_empty ok` |
| AC-PROV-081 | 081 | PASS | `grep -c '.as_array().cloned()' coingecko.rs` == 0; wiremock tests unchanged-green | grep count `0`; search/tickers wiremock tests ok |
| AC-PROV-QG | — | PASS | `cargo fmt --check` exit 0; `cargo clippy … -D warnings` exit 0; `cargo test` 0 failed; no new dep; no new f64; anchor untouched | fmt 0 / clippy 0 / 652 passed / Cargo.toml+lock diff empty / candles_agg.rs unchanged |

### Deferred (DB-gated `#[ignore]`, run at verify with `DATABASE_URL=… cargo test -- --ignored --test-threads=1`)

- `tests/db_integration.rs`: `scenario_02_migration_0021_is_noop_on_canonical_rows`,
  `…_rewrites_daily_to_1d_idempotently`, `…_drops_shadowed_duplicate_without_pk_violation`
  (FK-parent-seeded, child-before-parent teardown; exercise the SHIPPED 0021 body).
- `src/providers/binance.rs`: `fetch_spot_uses_24hr_ticker_price_volume_and_bid_ask`
  (provider-level end-to-end through the pacer → real DB round-trip).

### @MX tags placed

- `@MX:ANCHOR` on `config::Tier` (tier authority), `coingecko_range_snap_interval` (canonical
  stamp), `decimal_from_number` (Decimal-only parse core).
- `@MX:NOTE` on Binance `fetch_spot` (24hr-ticker source) and CoinGecko `fetch_derivatives`
  (boundary/venue/tie-break match).
- `ProviderError::NoCapableProvider(Capability)` added (F-26).

## §E.3 Run-phase Audit-Ready Signal

```yaml
run_complete_at: 2026-07-24
run_commit_sha: befe322            # M7, final implementation commit (progress.md is a follow-up chore)
run_status: pass-with-debt          # sandbox gates all green; DB-gated behaviour tests deferred to live-Postgres verify
ac_pass_count: 10                   # AC-PROV-065/068/072/074/076/077/079/080/081 + AC-PROV-QG
ac_pass_with_debt_count: 1          # AC-PROV-067 (migration behaviour DB-gated; no-DB shape test PASS)
ac_fail_count: 0
preserve_list_post_run_count: 0     # touched only the PRESERVE set (coingecko/binance/mod.rs, config.rs, migrations/0021, colocated tests)
new_warnings_or_lints_introduced: 0 # cargo clippy --all-targets --all-features -- -D warnings exit 0
cargo_fmt_check: pass               # exit 0
cargo_clippy_deny_warnings: pass    # exit 0
cargo_test_sandbox: pass            # 652 passed; 0 failed; 71 ignored (lib) + 20 migration_files + others
no_new_dependency: true             # git diff Cargo.toml Cargo.lock empty
no_new_f64_monetary: true           # (f64, MarketSearchResult) ordering key is net-zero pre-existing
vocabulary_anchor_untouched: true   # src/api/candles_agg.rs unchanged
f28_deep_clone_grep: 0              # grep -c '.as_array().cloned()' src/providers/coingecko.rs
total_run_phase_files: 6            # src/config.rs, src/providers/{coingecko,binance,mod}.rs, migrations/0021_*.sql, tests/{migration_files,db_integration}.rs (+ spec.md status flip, progress.md)
m1_to_mN_commit_strategy: one-conventional-commit-per-milestone-direct-to-main-not-pushed
push_state: not-pushed              # orchestrator owns push + live-DB verify + sync/close
```

## §E.4 Sync-phase Audit-Ready Signal

```yaml
sync_complete_at: 2026-07-24
sync_commit_sha: a39f9b572fb02b796c2dae9b37479e20df6a0f04
sync_status: pass-with-debt          # sync-auditor PASS-WITH-DEBT ~= 0.91; held at implemented, not completed
sync_auditor_verdict: "PASS-WITH-DEBT ~= 0.91 (4-dim: Func 92 / Sec 90 / Craft 88 / Consist 93)"
changelog_entry_position: "Unreleased > Fixed, immediately above the SPEC-PROV-002 entry"
frontmatter_status_transitions:
  spec_md: "in-progress -> implemented"
  plan_md: "no frontmatter block (narrative doc, no status field)"
  acceptance_md: "no frontmatter block (narrative doc, no status field)"
  progress_md: "in-progress -> implemented (prose status line, §E.1 header)"
mx_tags_validated: true              # @MX:ANCHOR x3 (config::Tier, coingecko_range_snap_interval, decimal_from_number) + @MX:NOTE x2 (Binance fetch_spot, CoinGecko fetch_derivatives) — all present, well-formed, @MX:REASON + @MX:SPEC populated; no new tags required
db_gated_debt:
  deferred_tests: 4
  tests:
    - tests/db_integration.rs::scenario_02_migration_0021_is_noop_on_canonical_rows
    - tests/db_integration.rs::scenario_02_migration_0021_rewrites_daily_to_1d_idempotently
    - tests/db_integration.rs::scenario_02_migration_0021_drops_shadowed_duplicate_without_pk_violation
    - src/providers/binance.rs::fetch_spot_uses_24hr_ticker_price_volume_and_bid_ask
  run_command: "DATABASE_URL=... cargo test -- --ignored --test-threads=1"
  reason: "no sandbox Postgres available; destructive migration/DB tests must not run against production"
future_spec_note: "sync-auditor F2 — Binance volume_24h is base-asset volume while CoinGecko total_volume is quote-currency; a possible cross-source unit mismatch to reconcile in a later SPEC (not a defect in this SPEC; both are spec-compliant per their own upstream contracts)"
b12_self_test_a: "grep -c 'SPEC-PROV-003' CHANGELOG.md == 0 before emission (parallel-session dup guard)"
b12_self_test_b: "acceptance.md AC row count == 11 (AC-PROV-065/067/068/072/074/076/077/079/080/081 + AC-PROV-QG); CHANGELOG entry references the same 11"
b12_self_test_c: "every file path in the CHANGELOG entry verified via ls before commit"
canary_compliance_check:
  applicable: false
  reason: "SPEC-PROV-003 does not define a forward-looking policy that its own sync tests"
```

Held at `implemented`, not `completed`, mirroring the SPEC-PROV-002 precedent: 4 DB-gated
tests are `#[ignore]` and were not executed (no sandbox Postgres; the migration tests are
destructive and must not run against production). A `completed` close follows a live-Postgres
verification pass running `DATABASE_URL=... cargo test -- --ignored --test-threads=1`.
