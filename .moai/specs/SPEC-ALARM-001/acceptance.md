# Acceptance Criteria — SPEC-ALARM-001 (Alarm Center Integration)

Each scenario maps to EARS requirements in `spec.md`. Pure scenarios (fingerprint/mapping/
active-set/transition) use plain unit tests; client scenarios use a mock HTTP server (e.g.
`wiremock`); level-state scenarios use the `#[ignore]` DB-integration path
(`DATABASE_URL=… cargo test -- --ignored`). See Test Strategy at the end.

The lifecycle model is server-driven TTL auto-clear: every raise/heartbeat carries
`timeoutSeconds = ALARM_TTL_SECS`; recovery = the reconciler simply stops refreshing so the
Alarm Center auto-clears the fingerprint once the TTL lapses; Critical/Error severities may
additionally fast-clear immediately on the observed active→inactive transition, and Warning
severities rely on TTL expiry alone.

## Scenario 1 — Feature fully disabled when unconfigured (REQ-ALARM-002)

- **Given** `ALARM_CENTER_URL` is unset (or empty)
- **When** the process starts and runs its workers
- **Then** the reconciler is never spawned, no request is ever sent to the alarm center,
  and observable behaviour is identical to a build without this feature.

## Scenario 2 — Feature active when configured (REQ-ALARM-001/010)

- **Given** `ALARM_CENTER_URL` set to a non-empty value
- **When** the process starts
- **Then** the `AlarmClient` is built and the reconciler is spawned as a fourth supervised
  worker alongside `live_poller`/`collection_queue`/`backfill`.

## Scenario 3 — Constant sourceService and deterministic fingerprints (REQ-ALARM-003/004)

- **Given** any condition becomes active
- **When** its alarm is raised
- **Then** the payload carries `sourceService="crypto-collector"` and a fingerprint of the
  form `crypto-collector:{condition-slug}[:{provider-or-component}]` that is identical on
  every re-raise of the same condition-and-target.

## Scenario 3b — Every raise carries timeoutSeconds (REQ-ALARM-052/053)

- **Given** any raise the system sends — an initial create, a heartbeat re-raise of a
  still-active condition, or the fatal startup-config raise
- **When** the request body is inspected
- **Then** it always includes `timeoutSeconds = ALARM_TTL_SECS` (never omitted), so the
  server sets/refreshes an auto-clear deadline rather than reverting the alarm to
  never-expire; and `ALARM_TTL_SECS` is sized above `ALARM_RECONCILE_INTERVAL_SECS` (default
  `ceil(2.5 * interval)`).

## Scenario 4 — Raise dedup: new vs repeat (REQ-ALARM-013/015/017)

- **Given** a mock alarm center
- **When** a condition is raised for a brand-new fingerprint, then re-raised unchanged on
  the next sweep
- **Then** the first raise is accepted as a create (201) and the re-raise is accepted as a
  dedup heartbeat (200 bumping `occurrenceCount`/`lastSeen` and refreshing the TTL deadline);
  no duplicate alarm is created.

## Scenario 5 — Recovery clears via stop-refresh + server TTL (Warning path) (REQ-ALARM-015/018)

- **Given** a Warning-severity condition that was active in the previous sweep
- **When** the next sweep finds it no longer active
- **Then** the reconciler simply stops re-raising that fingerprint (it issues NO explicit
  clear), and the Alarm Center auto-clears it once the TTL lapses — i.e. within
  approximately `ALARM_TTL_SECS` of the last heartbeat.

## Scenario 5b — Critical/Error fast-clear on transition, TTL fallback (REQ-ALARM-014)

- **Given** a Critical- or Error-severity condition (e.g. `all-providers-down` or
  `db-unreachable`) that was active and is observed transitioning to inactive
- **When** the reconciler detects the active→inactive transition
- **Then** it issues an immediate optional `clear()` for that fingerprint so the alarm
  resolves without waiting for TTL expiry; **and given** that fast-clear is dropped or lost
  in delivery, the alarm still auto-clears via TTL after the last refresh — confirming the
  fast-clear is a non-correctness-critical latency optimisation, not the clear mechanism.

## Scenario 6 — Fast-clear of a never-raised fingerprint is a no-op (REQ-ALARM-008)

- **Given** the alarm center returns 404 for a fast-clear (the fingerprint was never raised,
  or has already TTL-expired)
- **When** the reconciler issues that clear
- **Then** it is treated as success, not an error, and does not log at `error!` level.

## Scenario 7 — Delivery resilience: retry then log, never crash/block (REQ-ALARM-006/007)

