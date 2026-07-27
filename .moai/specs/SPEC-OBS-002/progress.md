# SPEC-OBS-002 Progress

- Started: 2026-07-27
- Tier: M (3 artifacts: spec.md + plan.md + acceptance.md, + progress.md skeleton)
- Methodology: per quality.yaml (brownfield — characterize existing lifecycle/observability behavior first)
- Language: Rust / moai-lang-rust
- Branch strategy: commit to main (no feature branches), Route A (Hybrid Trunk main-direct)
- Scope: lifecycle, shutdown & observability integrity (F-38..F-49). No new endpoint, no migration,
  no new dependency.

## §E.1 Plan-phase Audit-Ready Signal

- plan_status: audit-ready
- plan_complete_at: 2026-07-27
- Artifacts: spec.md + plan.md + acceptance.md + progress.md (4 files; Tier M)
- REQ IDs allocated: REQ-OBS-060..074 (15 REQs) + REQ-ALARM-080..082 (3 REQs) = 18 total.
  - Metric integrity: REQ-OBS-060 (shared const SSOT), 061 (describe/emit parity), 062 (drop tracked_markets ghost).
  - Supervision/durability: REQ-OBS-063 (generic supervisor + capped backoff), 064 (relay initial-connect retry), 065 (single run_supervised).
  - Bounded shutdown: REQ-OBS-066 (timeout(drain_secs, supervisor) + always pool.close/telemetry.shutdown), 067 (broadcast-before-drain + 15s grace preserved), 068 (airtight shutdown arms).
  - Readiness: REQ-OBS-069 (bind-before-ready), 070 (flags-before-cache).
  - Config diagnostics: REQ-OBS-071 (warn on unparseable), 072 (fail-fast where dangerous).
  - Credential-safe connection: REQ-OBS-073 (PgConnectOptions from parts).
  - Dead-surface: REQ-OBS-074 (single HeaderExtractor/OtelMakeSpan; delete start_api_server).
  - Alarm: REQ-ALARM-080 (sustained all_providers_down), 081 (observe_chain_records doc/behavior per REQ-ALARM-020), 082 (reconciler MissedTickBehavior::Skip).
- AC IDs: AC-OBS-060..074 (1:1 with REQs) + AC-ALARM-080/081/082 + G1 (quality gates) + G2 (rename operator-visibility). Nearly all unit-testable (no live DB); AC-OBS-064 and AC-OBS-073 have optional DB-gated variants but a non-DB-gated primary path.
- LOCKED decisions restated D1..D10 (metric rename, supervised relays, PgConnectOptions, bounded shutdown, bind-before-ready + flags-before-cache, warn+fail-fast config, generic supervisor+backoff, airtight arms, alarm signal quality, dead-surface cleanup).
- Milestones (decision-reversibility order — highest-change-likelihood first): M1 metric SSOT + parity + ghost removal (F-38/F-49) → M2 alarm signal quality (F-42/F-48) → M3 bounded shutdown + airtight arms (F-41/F-47) → M4 exact readiness (F-43/F-44) → M5 config diagnostics (F-45) → M6 credential-safe connection (F-40) → M7 generic supervisor + relay supervision (F-39/F-46) → M8 dead-surface cleanup (F-49). Execution-dependency note: within M7, run_supervised lands before the folded relay supervision.
- Cross-SPEC reconciliations: REQ-OBS-062 removes the tracked_markets half of SPEC-OBS-001 REQ-OBS-013 (table dropped by migration 0011); REQ-OBS-060 makes emitters match REQ-OBS-015's already-specified names. New numbered SPEC (not an in-place amendment); parent SPECs unedited.
- Open items for run: OR-OBS2-1 (run_supervised backoff constants), OR-OBS2-2 (all_providers_down sustained-window duration), OR-OBS2-3 (REQ-ALARM-081 doc-vs-code authoritative + non-Network streak decision per REQ-ALARM-020), OR-OBS2-4 (live_poller.rs:665 tracked_markets emit-vs-describe), OR-OBS2-5 (OtelMakeSpan placement + canonical test set).
- @MX targets: metrics name consts @MX:ANCHOR + @MX:NOTE (operator-visible rename); run_supervised @MX:ANCHOR + @MX:WARN (backoff-reset window); main.rs shutdown sequence @MX:ANCHOR + @MX:WARN (ordering); health check_readiness @MX:WARN (flags-before-cache); config.rs @MX:NOTE (warn-vs-fail-fast split).

