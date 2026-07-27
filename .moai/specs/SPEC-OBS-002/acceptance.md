# SPEC-OBS-002 — Acceptance Criteria

> Each AC is observable and testable. Test framework: `cargo test` (unit + integration).
> Most ACs are unit-level (no live DB). DB-gated variants are marked; the primary
> verification for every AC is the non-DB-gated path unless stated otherwise.
> AC IDs map 1:1 to requirements; global gates are G1-G2.

## AC ↔ REQ Traceability

| AC | REQ | Finding | DB-gated? |
|----|-----|---------|-----------|
| AC-OBS-060 | REQ-OBS-060 | F-38 | No |
| AC-OBS-061 | REQ-OBS-061 | F-38 | No |
| AC-OBS-062 | REQ-OBS-062 | F-49 | No |
| AC-OBS-063 | REQ-OBS-063 | F-46 | No |
| AC-OBS-064 | REQ-OBS-064 | F-39 | No (unit); optional DB-gated variant |
| AC-OBS-065 | REQ-OBS-065 | F-46 | No |
| AC-OBS-066 | REQ-OBS-066 | F-41 | No |
| AC-OBS-067 | REQ-OBS-067 | F-41 | No |
| AC-OBS-068 | REQ-OBS-068 | F-47 | No |
| AC-OBS-069 | REQ-OBS-069 | F-43 | No |
| AC-OBS-070 | REQ-OBS-070 | F-44 | No |
| AC-OBS-071 | REQ-OBS-071 | F-45 | No |
| AC-OBS-072 | REQ-OBS-072 | F-45 | No |
| AC-OBS-073 | REQ-OBS-073 | F-40 | No (unit options assertion); optional DB-gated connect |
| AC-OBS-074 | REQ-OBS-074 | F-49 | No |
| AC-ALARM-080 | REQ-ALARM-080 | F-42 | No |
| AC-ALARM-081 | REQ-ALARM-081 | F-48 | No |
| AC-ALARM-082 | REQ-ALARM-082 | F-42/F-48 | No |
| G1 | all | quality gates | No |
| G2 | REQ-OBS-060 | F-38 operator-visibility | No |

## Per-AC Criteria