- **Given** the alarm center is unreachable or timing out
- **When** the reconciler issues a raise or an optional fast-clear
- **Then** the client retries up to `ALARM_CENTER_MAX_RETRIES` with an
  `ALARM_CENTER_TIMEOUT_MS` per-attempt timeout, then logs an `error!` and returns; the
  reconciler and all collectors continue unaffected (no block, no panic). A dropped raise or
  fast-clear cannot strand an alarm because clearing is server-driven via TTL.

## Scenario 8 — Dropped raise / dropped fast-clear self-heal (REQ-ALARM-016)

- **Given** a raise (of a still-active condition) or an optional fast-clear that failed to
  deliver on one sweep
- **When** the next sweep re-derives desired state
- **Then** a still-active condition is simply re-raised next sweep, and a dropped fast-clear
  falls back to TTL expiry — the missed operation self-heals with no compensating action and
  no bespoke retry bookkeeping.

## Scenario 9 — Shutdown just stops, no mass-clear (REQ-ALARM-018)

- **Given** a reconciler with active alarms
- **When** a shutdown signal is received
- **Then** the reconciler simply stops (between sweeps) and does NOT clear its active alarms.
  Every alarm it raised carries a TTL, so a still-true condition stays visible (another
  replica keeps refreshing it, or it auto-expires after restart if truly recovered) and a
  condition that recovered during shutdown auto-clears via TTL — no mass-clear or
  finish-sweep clear logic is involved.

## Scenario 9a — Cross-restart orphan auto-expires via TTL (REQ-ALARM-015/018)

- **Given** a fingerprint the previous reconciler instance had raised (with a TTL) whose
  condition recovered during a reconciler/pod restart gap
- **When** the fresh instance starts, finds that condition inactive, and therefore never
  refreshes the fingerprint — performing NO startup read-back of open alarms and issuing NO
  explicit clear
- **Then** the Alarm Center auto-clears the fingerprint once its TTL lapses, so no alarm is
  stranded open across the restart; and **conversely**, had the condition still been active,
  the fresh instance's sweeps would re-raise and refresh it, keeping it visible — correctness
  holds either way with no startup seeding.

## Scenario 10 — Provider unreachable, sustained (REQ-ALARM-020)

- **Given** one provider producing only `ProviderError::Network` failures with no success
- **When** the failure persists past `ALARM_PROVIDER_UNREACHABLE_SECS`
- **Then** a Warning alarm is active with component `providers`, code `PROVIDER_UNREACHABLE`,
  fingerprint `crypto-collector:provider-unreachable:{provider}`; it heartbeats while the
  outage lasts and, once that provider next succeeds so the condition goes inactive, the
  reconciler stops refreshing and the alarm auto-clears via TTL (Warning: no fast-clear).

## Scenario 11 — Provider rate-limited / cooldown active (REQ-ALARM-021)

- **Given** `upstream_request_pacer.cooldown_until` for a provider is in the future
- **When** the reconciler sweeps
- **Then** a Warning alarm is active with component `pacer`, code `PROVIDER_RATE_LIMITED`,
  fingerprint `crypto-collector:provider-rate-limited:{provider}`; once the cooldown has
  expired the reconciler stops refreshing and it auto-clears via TTL.

## Scenario 12 — All providers down (REQ-ALARM-022)

- **Given** a chain fetch where every `AttemptRecord.outcome == Failure` sets the
  chain-all-down flag
- **When** the reconciler sweeps
- **Then** a Critical alarm is active with component `providers`, code `ALL_PROVIDERS_DOWN`,
  fingerprint `crypto-collector:all-providers-down`; on the first subsequent chain success
  the condition goes inactive and, being Critical, the reconciler fast-clears it immediately
  (with TTL as the fallback if the fast-clear is dropped).

## Scenario 13 — Provider credit exhausted (REQ-ALARM-023)

- **Given** a provider with `credits_used >= credit_limit` in `upstream_request_pacer`
- **When** the reconciler sweeps
- **Then** an Error alarm is active with component `pacer`, code `PROVIDER_CREDIT_EXHAUSTED`,
  fingerprint `crypto-collector:provider-credit-exhausted:{provider}`; once the credit window
  resets it fast-clears immediately (Error severity), with TTL as the fallback.

## Scenario 14 — Database unreachable (REQ-ALARM-030)

- **Given** the readiness DB-ping (`SELECT 1`) failing for at least
  `ALARM_DB_UNREACHABLE_SECS`
