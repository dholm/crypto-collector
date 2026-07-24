# SPEC-PROV-002 — Progress

Lifecycle: plan → run → sync. Status: **implemented** (run-phase + sync-phase complete;
plan-audit PASS-WITH-DEBT 0.87 + Implementation Kickoff Approval granted; DB-gated ACs
deferred to a live-Postgres verification pass — `completed` transition held pending that
run; see §E.4 for rationale).

## §E.1 Plan-phase Audit-Ready Signal

Plan-phase artifacts complete: `spec.md`, `plan.md`, `acceptance.md`, `progress.md`.

- Tier: **M** (upper-edge M, L-adjacent on file count). ~8 files under change —
  `src/providers/{transport(new),coingecko,binance,bitstamp,mod}.rs`, `src/pacer/mod.rs`,
  `src/config.rs`, `src/main.rs` — + colocated pure/wiremock and DB-gated tests. Net new
  logic sits in the M band (300–1000 LOC); the ~10-endpoint migration inflates the diff but
  is behavior-preserving under the existing wiremock coverage, and the decision surface is
  small (4 settled decisions). The research role is served by the repo-level
  `research/idiomatic-rust.md` §6 Category C, so a Tier-L design.md/research.md would largely
  duplicate it. Consistent with the 7-phase roadmap convention (behavioral-defect phases are
  Tier M; Phase 2 SPEC-CANDLE-002 was Tier M). Rationale recorded here for retrospective
  audit; a plan-auditor first-pass score regression would trigger a tier-up review.
- Requirements: **REQ-PROV-050..064** (15 GEARS requirements), extending SPEC-PROV-001's
  REQ-PROV-0xx block (non-colliding — SPEC-PROV-001 occupies REQ-PROV-001..045). Mapped 1:1
  to AC-PROV-050/053/055/058/060/063 + AC-PROV-QG.
- Findings: F-10 (High), F-11 (High), F-12 (Medium), F-13 (Medium), F-14 (Medium), F-15
  (Medium), F-17 (Low), F-18 (Low), F-19 (Informational) — Category C of
  `research/idiomatic-rust.md` §6 (lines 138–180). F-16 (per-provider chain pacing) is
  explicitly OUT → Phase 7.
- Decisions settled: **D1** helper module placement = new `src/providers/transport.rs`;
  **D2** shared `build_client()` + env-tunable timeouts (defaults total 30 s / connect 10 s,
  envs `PROVIDER_HTTP_TIMEOUT_SECS` / `PROVIDER_HTTP_CONNECT_TIMEOUT_SECS`); **D3** F-13 fork
  = sleep the full computed wait + surface backlog via `warn!`/metric (NOT truncate — reject
  a `Backlogged` variant); **D4** new pacer variant = `AcquireSlotError::Contended(String)`
  for the blocked-path race, `NotFound` reserved for the genuinely-absent row.