- **AC-OBS-060** — Metric-name SSOT. A shared `pub const` for each persistence-latency metric exists in `src/metrics/mod.rs`; `describe_all()` and every emitter reference the same const. Canonical names are exactly `quote_insert_duration_seconds` and `candle_insert_duration_seconds`. Verify: `grep -n 'quote_insert_duration_seconds\|candle_insert_duration_seconds' src/metrics/mod.rs src/db/upserts.rs` shows both sites referencing the const (not string literals), and no `coin_quote_insert_duration_seconds` / `coin_candle_insert_duration_seconds` literal remains anywhere (`grep -rn 'coin_quote_insert\|coin_candle_insert' src/` → 0).
- **AC-OBS-061** — Describe/emit parity. An explicit parity unit test asserts every described metric name has an emitter reference via the shared const and vice versa; the test is green. Because unit tests emit THROUGH the shared consts, describe/emit drift is structurally impossible.
- **AC-OBS-062** — `tracked_markets` ghost removed. `grep -rn 'tracked_markets' src/` returns 0 matches (describe entry, module-catalogue row, unit test, and the `live_poller.rs:665` reference all removed/reconciled). `tracked_coins` gauge is unchanged and still described.
- **AC-OBS-063** — Backoff grows and caps. A deterministic-crash future run under `run_supervised` produces restart intervals that grow exponentially and cap at the configured ceiling; a healthy-run period resets the backoff to the initial delay. Verified by a unit test asserting the sequence of restart delays.
- **AC-OBS-064** — Relay initial-connect retry. A relay whose initial connect/`listen()` fails retries with capped backoff rather than permanently returning; once the DB is available the relay comes up. Verified by an unreachable-port / injected-failure unit test following the `migrate_with_retry` test pattern. Additionally, the `src/listener.rs:54` doc comment is asserted to match the implemented retry behavior (the F-39 doc/code drift is closed) — verified by inspection that the doc describes the bounded-retry behavior now implemented, not the old permanent-return behavior. (Optional DB-gated variant: kill-the-DB-then-start against live Postgres.)
- **AC-OBS-065** — Single supervisor. Exactly one `run_supervised(name, registry, shutdown, make_future)` exists; the four former `run_supervised_{live_poller,queue_worker,backfill_worker,reconciler}` functions are gone (`grep -rn 'run_supervised_' src/` → 0). Log severity is identical between the panic arm and the error arm (asserted or inspected).
- **AC-OBS-066** — Bounded shutdown. A wedged (never-completing) worker future cannot extend shutdown beyond `drain_secs`; shutdown returns bounded by `drain_secs` via `tokio::time::timeout`. `pool.close()` and `telemetry::shutdown()` run on BOTH the drain-completed and drain-timed-out paths. Verified by a unit/integration test with a stub supervisor future.
- **AC-OBS-067** — Broadcast before drain, grace preserved. Workers receive the shutdown broadcast before the drain wait begins, and the 15 s endpoint-removal grace sleep remains upstream of the broadcast. Verified by an ordering test (or inspection of the shutdown sequence) asserting: grace sleep → broadcast → drain wait.
- **AC-OBS-068** — No busy-spin on dropped sender. When the shutdown watch sender is dropped without sending `true`, every select arm (listener, worker loops, reconciler) breaks instead of hot-looping on the immediate `Err`. Verified by a unit test that drops the sender and asserts the loop terminates promptly (no spin).
- **AC-OBS-069** — Bind before ready. Readiness is not-ready until after the API `TcpListener::bind` (and relay spawn); only `axum::serve` follows `set_ready()`. Verified via `HealthState::for_test` (or equivalent) asserting the ready flag is false until bind ordering completes. The documented Step 10-11-external startup ordering is unchanged.
- **AC-OBS-070** — 503 immediately on shutdown even with warm cache. After `set_shutting_down()`, `check_readiness` returns 503 immediately even when the 2 s readiness cache is warm (populated). Verified by a unit test that primes the cache with a 200, calls `set_shutting_down()`, and asserts the next `check_readiness` is 503.
- **AC-OBS-071** — Warn on unparseable env. A present-but-unparseable env value emits a `tracing::warn!` naming the variable and the fallback used. Verified by a unit test capturing the tracing output (or asserting the warn path is taken) for a garbage value.
- **AC-OBS-072** — Fail-fast on dangerous unparseable. A present-but-unparseable value for a dangerous-to-mis-set variable (pacer cooldown) causes fail-fast (error/panic at config load) rather than a silent default. Verified by a unit test asserting the error path.
- **AC-OBS-073** — Special-character password connects. The DB connection is built from `sqlx::postgres::PgConnectOptions` from parts; a password containing URL-significant characters (`@ / : # %` or spaces) yields correct connect options (host/username/password preserved, not re-parsed). Primary verification: a unit-level assertion on the constructed `PgConnectOptions` fields for a special-character password. The `DATABASE_URL` override path is exercised (still honored). (Optional DB-gated variant: actual connect against live Postgres with a special-character password.)
- **AC-OBS-074** — No dead surface. Exactly one `HeaderExtractor` and one `OtelMakeSpan`, both in `src/telemetry/`; the private `main.rs` copy is gone (`grep -n 'struct HeaderExtractor\|HeaderExtractor' src/main.rs` → 0 definition). `start_api_server` is deleted (`grep -rn 'start_api_server' src/` → 0). The build compiles and the OTel span/header behavior is unchanged (existing telemetry tests green).
- **AC-ALARM-080** — Sustained `all_providers_down`. The Critical alarm is raised only when the whole chain has failed continuously for the sustained window (timestamp-based `last_all_failed_at` / `last_chain_success_at` via `sustained_*`), and is NOT flipped by a single coin's failure among successes nor suppressed by a lone success mid-outage. Verified by a unit test driving a mixed success/failure sequence across the window boundary and asserting the alarm state.
- **AC-ALARM-081** — Doc matches behavior. `observe_chain_records`' doc comment matches its implementation; the resolution states which of doc/code is authoritative per REQ-ALARM-020, and whether the `provider-unreachable` streak counts non-`Network` failures. PASS requires ALL of: (a) the `alarm_docs_parity` test is added or extended to cover `observe_chain_records` doc/behavior parity and is green (NOT merely "left green" — a parity assertion for this function must exist); AND (b) OR-OBS2-3 (whether the per-provider `provider-unreachable` streak counts non-`Network` failures) is resolved and its decision recorded in `progress.md` per REQ-ALARM-020. If OR-OBS2-3 is unresolved, this AC is NOT PASS — return a blocker.
- **AC-ALARM-082** — Reconciler ticker `Skip`. The reconciler ticker uses `MissedTickBehavior::Skip` (`grep -n 'MissedTickBehavior' src/alarm/reconciler.rs` shows `Skip`, not `Burst`/default). Consistent with `live_poller`.
- **G1** — Quality gates. `cargo test` exits 0; `cargo clippy --all-targets --all-features -- -D warnings` exits 0 (no NEW warnings vs the pre-flight baseline); `cargo fmt --check` exits 0.
- **G2** — Operator-visibility of the rename. The metric rename (`coin_*` → canonical) is documented: stated in the run-phase commit body AND in the observability docs / metrics-catalogue module header. Verified by inspecting the commit body and the docs diff.