- **When** the reconciler sweeps
- **Then** a Critical alarm is active with component `db`, code `DB_UNREACHABLE`,
  fingerprint `crypto-collector:db-unreachable`; once the ping succeeds it fast-clears
  immediately (Critical), with TTL as the fallback. (Note: while the DB is unreachable the
  reconciler cannot run its SQL-derived checks; it still derives db-unreachable from the ping
  and delivers over HTTP.)

## Scenario 15 — Missing pacer row (REQ-ALARM-031)

- **Given** a configured provider with no `upstream_request_pacer` row
- **When** the reconciler sweeps (comparison, or an observed `AcquireSlotError::NotFound`)
- **Then** an Error alarm is active with component `pacer`, code `MISSING_PACER_ROW`,
  fingerprint `crypto-collector:missing-pacer-row:{provider}`; once the row exists it
  fast-clears immediately (Error), with TTL as the fallback.

## Scenario 16 — Recent collection-queue failures, windowed (REQ-ALARM-032)

- **Given** `count(*) FROM collection_queue WHERE status='failed' AND updated_at > now() -
  ALARM_QUEUE_FAILED_WINDOW_SECS` at or above `ALARM_QUEUE_FAILED_THRESHOLD`
- **When** the reconciler sweeps
- **Then** a Warning alarm is active with component `collection_queue`, code
  `COLLECTION_QUEUE_FAILURES`, fingerprint `crypto-collector:collection-queue-failures`;
  and **when** no new failures land for `ALARM_QUEUE_FAILED_WINDOW_SECS` (so the windowed
  count drops below the threshold even though the terminal `'failed'` rows persist), the
  condition goes inactive, the reconciler stops refreshing, and the alarm auto-clears via TTL
  — verifying detection does not latch on the monotonic cumulative count.

## Scenario 17a — Recent backfill-chunk failures, windowed (REQ-ALARM-033)

- **Given** `count(*) FROM backfill_chunks WHERE status='failed' AND updated_at > now() -
  ALARM_BACKFILL_FAILED_WINDOW_SECS` ≥ `ALARM_BACKFILL_FAILED_THRESHOLD`
- **When** the reconciler sweeps
- **Then** a Warning `crypto-collector:backfill-failed` alarm (component `backfill`) is
  active; and **when** no new failures land for `ALARM_BACKFILL_FAILED_WINDOW_SECS`, the
  reconciler stops refreshing and it auto-clears via TTL even though the terminal `'failed'`
  rows remain (no latch on cumulative count).

## Scenario 17b — Backfill stalled, level condition (REQ-ALARM-033)

- **Given** pending backfill chunks that have not advanced for ≥ `ALARM_BACKFILL_STALL_SECS`
- **When** the reconciler sweeps
- **Then** a Warning `crypto-collector:backfill-stalled` alarm (component `backfill`) is
  active, independent of the `backfill-failed` fingerprint; once progress resumes or no
  pending chunks remain the reconciler stops refreshing and it auto-clears via TTL.

## Scenario 18 — Worker crash-looping (REQ-ALARM-019/034)

- **Given** a worker whose timestamped restart events within
  `ALARM_WORKER_CRASHLOOP_WINDOW_SECS` reach `ALARM_WORKER_CRASHLOOP_THRESHOLD`
- **When** the reconciler sweeps
- **Then** an Error alarm is active with component `collectors`, code
  `WORKER_CRASH_LOOPING`, fingerprint `crypto-collector:worker-crash-looping:{worker}`;
  once older restart events age out of the window and the in-window count falls below the
  threshold the condition goes inactive and, being Error, the reconciler fast-clears it
  (TTL fallback) — verifying the signal is a decaying event set, not a monotonic counter that
  could never clear.

## Scenario 19 — Startup config error fatal raise + self-heal (REQ-ALARM-035)

- **Given** `PROVIDERS` contains an unknown name and `ALARM_CENTER_URL` is set
- **When** `build_chain` fails fast at startup
- **Then** the process makes a single best-effort blocking raise — exactly one attempt,
  bounded by `ALARM_CENTER_TIMEOUT_MS`, with zero retries, carrying
  `timeoutSeconds = ALARM_TTL_SECS` so a crash-looping pod is not delayed — of a Critical
  `crypto-collector:startup-config-error` (component `config`) before exiting non-zero; and
  **given** a subsequent successful start, the alarm auto-clears via TTL once the healthy
  process stops re-raising it (and the reconciler may fast-clear it on its first sweep).

## Scenario 20 — Coins stalled, aggregated (REQ-ALARM-040)

- **Given** the number of `tracked_coins` stale beyond `ALARM_COIN_STALENESS_SECS` reaches
  `ALARM_COINS_STALLED_THRESHOLD`