## §E.2 Run-phase Evidence

Run-phase COMPLETE (2026-07-27, cycle_type=tdd, M1→M8). See § Run-phase completion evidence
below. All 18 ACs + G1/G2 PASS; commits M1..M8 on `main` (local, unpushed — push deferred to
the orchestrator). The abort/recovery note below is retained as historical record.

### Run-phase abort/recovery note (orchestrator ledger closure — 2026-07-27)

- Run-phase delegation (manager-develop, cycle_type=tdd, M1→M8) was **aborted mid-M1** by an environment session-limit API error (resets 12:20pm Europe/Stockholm). NOT a code/delegation defect.
- Observed tree state at abort (read-only verified, not assumed):
  - No run-phase commit landed. HEAD = `b1e613a` (plan baseline; local-only, `0 1` ahead of unpushed origin).
  - `src/metrics/mod.rs`: uncommitted partial M1 edit — the two metric-name `pub const`s added (`QUOTE_INSERT_DURATION_SECONDS`, `CANDLE_INSERT_DURATION_SECONDS`). Emitter references (`src/db/upserts.rs`), persistence-latency test updates, the describe/emit parity test, and the `tracked_markets` ghost removal are NOT yet done.
  - Frontmatter `status: draft` (unchanged — no run commit, so no `draft → in-progress` transition yet).
- Resume: re-run `/moai run SPEC-OBS-002` after the limit resets. manager-develop continues M1 from the present tree (consts already present — extend to emitters/tests/parity/ghost-removal), then M2→M8. Author email note: recent commits used `dholmster@gmail.com` (env git config); intended author is `david@dholm.com` — verify `git config user.email` at resume.

### Run-phase completion evidence (2026-07-27)

**Open-item resolutions (OR-OBS2-1..5):**

- **OR-OBS2-1** (backoff constants): `run_supervised` uses capped exponential backoff — initial **1 s**, cap **30 s** (doubling), reset-after-healthy window **60 s** (`SUPERVISE_INITIAL_BACKOFF` / `SUPERVISE_MAX_BACKOFF` / `SUPERVISE_HEALTHY_RESET` in `src/collectors/mod.rs`). Consistent with the `src/db/pool.rs` `RETRY_INITIAL_BACKOFF`(1 s)/`RETRY_MAX_BACKOFF`(30 s) precedent.
- **OR-OBS2-2** (sustained-window for `all_providers_down`): **180 s** (`ALARM_ALL_PROVIDERS_DOWN_SECS`, `src/config.rs`). Shorter than the per-provider 300 s (a whole-chain outage is more urgent), a multiple of the 30 s reconcile interval (~6 consecutive all-failed sweeps).
- **OR-OBS2-3** (REQ-ALARM-081 authoritative source + non-Network streak): **CODE is authoritative.** `observe_chain_records` derives ONLY the chain-outcome signal (all-failed vs any-success among attempted records); it does NOT touch the per-provider network-failure streak. The per-provider `provider-unreachable` streak counts **ONLY `ProviderError::Network` failures** — non-`Network` failures (repeated 5xx) do NOT count. Enforced at the concrete-error call sites (`src/providers/mod.rs` `chain_fetch_ohlc`: `if matches!(e, ProviderError::Network(_))`) + the `consecutive_network_failures` field (REQ-ALARM-020). The prior `observe_chain_records` doc ("records ANY failure as a network failure") was WRONG and is corrected; two parity tests in `tests/alarm_docs_parity.rs` pin the contract.
- **OR-OBS2-4** (`live_poller.rs` `tracked_markets` reference): it was a **vestigial negative TEST assertion** (`claim_sql_targets_tracked_coins` asserted the claim SQL does NOT reference `tracked_markets`), NOT a live emit. The `markets` table was dropped by migration `0011_remove_markets.sql`, so the negative check on a non-existent table is dead — removed; the positive `targets tracked_coins` assertion remains.
- **OR-OBS2-5** (`OtelMakeSpan` placement + canonical test set): `OtelMakeSpan` moved into `src/telemetry/mod.rs` immediately after the canonical `HeaderExtractor` (the extractor it uses; the telemetry module doc already anticipated it). The **canonical test set** is the `telemetry` `header_extractor_*` tests + a new `otel_make_span_builds_span_without_panic` smoke test; the duplicated `main.rs` HeaderExtractor tests were removed.