## Given / When / Then Scenarios

### Scenario 1 — Describe/emit parity (AC-OBS-060, AC-OBS-061)
- **Given** the metrics module defines shared name consts and unit tests emit through them,
- **When** the describe/emit parity test runs,
- **Then** every described metric has a matching emitter reference and no emitter uses an undescribed name, and no `coin_*`-prefixed persistence-latency name exists anywhere in `src/`.

### Scenario 2 — Bounded shutdown with a wedged worker (AC-OBS-066, AC-OBS-067)
- **Given** the shutdown sequence runs the 15 s endpoint grace sleep, then broadcasts shutdown to workers, then waits with `timeout(drain_secs, supervisor)`, and one worker future never completes,
- **When** shutdown is triggered,
- **Then** the drain returns bounded by `drain_secs` (does not block indefinitely), and `pool.close()` and `telemetry::shutdown()` both run afterward.

### Scenario 3 — 503 on shutdown with a warm readiness cache (AC-OBS-070)
- **Given** the readiness cache holds a fresh 200 result and `set_shutting_down()` is then called,
- **When** `check_readiness` is invoked before the 2 s cache expires,
- **Then** it returns 503 (the shutting-down atomic is consulted before the cache fast-path).

### Scenario 4 — Flappy alarm no longer flips on a single sample (AC-ALARM-080)
- **Given** the chain has one coin failure among many successes within the sustained window,
- **When** the reconciler samples the alarm signal,
- **Then** `all_providers_down` is NOT raised; it raises only after continuous whole-chain failure across the window, and a lone mid-outage success does not suppress a genuinely sustained outage.

### Scenario 5 — Special-character DB password (AC-OBS-073)
- **Given** the DB password contains `@`, `/`, `:`, `#`, `%`, or a space,
- **When** the connection options are built from parts via `PgConnectOptions`,
- **Then** the constructed options carry the exact host/username/password (no URL re-parse corruption), and the `DATABASE_URL` override path, when set, is still honored.

## Edge Cases

- Deterministic-crash future restarting under the supervisor: backoff must cap (no unbounded growth) AND reset after a healthy run (no permanent max-delay after one healthy cycle).
- Shutdown sender dropped by a panicking orchestrator before sending `true`: every arm breaks (AC-OBS-068) — no hot loop, no CPU spin.
- Relay initial connect fails N times then succeeds: relay comes up (AC-OBS-064) — the failure does not permanently disable cross-replica delivery (REQ-API-148).
- Config value present but empty string vs present-but-garbage: both take the warn path (AC-OBS-071); the dangerous subset (pacer cooldown) fails fast (AC-OBS-072).
- `tracked_coins` gauge must remain after `tracked_markets` removal (AC-OBS-062) — do not remove both.

## Definition of Done

- [ ] AC-OBS-060 through AC-OBS-074 all PASS (15 OBS ACs).
- [ ] AC-ALARM-080, AC-ALARM-081, AC-ALARM-082 all PASS (3 ALARM ACs).
- [ ] G1 quality gates green: `cargo test`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check` all exit 0.
- [ ] G2: metric rename stated in commit body + observability docs (operator-visible).
- [ ] No new dependency added (`git diff Cargo.toml Cargo.lock` adds 0 packages).
- [ ] Scope confined to the `plan.md §F` file-touch map; no unrelated files modified.
- [ ] Open Items OR-OBS2-1..5 resolved (constants/window/authoritative-source recorded in progress.md) or returned as a blocker.
- [ ] In-change doc updates landed (metrics catalogue header, `listener.rs` doc, `observe_chain_records` doc) — not deferred.