- **When** the reconciler sweeps
- **Then** a single aggregated Warning alarm is active with component `live_poller`, code
  `COINS_STALLED`, fingerprint `crypto-collector:coins-stalled`, and the stalled count in
  `details` (no per-coin alarm spam); once the count falls below the threshold the reconciler
  stops refreshing and it auto-clears via TTL.

## Scenario 21 — DB pool exhaustion (REQ-ALARM-041)

- **Given** the pool saturated (`num_idle()==0 && size()==MAX_CONNECTIONS`) continuously
  for at least `ALARM_DB_POOL_SATURATION_SECS`
- **When** the reconciler samples the pool
- **Then** an Error alarm is active with component `db`, code `DB_POOL_EXHAUSTED`,
  fingerprint `crypto-collector:db-pool-exhausted`; once idle connections are available it
  fast-clears immediately (Error), with TTL as the fallback.

## Scenario 22 — DB upsert-failure streak (REQ-ALARM-042)

- **Given** the consecutive upsert-failure streak reaching `ALARM_UPSERT_FAILURE_STREAK`
- **When** the reconciler sweeps
- **Then** an Error alarm is active with component `db`, code `DB_UPSERT_FAILURES`,
  fingerprint `crypto-collector:db-upsert-failures`; once a successful upsert resets the
  streak the condition goes inactive and, being Error, the reconciler fast-clears it (TTL
  fallback).

## Scenario 23 — Idempotent unchanged sweeps (REQ-ALARM-017)

- **Given** a set of active conditions unchanged across consecutive sweeps
- **When** the reconciler runs repeatedly
- **Then** no duplicate alarms are created — each unchanged condition only re-raises its
  existing fingerprint (server-side dedup, refreshing the TTL deadline), and no spurious
  clears occur.

## Scenario 24 — Multi-replica dedup and self-heal (REQ-ALARM-060)

- **Given** two replicas each running a reconciler
- **When** both raise the same fingerprint, and one replica (seeing the condition recovered
  locally) issues a Critical/Error fast-clear while the other still observes it active
- **Then** the alarm center dedups identical fingerprints to one alarm; the alarm stays
  active while ANY replica keeps refreshing it and expires only once ALL stop; the stray
  fast-clear is self-healed by the still-active replica's next-sweep re-raise, converging to
  the true global state. Because the normal path issues no explicit clears, there are no
  clear races and no leader election is required.

## Scenario 25 — Operator documentation is complete and in-sync (REQ-ALARM-070)

- **Given** the repository
- **When** `docs/alarms.md` is inspected
- **Then** it exists, opens with the overview block (documenting the `ALARM_CENTER_URL`
  feature gate, the reconciler's TTL-driven self-clearing lifecycle — raise/heartbeat with a
  server-side auto-clear TTL, stop-refresh on recovery, optional Critical/Error fast-clear —
  the best-effort delivery semantics, and the
  `crypto-collector:{condition-slug}[:{provider-or-component}]` fingerprint scheme), and
  contains one entry for every one of the 14 alarm fingerprints
  (`provider-unreachable`, `provider-rate-limited`, `all-providers-down`,
  `provider-credit-exhausted`, `db-unreachable`, `missing-pacer-row`,
  `collection-queue-failures`, `backfill-failed`, `backfill-stalled`,
  `worker-crash-looping`, `startup-config-error`, `coins-stalled`, `db-pool-exhausted`,
  `db-upsert-failures`), each carrying its title/description, `code`, `severity`,
  `component`, fingerprint template, active-signal, clear trigger, governing env-var
  thresholds/windows with defaults, and an operator remediation hint; **and** the
  documented code/severity/component/fingerprint for each entry matches the code condition
  catalogue exactly (no drift).

## Scenario 26 — TTL sizing prevents flap on a slow/missed sweep (REQ-ALARM-052)

- **Given** `ALARM_TTL_SECS` sized above `ALARM_RECONCILE_INTERVAL_SECS` by the safety
  margin (default `ceil(2.5 * interval)`)
- **When** a single reconcile sweep is delayed or missed (so one heartbeat is skipped) while
  the condition is still active
- **Then** the alarm's server-side deadline does NOT lapse before the next sweep refreshes
  it, so there is no spurious expire→re-raise flap; a shorter TTL that did not exceed the
  interval by the margin would flap under the same skipped sweep.

## Test Strategy