**AC PASS/FAIL matrix (E1):** all non-DB-gated primary paths PASS.

| AC | Status | Verification |
|----|--------|--------------|
| AC-OBS-060 | PASS | `grep -rn 'coin_quote_insert\|coin_candle_insert' src/` → 0; consts referenced at describe + emitter sites; `cargo test --lib persistence_metric_describe_emit_parity` ok |
| AC-OBS-061 | PASS | `persistence_metric_describe_emit_parity` green (emit-through-const structural parity) |
| AC-OBS-062 | PASS | `grep -rn 'tracked_markets' src/` → 0; `tracked_coins` gauge still described + `tracked_coins_gauge_registered` green |
| AC-OBS-063 | PASS | `supervise_backoff_grows_caps_and_resets` green (1→2→4→8→16→30 cap; healthy-run reset) |
| AC-OBS-064 | PASS | `relay_returns_err_on_initial_connect_failure` + `run_listener_doc_describes_supervised_retry` green |
| AC-OBS-065 | PASS | `grep -rn 'run_supervised_' src/` → 0; `only_one_generic_supervisor_no_underscore_variants` green; log severity identical (both arms `error!`) |
| AC-OBS-066 | PASS | `bounded_drain_times_out_on_wedged_worker_and_still_cleans_up` + `bounded_drain_returns_early_when_workers_finish` green (virtual-time) |
| AC-OBS-067 | PASS | `shutdown_sequence_order_grace_then_broadcast_then_bounded_drain` + `shutdown_timing_grace_plus_drain_fits_in_termination_grace` green |
| AC-OBS-068 | PASS | `listener_shutdown_arm_guards_dropped_sender` + `reconciler_shutdown_arm_guards_dropped_sender` green; worker loops already airtight (SPEC-SCHED-002) |
| AC-OBS-069 | PASS | `readiness_flips_only_after_api_bind_and_relay_spawn` green (source-order: relay spawn + bind before `set_ready`, only `axum::serve` follows) |
| AC-OBS-070 | PASS | `readiness_503_on_shutdown_even_with_warm_cache` + `readiness_flags_are_never_served_from_cache` green |
| AC-OBS-071 | PASS | `parse_env_value_present_but_unparseable_warns_and_defaults` green (warn path taken) |
| AC-OBS-072 | PASS | `resolve_pacer_cooldown_present_but_unparseable_fails_fast` (`#[should_panic]`) green |
| AC-OBS-073 | PASS | `special_char_password_yields_correct_connect_options` (host/port/db/user preserved; naive-URL contrast corrupts) + `database_url_override_is_parsed` green |
| AC-OBS-074 | PASS | `grep -n 'HeaderExtractor' src/main.rs` → 0; `grep -rn 'start_api_server' src/` → 0; single struct each in `src/telemetry/`; telemetry tests green |
| AC-ALARM-080 | PASS | `all_providers_down_{not_raised_by_single_failure,raised_only_after_sustained_window,not_suppressed_by_lone_success_blip}` + `chain_all_failed_now_semantics` green |
| AC-ALARM-081 | PASS | `observe_chain_records_{derives_chain_outcome_but_not_provider_streak,any_success_clears_chain_outcome}` green (parity assertion for the function EXISTS) + OR-OBS2-3 recorded above |
| AC-ALARM-082 | PASS | `grep -n 'MissedTickBehavior' src/alarm/reconciler.rs` → `Skip` |
| G1 | PASS | `cargo test` exit 0; `cargo clippy --all-targets --all-features -- -D warnings` exit 0; `cargo fmt --check` exit 0 |
| G2 | PASS | rename stated in M1 commit body (`7ca92fb`) + `src/metrics/mod.rs` module-header rename note (`coin_`-prefix mapping) |