- Open items: **0** — no `[NEEDS CLARIFICATION]` markers remain.
- Development mode: TDD (brownfield). Independent of Phases 1–2 (both COMPLETE); **Phase 4
  depends on this phase** (shared helpers + client constructor are Phase 4's landing site).
- Route: A (Hybrid Trunk main-direct, Tier M default) — commit-direct-to-main per
  CLAUDE.local.md; no feature branch, no per-phase PR.
- plan_complete_at: 2026-07-24
- plan_status: audit-ready

### Documented Debt (carry to run-phase)

Two run-phase testability seams surfaced in plan-audit (PASS-WITH-DEBT 0.87), recorded here
for the run-phase implementer (not plan-audit-blocking):

- **D1 — REQ-PROV-062 `Contended` pure-core testability.** The `Contended` positive path is
  not deterministically reproducible via a live race. Run-phase SHOULD extract a pure
  `classify_blocked(row, now) -> AcquireSlotError` core for the blocked-path diagnostic
  (lapsed-block row present → `Contended`; row absent → `NotFound`), add a deterministic pure
  AC over it, and mirror the `sleep_plan` / `is_transient` pure-core pattern. Scenario 5c
  (DB-gated) remains; the deterministic `Contended` assertion lives in the pure core.
- **D2 — REQ-PROV-060 metric assertion.** Add a metric-increment assertion for
  `pacer_backlog_wait_exceeded_total{provider}` (increments when `sleep_plan` returns
  `backlog_exceeded == true`), OR document the metric wiring as inspection-verified if the
  metric registry is not unit-observable in the test harness.

## §E.2 Run-phase Evidence

Development mode: TDD (brownfield). Cycle_type=tdd. Verification commands run in the
sandbox (no live Postgres). Gate exit codes are verbatim from the run below.

### Quality Gate (AC-PROV-QG) — verbatim

| Gate | Command | Actual Output | Status |
|------|---------|---------------|--------|
| Format | `cargo fmt --check` | `fmt-check-exit=0` | PASS |
| Lint | `cargo clippy --all-targets --all-features -- -D warnings` | `clippy-exit=0` | PASS |
| Test | `cargo test` | `test result: ok. 630 passed; 0 failed; 70 ignored` (+ integration suites 8/3/2/20/12 pass, 15 doc-ignored; 0 failed anywhere) | PASS |
| No new dependency | `git diff --stat Cargo.toml Cargo.lock` | (empty — no change) | PASS |
| No new f64 in monetary path | `git diff … \| grep '^+' \| grep -w f64` | `(no new f64 added)` | PASS |
| Provider trait surface unchanged | `git diff src/providers/mod.rs \| grep 'async fn (fetch_\|search_\|supports\|name)'` | `(no trait signature lines changed)` | PASS |

### Per-AC matrix (AC → REQ)

| AC / REQ | Verification | Actual Output | Status |
|----------|--------------|---------------|--------|
| AC-PROV-050 · REQ-PROV-050/051/052 | `grep -rn "Err(ProviderError::RateLimited) => {" src/providers/`; transport get_json tests; pre-existing wiremock suites | grep → only `transport.rs:103` (no inline block outside the frame); `get_json_*` 4 tests PASS; all pre-existing CoinGecko/Binance/Bitstamp wiremock tests still green | PASS |
| AC-PROV-053 · REQ-PROV-053 (slot consumed) | DB-gated `db_acquire_slot_absent_is_not_found_present_is_ok` + search/tickers routed through `paced()` | Wiremock: client 429→RateLimited so `paced()` consumes a slot + signals cooldown; slot-advance is DB-observable → deferred | PASS-WITH-DEBT |
| AC-PROV-053 · REQ-PROV-054 (429 signals cooldown, empty preserved) | `search_coins_client_429_returns_rate_limited`, `fetch_coin_tickers_client_429_returns_rate_limited`; `*_degrades_to_empty_on_non_success` (503) | 429→`Err(RateLimited)` PASS; non-429→`Ok(vec![])`+warn PASS; trait boundary degrades ALL errors to empty; cooldown-row-set is DB-observable → deferred | PASS-WITH-DEBT |
| AC-PROV-055 · REQ-PROV-055/056/057 | `hanging_upstream_errors_at_timeout_instead_of_hanging`, `build_client_attaches_user_agent`, `resolve_positive_secs_guards_zero_and_unparseable_to_default`; greps | timeout→`is_timeout()` + surfaces as `Network` PASS; UA header PASS; zero-guard PASS (unset/`"0"`/unparseable→default, `"45"`→45); `Client::builder` only in `transport.rs`; timeouts via shared constructor | PASS |
| AC-PROV-058 · REQ-PROV-058/059 | `is_transient_status_matrix` (pure) | 408/425/429/500..=599 → true; 400/401/403/404/409/410/422 → false (RED against pre-fix 404=true); RateLimited true, CreditExhausted/Parse/NotSupported false | PASS |
| AC-PROV-060 · REQ-PROV-060 (full-wait + backlog) | `sleep_plan_*` (4 pure), `record_backlog_wait_increments_provider_labelled_counter` (D2 metric) | full wait (120s→120_000ms, no 60s truncation) + `backlog_exceeded=true`; ≤ceiling→false; exactly-ceiling→false; ≤0→zero; metric `pacer_backlog_wait_exceeded_total{provider="coingecko"}` increments | PASS |
| Sub-5b · REQ-PROV-061 (monotonic cooldown) | `signal_cooldown` GREATEST + DB-gated `db_signal_cooldown_is_monotonic_never_shortens` | SQL `GREATEST(COALESCE(cooldown_until,'epoch'),$2)` implemented + compiles; monotonicity is DB-observable → deferred | PASS-WITH-DEBT |
| Sub-5c · REQ-PROV-062 (Contended vs NotFound) | `classify_blocked_*` (4 pure, D1) + DB-gated `db_acquire_slot_absent_is_not_found_present_is_ok` | pure core: lapsed-block→Contended, absent→NotFound, active-cooldown→Cooldown, credit→CreditExhausted (all PASS, deterministic); DB absent→NotFound path deferred | PASS (pure) / PASS-WITH-DEBT (DB half) |
| AC-PROV-063 · REQ-PROV-063/064 (startup validation) | `missing_pacer_rows` + `main.rs` Step 8a wiring + DB-gated `db_missing_pacer_rows_detects_absent_member` | `missing_pacer_rows` implemented; wired after `migrate_with_retry` success, before `set_ready` (a missing row → best-effort alarm + non-zero exit, readiness never flips); build passes; startup path is DB-observable → deferred | PASS-WITH-DEBT |

### Preserve-list invariants (post-run)

- Files touched: `src/providers/{transport(new),coingecko,binance,bitstamp,mod}.rs`, `src/pacer/mod.rs`, `src/config.rs`, `src/main.rs` — exactly the plan.md §13 PRESERVE set. `coinbase.rs`/`kraken.rs` (stubs, no reqwest client) untouched; no `src/api/*`, migration, `Cargo.toml`, `src/metrics/mod.rs`, or unrelated file touched.
- @MX: `@MX:ANCHOR` on `transport::paced`/`get_json` + `transport::build_client`; `@MX:WARN` updated on `acquire_slot` (full-wait, no 60s truncation) + added on `signal_cooldown` (monotonic GREATEST); `@MX:NOTE` on the `main.rs` startup pacer-row check. All ANCHOR/WARN carry `@MX:REASON`.

## §E.3 Run-phase Audit-Ready Signal

```yaml
run_complete_at: 2026-07-24
run_commit_sha: 380150fac06d543d7139bfc17553789f587d98b9   # feat(SPEC-PROV-002): M1
run_status: pass-with-debt
ac_pass_count: 5        # AC-PROV-050, AC-PROV-055, AC-PROV-058, AC-PROV-060, AC-PROV-QG (Sub-5c pure core PASS)
ac_fail_count: 0
ac_pass_with_debt_count: 4   # AC-PROV-053 (REQ-053+054), Sub-5b, Sub-5c DB half, AC-PROV-063 — all deferred to live-Postgres
preserve_list_post_run_count: 8   # 7 modified + 1 new (transport.rs), all within plan.md §13
l44_pre_commit_fetch: n/a   # Route A Hybrid Trunk main-direct; orchestrator controls push (15 unpushed Phase 1-2 commits precede)
l44_post_push_fetch: n/a    # push deferred to orchestrator/user
new_warnings_or_lints_introduced: 0   # clippy -D warnings exit 0
cross_platform_build:
  note: single-target aarch64 deployment; no GOOS matrix (Rust). cargo build exit 0.
total_run_phase_files: 8
m1_to_mN_commit_strategy: logically-grouped single implementation commit (M1-M6 interdependent — transport.rs used by all provider migrations; pacer changes used by transport); split commits would create non-compiling intermediate trees
db_gated_tests_written:
  count: 4
  marker: "#[tokio::test] #[ignore]"
  run_convention: "DATABASE_URL=... cargo test -- --ignored --test-threads=1"
  tests:
    - pacer::tests::db_signal_cooldown_is_monotonic_never_shortens   # REQ-PROV-061
    - pacer::tests::db_acquire_slot_absent_is_not_found_present_is_ok  # REQ-PROV-062 (DB half)
    - pacer::tests::db_missing_pacer_rows_detects_absent_member       # REQ-PROV-063/064
  note: >
    3 net-new DB-gated tests for SPEC-PROV-002 (the pre-existing pacer #[ignore] suite is
    unchanged). The Contended positive path is NOT deterministically reproducible via a live
    race, so its deterministic assertion lives in the pure classify_blocked test (D1
    resolved); the DB half asserts absent→NotFound + present→Ok.
gaps:
  - DB-gated ACs (REQ-PROV-053/054 slot+cooldown row, REQ-PROV-061 monotonic, REQ-PROV-062 DB half, REQ-PROV-063/064 startup) NOT executed — sandbox has no Postgres; deferred to a live-Postgres `--test-threads=1` run (owner: orchestrator/sync-gate).
  - run_commit_sha is a placeholder pending post-commit backfill.
  - Metric `pacer_backlog_wait_exceeded_total` is emitted without a `describe_counter!` HELP line (registration lives in metrics/mod.rs, which is outside the PRESERVE list); the counter auto-registers on first use and renders correctly (D2 test confirms).
```

## §E.4 Sync-phase Audit-Ready Signal

Sync-phase close performed by manager-docs. **No live Postgres was available in this
environment** — the 3 net-new DB-gated tests (`db_signal_cooldown_is_monotonic_never_shortens`,
`db_acquire_slot_absent_is_not_found_present_is_ok`, `db_missing_pacer_rows_detects_absent_member`)
plus the DB-observable halves of AC-PROV-053 (slot advance + cooldown row) and AC-PROV-063
(startup validation) were **not executed** here; §E.2/§E.3 already recorded them as
`PASS-WITH-DEBT`/deferred, and this sync-phase does not upgrade that classification. No
`cargo test`/`clippy`/`fmt` re-run was needed since no logic changed in this phase (one
doc-only `@MX:ANCHOR` comment added — see below); the sandbox gate evidence in §E.2 stands
unchanged.

### Documentation

- `CHANGELOG.md` `[Unreleased]` — SPEC-PROV-002 entry added (F-10/F-11/F-12/F-13/F-14/F-15/
  F-17/F-18/F-19, 15 REQs, 6 scenario ACs + AC-PROV-QG, DB-gated-deferred gap named). Placed
  before the pre-existing SPEC-CANDLE-002 entry (most-recent-first under `[Unreleased]`).
  Pre-emission grep `grep -c 'SPEC-PROV-002' CHANGELOG.md` returned `0` before the edit
  (no duplicate). All 8 file paths named in the entry verified to exist via `ls`/`[ -f ]`.
- No README.md exists at the project root and no dedicated config-doc surface exists beyond
  source rustdoc — confirmed via `ls` at repo root. The two new env vars
  (`PROVIDER_HTTP_TIMEOUT_SECS` default 30, `PROVIDER_HTTP_CONNECT_TIMEOUT_SECS` default 10,
  zero/unparseable-guarded to default) are documented in `src/providers/transport.rs` module
  doc-comment (`build_client`/`build_client_with` rustdoc) and `src/providers/mod.rs` §
  "Shared request path (SPEC-PROV-002)" module doc-comment — both were already complete from
  run-phase; no new doc file was invented, per Scope Discipline.
- `src/providers/transport.rs` rustdoc verified complete: module doc-comment names all three
  pieces (`build_client`, `paced`, `get_json`) with env-var/default/REQ-ID references;
  `build_client`/`build_client_with`/`paced`/`get_json` each carry a doc-comment. No gaps
  found — no augmentation needed beyond the @MX tag below.

### @MX validation (grep evidence)

```
$ grep -n "@MX" src/providers/transport.rs
38:// @MX:ANCHOR: [AUTO] build_client — the single provider reqwest::Client constructor
80:// @MX:ANCHOR: [AUTO] paced — the shared request-path frame (throttle + slot + 429 postlude)
121:// @MX:ANCHOR: [AUTO] get_json — the shared response epilogue (429/status/parse mapping)  [ADDED this sync]

$ grep -n "@MX" src/pacer/mod.rs | grep -E "acquire_slot|signal_cooldown|WARN"
236:// @MX:WARN: [AUTO] acquire_slot is the single fleet-wide egress governor; ... full-wait
      + backlog-observability text present (UPDATED in run-phase — verified accurate)
314:// @MX:WARN: [AUTO] signal_cooldown is monotonic — GREATEST never shortens ... (ADDED in
      run-phase — verified present, was previously untagged per spec.md §420)

$ grep -n "@MX" src/main.rs | grep pacer-row
235:// @MX:NOTE: [AUTO] pacer-row completeness check: after migrations succeed, before
      set_ready (present, run-phase)
```

**Finding**: run-phase correctly added/updated the `acquire_slot` `@MX:WARN`, the
`signal_cooldown` `@MX:WARN`, and the `main.rs` `@MX:NOTE`, exactly as spec.md § MX
Annotation Targets specified. spec.md additionally asked for `@MX:ANCHOR` on **both**
`transport::paced` **and** `transport::get_json` (fan_in ≥ 3 drift-prevention frame) —
run-phase added it only to `paced` and `build_client`; `get_json` was left untagged. This
sync-phase closed that gap: added `@MX:ANCHOR` on `get_json` (transport.rs:121, verified via
`grep -c "@MX:ANCHOR" src/providers/transport.rs` → `3`, at the `mx.yaml` `anchor_per_file: 3`
limit). Verified `cargo fmt --check` exit `0` and `cargo clippy --all-targets --all-features
-- -D warnings` exit `0` after the doc-only addition (no logic changed).

### Status transition — held at `implemented`, NOT promoted to `completed`

**Decision: `in-progress → implemented`, NOT `implemented → completed`.**

Rationale: per the sync-phase task instructions, a DB-gated-deferred SPEC MAY be classified
PASS-WITH-DEBT and closed to `completed` with the outstanding live-Postgres verification
recorded as close-out debt (the SPEC-CANDLE-002 precedent). I am declining that path here
and holding at `implemented` instead, because the DB-gated debt on THIS SPEC is materially
larger than the CANDLE-002 precedent it would be modeled on:

- CANDLE-002's DB-gated debt was **already resolved by the time of its close** (`updated`
  history shows 4 DB-gated integration tests ran green against live PostgreSQL 16 before the
  sync commit — see the `improvement-phases` memory: "Phase 1 = SPEC-SCHED-002 COMPLETE
  (sync ..., unpushed)"). This SPEC's DB-gated ACs, by contrast, have **never** been run
  against a live database at any point in its lifecycle — 3 net-new `#[ignore]` tests plus
  two DB-observable AC halves (F-10 slot/cooldown-row effect, F-15 startup validation) are
  ALL still untested against real Postgres.
- The deferred surface here is not a minor edge: F-10 (High) and F-15 (Medium) are two of
  the three highest-severity findings in this SPEC, and their DB-observable halves —
  `next_allowed_at` actually advancing, `cooldown_until` actually being set, a missing pacer
  row actually failing startup — are exactly the behavior the SPEC exists to fix. Pure-core
  and wiremock coverage confirms the Rust logic is correct, but does not confirm the SQL
  (`GREATEST(...)`, the gated `UPDATE ... WHERE`, the `SELECT ... WHERE provider = ANY($1)`)
  behaves as designed against a real Postgres planner/transaction semantics.
- Per `verification-claim-integrity.md` §1 ("no unobserved-verification-claim"), marking
  `completed` here would assert this SPEC's core defensive fixes were verified when 3 of 9
  findings' DB-observable behavior was never actually observed running.

Holding at `implemented` is the conservative, defensible choice: run-phase is genuinely
complete (all code written, all sandbox-verifiable ACs green, all @MX tags in place), but
"implemented" — not "completed" — accurately states that acceptance is not yet fully
verified. The frontmatter `status:` will be advanced to `completed` in a follow-up
sync-phase step once the live-Postgres run below executes and the 3 DB-gated tests +
2 DB-observable AC halves are confirmed PASS.

```yaml
sync_complete_at: 2026-07-24
sync_commit_sha: pending-backfill-sync-prov-002   # backfilled in a follow-up commit
sync_status: pass-with-debt-held-at-implemented
frontmatter_status_transition: "in-progress -> implemented"   # NOT -> completed
b12_self_test_a: "grep -c 'SPEC-PROV-002' CHANGELOG.md before edit == 0 (no duplicate)"
b12_self_test_b: "AC count: 6 scenario ACs (AC-PROV-050/053/055/058/060/063) + AC-PROV-QG, matches acceptance.md (SSOT)"
b12_self_test_c: "8 file paths in CHANGELOG entry verified via ls/[ -f ] — all exist"
mx_tags_added_this_sync: 1   # @MX:ANCHOR on transport::get_json (spec.md gap)
mx_tags_verified_unchanged: 3   # acquire_slot @MX:WARN, signal_cooldown @MX:WARN, main.rs @MX:NOTE
```

### Gaps (carried forward, unresolved by this sync)

- **DB-gated ACs still unexecuted**: AC-PROV-053 (REQ-PROV-053/054 slot advance + cooldown
  row), Sub-5b (REQ-PROV-061 monotonic cooldown, DB half), Sub-5c (REQ-PROV-062 `Contended`
  DB half), AC-PROV-063 (REQ-PROV-063/064 startup validation) — none executed against a live
  Postgres in this or any prior phase. Owner: next session with `DATABASE_URL` set, command
  `DATABASE_URL=... cargo test -- --ignored --test-threads=1`.
- **`sync_commit_sha` placeholder**: this commit's own SHA is unknown until after it lands;
  backfilled in a follow-up commit per the established SHA-placeholder-backfill pattern
  (`spec-frontmatter-schema.md` § SHA placeholder backfill exemption).
- **`completed` transition deferred**: frontmatter remains `status: implemented` pending the
  live-Postgres run above. A future sync-phase (or a dedicated close-out commit once the
  live-Postgres run passes) performs the `implemented → completed` transition.
