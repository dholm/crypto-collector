# SPEC-API-003 Progress

- Started: 2026-07-01
- Tier: S (one endpoint enhanced, < 5 files, AC in acceptance.md — 16 scenarios + DoD)
- Methodology: TDD (RED-GREEN-REFACTOR), brownfield (extends SPEC-API-002 `list_candles`)
- Language: Rust / moai-lang-rust
- Branch strategy: commit to main (no feature branches)
- Scope: read-time OHLCV aggregation fallback for `GET /v1/coins/{coin_id}/candles` — when no
  native candles exist at the exact `interval`, compose coarser buckets from finer stored candles
  (`src/api/candles_agg.rs`); no migration; reuses `coin_candles`.

> **Retroactive sync note.** Implementation landed in commit `bcb9d99` (2026-07-01) and was merged
> to `main` (49 commits deep at close) and deployed. The sync-phase close (this file + frontmatter
> `draft → completed`) was performed on 2026-07-21, mirroring the SPEC-API-004 `d1f8540` close.

## §E.2 Run-phase Evidence

Implementation committed in `bcb9d99` (run-phase, 2026-07-01): `src/api/candles.rs` (+608),
`src/api/candles_agg.rs` (new, 883 LOC — pure fold logic + 50 unit tests), `src/api/mod.rs`,
`api/crypto-collector.yaml` (OpenAPI doc-parity). Sync-time re-verification (2026-07-21) confirms
the merged tree is green.

| AC (Scenario / REQ) | Status | Verification | Evidence (observed at sync 2026-07-21) |
|---------------------|--------|--------------|----------------------------------------|
| S2/S3 — OHLC fold + largest-divisor selection (REQ-API-201/205/206/208/212) | PASS | `api::candles_agg::tests` unit fold tests | ran `ok` in `cargo test` lib binary |
| S5/S6 — volume sums iff all components present, else null (REQ-API-207a/207b) | PASS | `aggregate_candles_*` volume unit tests | ran `ok` |
| S7/S8/S8b — closed gap dropped, forming bucket incomplete, closed-incomplete dropped even if newest (REQ-API-209/210/211/214) | PASS | `aggregate_candles_closed_gap_bucket_dropped_scenario_7`, `..._closed_incomplete_dropped_even_if_newest_scenario_8b` | ran `ok` |
| S9 — no divisor stored → empty page, not error (REQ-API-202) | PASS | `aggregate_candles_empty_source_returns_empty` | ran `ok` |
| S10 — divisibility by seconds modulo; bucket_start alignment (REQ-API-203) | PASS | `bucket_start_4h_alignment`, `bucket_start_1d_alignment` | ran `ok` |
| S1/S4/S11/S12/S14/S15/S16 — native precedence, keyset pagination, vs_currency boundary, start/end filter (DB-gated) | HISTORICAL | `db_scenario_*` — `#[ignore]`, verified live at run-phase (bcb9d99); NOT re-run at sync | `#[ignore]` in this run (58 ignored) — see Gaps |
| Quality gate — clippy/test green | PASS | `cargo clippy --all-targets --all-features -- -D warnings`; `cargo test` | exit 0 / exit 0 — 620 passed, 0 failed, 58 ignored |
| No `f64` for any OHLCV value (REQ-PROV-012 / REQ-API-216) | PASS | Decimal end-to-end in `candles_agg.rs` | fold operates on `Option<Decimal>` |

**Baseline-attribution.** All PASS rows above are attributed to the `cargo test` + `cargo clippy`
run against `main` HEAD at 2026-07-21 (logs: `.moai/state/verify/api003-sync/{test,clippy}.log`).

**Gaps.** The DB-gated scenarios (`db_scenario_*`, `db_candle_001_*`) are `#[ignore]` and were NOT
re-executed at sync — they require a live PostgreSQL (`DATABASE_URL=... cargo test -- --ignored`).
They were verified live during the original run-phase (bcb9d99); this sync did not reproduce that.

**Residual-risk.** Since 2026-07-01, SPEC-CANDLE-001 (deployed) introduced materialized native
`rollup:5m` rows that serve 1d/1w as the primary path, so this SPEC's read-time aggregation is now
the coverage-aware **fallback** rather than the sole path. `src/api/candles_agg.rs` remains live in
that fallback role (present in HEAD). No behavioral regression is implied — the aggregation is
reached only when no native candles cover the requested interval. See project memory
`candle-agg-coverage-aware`.

## §E.3 Run-phase Audit-Ready Signal

```yaml
run_complete_at: 2026-07-01
run_commit_sha: bcb9d99                   # single run-phase commit (merged to main)
run_status: pass
ac_pass_count: 8                          # unit-verified AC groups (fold logic) observed green at sync
ac_db_gated_historical: 7                 # DB-gated scenarios verified at run-phase, #[ignore] at sync
ac_fail_count: 0
db_scenarios_executed: historical         # run against live PG at run-phase (bcb9d99); #[ignore] at sync
preserve_list_post_run_count: 0
new_warnings_or_lints_introduced: 0
cross_platform_build: n/a-single-target   # Rust; aarch64 target
total_run_phase_files: 4                   # src/api/candles.rs, src/api/candles_agg.rs, src/api/mod.rs, api/crypto-collector.yaml
m1_to_mN_commit_strategy: single-commit    # Tier S, one endpoint enhancement
```

## §E.4 Sync-phase Audit-Ready Signal

```yaml
sync_complete_at: 2026-07-21
sync_status: audit-ready
sync_commit_sha: pending-backfill-sync    # this commit cannot name its own hash (D3 self-reference exemption)
doc_surface_scope: none                    # no CHANGELOG.md in this repo (do not create); README has no /v1 endpoint list; OpenAPI (api/crypto-collector.yaml) already updated by run-phase (bcb9d99)
retroactive_sync: true                     # code merged+deployed since 2026-07-01; sync close performed 2026-07-21
supersession_relationship: none            # SPEC-CANDLE-001 made this the fallback path, not a supersession
```