**Per-milestone commit SHAs (on `main`, local — unpushed):**

- M1: `7ca92fb` metric-name SSOT + describe/emit parity + drop tracked_markets ghost
- M2: `a49905f` alarm signal quality — sustained all_providers_down, doc parity, ticker Skip
- M3: `867a4f4` bounded shutdown drain + airtight shutdown-select arms
- M4: `26ac3db` exact readiness — bind-before-ready + flags-before-cache
- M5: `f345224` config diagnostics — warn on unparseable, fail-fast where dangerous
- M6: `13c044a` credential-safe DB connection via PgConnectOptions from parts
- M7: `b3f5814` generic supervisor + capped backoff + relay supervision
- M8: `02e9fda` dead-surface cleanup — dedup HeaderExtractor/OtelMakeSpan, delete dead API bootstrap
- M1-fixup: `9fb0875` reword rename note to satisfy AC-OBS-060 strict grep

**No new dependency:** `git diff b1e613a -- Cargo.toml Cargo.lock` → 0 lines.

**Note on DB-gated variants:** AC-OBS-064 (kill-the-DB-then-start live) and AC-OBS-073 (live special-char-password connect) optional DB-gated variants are DEFERRED per project convention (`DATABASE_URL=... cargo test -- --ignored --test-threads=1` on real Postgres) — the non-DB-gated primary paths PASS. No new `#[ignore]` DB tests were added by this SPEC.

## §E.3 Run-phase Audit-Ready Signal

```yaml
run_complete_at: 2026-07-27
run_commit_sha: 9fb0875   # HEAD (local, unpushed — push deferred to the orchestrator)
run_status: PASS
ac_pass_count: 20         # AC-OBS-060..074 (15) + AC-ALARM-080/081/082 (3) + G1 + G2
ac_fail_count: 0
preserve_list_post_run_count: 3   # startup step ordering, 15 s grace sleep, zero-drop broadcast-before-drain — all preserved
l44_pre_commit_fetch: "git rev-list --count --left-right origin/main...HEAD → 0 10 (clean, local ahead by 10; no origin race)"
l44_post_push_fetch: not-performed (push deferred to the orchestrator per run-phase scope)
new_warnings_or_lints_introduced: 0   # clippy --all-targets --all-features -D warnings clean; fmt clean
cross_platform_build:
  note: "Rust single-target (aarch64 cross-compiled at deploy via `cross`); no Go build tags. `cargo build` + `cargo check --all-targets --all-features` exit 0."
total_run_phase_files: 12   # metrics/mod.rs, db/upserts.rs, collectors/{mod,live_poller}.rs, alarm/{registry,reconciler}.rs, main.rs, health/mod.rs, config.rs, listener.rs, telemetry/mod.rs, api/mod.rs, db/{pool,mod}.rs + tests/alarm_docs_parity.rs
m1_to_mN_commit_strategy: "9 per-milestone commits M1..M8 + 1 M1 doc-fixup, direct to main (Route A Hybrid Trunk, Tier M); status draft→in-progress on M1 (7ca92fb)"
baseline_test_delta: "lib 661→681 (+20 tests); bin 8→9 (net: +4 run-phase, −3 dup HeaderExtractor moved to telemetry); alarm_docs_parity 3→5; pre-existing 80 lib + 16 db_integration ignored (DB-gated, unchanged)"
```

## §E.4 Sync-phase Audit-Ready Signal