- **Unit-testable (no DB, no network) — preferred pure functions:** fingerprint
  construction and condition→`AlarmSpec` mapping (Scenarios 3, 10–22 mapping portions); the
  `DesiredState` active-set computation and the active→inactive transition detection that
  decides raise/heartbeat/optional-fast-clear (Scenarios 4, 5, 5b, 8, 23); the
  `ALARM_CENTER_URL` gate (Scenario 1); TTL sizing (Scenario 26). Modelled on
  `pacer::pacer_decision`'s pure-core pattern.
- **Mock HTTP server (`wiremock` or equivalent, dev-dependency):** the `AlarmClient`
  contract — request body/path including `timeoutSeconds` on every raise, 201-vs-200 dedup,
  clear-404-as-success, timeout + bounded retry + swallow-error, the zero-retry startup raise
  carrying `timeoutSeconds`, no error propagation (Scenarios 3b, 4, 5b, 6, 7, 19). Runs
  under plain `cargo test` (no DB).
- **`#[ignore]` DB-integration (`DATABASE_URL=… cargo test -- --ignored`):** the level-state
  derivations that query real tables — cooldown/credit/missing-pacer-row against
  `upstream_request_pacer`, windowed queue/backfill failure counts (asserting detection
  recovers after the window elapses with no new failures — so the reconciler stops refreshing
  and the alarm TTL-clears — despite persistent terminal `'failed'` rows) and backfill stall,
  coin staleness, pool saturation, and the sustained-timer conditions (Scenarios 11, 13–17b,
  20–22). Matches the project's existing no-DB / DB split
  (`model_serde`/`migration_files` vs `db_integration`).
- **Registry-driven conditions** (provider-unreachable, all-providers-down,
  worker-crash-looping, upsert-streak — Scenarios 10, 12, 18, 22) are unit-testable by
  driving the `HealthRegistry` directly then asserting the derived desired state, with the
  timer/threshold boundaries exercised via injected clock values.
- **Startup fatal raise (Scenario 19)** is exercised via a mock server + an invalid
  `PROVIDERS` value, asserting a single bounded raise carrying `timeoutSeconds` then non-zero
  exit.
- **Documentation parity (Scenario 25)** is a repo/static check (no DB, no network):
  assert `docs/alarms.md` exists, has the overview block, and carries a complete entry for
  each of the 14 fingerprints matching the code catalogue. If the OPTIONAL parity test
  (OR-ALARM-7) is implemented, it enumerates the code catalogue and asserts a matching
  `docs/alarms.md` entry per fingerprint (mirroring the `commands_audit_test.go` pattern);
  otherwise this is a review/checklist item.

## Quality Gate / Definition of Done

- [ ] Unconfigured = full no-op; configured = reconciler spawned + supervised (1, 2).
- [ ] Constant `sourceService`, deterministic fingerprints, `timeoutSeconds` on every raise,
      dedup on re-raise (3, 3b, 4, 23).
- [ ] Recovery clears via stop-refresh + server TTL for Warning severities, and via the
      optional immediate fast-clear (TTL fallback) for Critical/Error; clear-404 no-op;
      dropped raise/fast-clear self-heal; shutdown just stops with no mass-clear; a
      cross-restart orphan auto-expires via TTL with no startup seeding (5, 5b, 6, 8, 9, 9a).
- [ ] Delivery resilient: bounded retry then log, never blocks/panics a collector (7).
- [ ] All 14 fingerprints (13 conditions; backfill = `backfill-failed` + `backfill-stalled`)
      raise with correct fingerprint/severity/component and clear on recovery via the
      severity-appropriate path (10–22, incl. 17a/17b).
- [ ] No re-latch traps: windowed queue/backfill failure signals let detection recover after
      the window despite terminal `'failed'` rows so the alarm TTL-clears (16, 17a); windowed
      restart events let the crash-loop alarm clear (18).
- [ ] Startup-config fatal raise (one attempt, zero retries, carrying `timeoutSeconds`) +
      TTL/first-sweep self-heal clear (19).
- [ ] Multi-replica dedup + self-heal verified; alarm stays active while any replica
      refreshes, expires when all stop, no clear races (24).
- [ ] TTL sized above the reconcile interval by the safety margin so a slow/missed sweep does
      not flap the alarm (26).
- [ ] `docs/alarms.md` exists with the overview block (TTL self-clearing model) and a
      complete, in-sync entry for every one of the 14 fingerprints (six required fields
      each); lockstep maintenance obligation honoured (25).
- [ ] All EARS REQ-ALARM-001..070 (excluding the superseded 018a) covered by tests via the
      unit / mock / `#[ignore]` DB split above.
- [ ] Open items OR-ALARM-1..6 resolved or explicitly deferred with user sign-off
      (OR-ALARM-1 resolved, OR-ALARM-5 downgraded).