```yaml
sync_complete_at: 2026-07-27
sync_commit_sha: pending-backfill-obs002-sync
sync_status: PASS
b12_self_test_a: "grep -c 'SPEC-OBS-002' CHANGELOG.md → 0 (pre-emission), 1 (post-emission)"
b12_self_test_b: "acceptance.md AC-ID count (grep -oE '\\*\\*AC-[A-Z]+-[0-9]+\\*\\*|\\*\\*G[0-9]\\*\\*') = 20; CHANGELOG entry cites 20"
b12_self_test_c: "all 15 file paths + tests/alarm_docs_parity.rs verified via ls before commit — all present"
changelog_entry_position: "CHANGELOG.md [Unreleased] > Fixed, immediately above the SPEC-API-005 entry"
frontmatter_status_transitions:
  spec_md: "in-progress -> completed (single sync commit; updated: unchanged 2026-07-27, same-day close)"
  plan_md: "no frontmatter block (plan-phase artifact, no status field)"
  acceptance_md: "no frontmatter block (plan-phase artifact, no status field)"
  progress_md: "no frontmatter block; this §E.4 entry is the sync-phase signal"
mx_tag_validation:
  status: PASS
  inventory: "metrics/mod.rs QUOTE/CANDLE_INSERT_DURATION_SECONDS consts ANCHOR+REASON+NOTE(rename)+SPEC; collectors/mod.rs run_supervised ANCHOR+REASON+WARN(healthy-reset)+REASON+SPEC; main.rs shutdown sequence ANCHOR+REASON+WARN(3 tasks)+REASON+SPEC, pool.close/telemetry.shutdown WARN+REASON+SPEC; health/mod.rs check_readiness WARN+REASON+SPEC; config.rs Tier ANCHOR (pre-existing) + NOTE(warn-vs-fail-fast split)+SPEC; listener.rs pre-existing WARN+REASON+SPEC (F-39 retry now matches doc); alarm/reconciler.rs pre-existing ANCHOR+REASON + WARN+REASON; telemetry/mod.rs ANCHOR+REASON+SPEC + NOTE(OtelMakeSpan placement)+SPEC(OBS-002 REQ-OBS-074)"
  gaps: "none — all plan.md §MX targets present and well-formed; every WARN/ANCHOR carries @MX:REASON"
docs_sync:
  observability_docs: "no separate metrics-catalogue doc found outside src/ (docs/ has only alarms.md, prediction-research.md — neither references metric names); the src/metrics/mod.rs module-header catalogue (already updated in run-phase, includes the operator-visible rename note) is the canonical catalogue — no additional doc update required"
canary_compliance_check:
  applicable: false
  note: "SPEC-OBS-002 does not define a forward-looking policy that tests its own sync — no canary check applicable"
```

**Residual (non-blocking, deferred):** AC-OBS-064 and AC-OBS-073 optional DB-gated variants (kill-the-DB-then-start relay retry; live special-character-password connect against real Postgres) remain deferred per project convention (`DATABASE_URL=... cargo test -- --ignored --test-threads=1`). Non-DB-gated primary paths for both PASS; this does not block `completed` per the close-to-completed rationale (all ACs' PRIMARY paths pass, unlike the DB-gated-is-primary precedent in SPEC-PROV-002/003).

## §F Phase 4 Mode Selection

- Input parameters: tier=M; scope≈11 source files; domains=observability/lifecycle/config/alarm (single language: Rust); file language mix=100% Rust; concurrency benefit=LOW (coding-heavy, not research).
- Mode evaluation:
  - Mode 1 trivial — not selected (multi-file semantic change).
  - Mode 2 background — not selected (write work, must block).
  - Mode 3 agent-team — RETIRED, never selected.
  - Mode 4 parallel — not selected (coding-heavy, not multi-domain research; Anthropic coding-task parallelism caveat).
  - Mode 5 sub-agent — SELECTED (default for coding-heavy work; sequential per-milestone manager-develop).
  - Mode 6 workflow — not selected (not ≥30 files, not a single uniform mechanical transform; semantic brownfield fixes).
- Decision: sub-agent
- Justification: Coding-heavy brownfield defect/maintainability work across a single language with milestone dependencies (M7 run_supervised before folded relay supervision) is the canonical Mode 5 case; Anthropic's coding-task parallelism caveat directs sequential sub-agent over parallel/workflow fan-out. Implementation Kickoff Approval passed (autonomous progression) before this selection.
- Kickoff: Implementation Kickoff Approval = APPROVED (run now, autonomous), 2026-07-27.
