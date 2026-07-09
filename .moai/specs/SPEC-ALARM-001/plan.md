# Implementation Plan — SPEC-ALARM-001 (Alarm Center Integration)

Contracts: [../SPEC-PROV-001/spec.md](../SPEC-PROV-001/spec.md) (`ProviderError`,
`build_chain`, `AttemptRecord`, chain fetchers), [../SPEC-SCHED-001/spec.md](../SPEC-SCHED-001/spec.md)
(`spawn_workers` supervision, `collection_queue`/`backfill_chunks`),
[../SPEC-OBS-001/spec.md](../SPEC-OBS-001/spec.md) (readiness DB-ping, statelessness NFRs),
[../SPEC-DB-001/spec.md](../SPEC-DB-001/spec.md) (`PgPool`, `upstream_request_pacer`).
External: `../alarm-center/api/alarm-center.yaml` (fixed contract; `POST /api/v1/alarms`
optional `timeoutSeconds` server-side auto-clear, alarm-center.yaml:34-37).
Methodology: greenfield TDD — pure-function unit tests for fingerprint/mapping/desired-active-set
computation (and the optional active→inactive fast-clear transition detection), a mock-server
test for the client contract, and `#[ignore]` DB-integration tests for the level-state SQL
derivations.

## Technical Approach

A new module (`src/alarm/`) provides three units:

1. **`AlarmClient`** — holds one shared `reqwest::Client` in an `Arc` (there is NO shared
   client today; each provider builds its own — `binance.rs:33`, `bitstamp.rs:69`,
   `coingecko.rs:160` — so this introduces the first). `raise(&AlarmSpec)` POSTs
   `/api/v1/alarms` and ALWAYS sets `timeoutSeconds = ALARM_TTL_SECS` so the server
   installs/refreshes an auto-clear deadline; omitting it would revert the alarm to
   never-expire (alarm-center.yaml:34-37), so it is never omitted (REQ-ALARM-052/053).
   `clear(&str)` POSTs `/api/v1/alarms/{fingerprint}/clear` and is retained ONLY as the
   optional Critical/Error fast-path (REQ-ALARM-014). There is no startup read-back of open
   alarms — the server owns what is currently active, so the reconciler needs no reported-set
   to seed. Each method loops up to `ALARM_CENTER_MAX_RETRIES` with a per-attempt
   `ALARM_CENTER_TIMEOUT_MS` timeout, treats `clear` 404 as success, and on exhaustion logs
   `error!` and returns without propagating — the swallow-error contract mirrors the existing
   `let _ = pacer::signal_cooldown(...)` idiom (`coingecko.rs:904`, `binance.rs:292`). The
   startup fatal raise is the one exception to the retry policy: a single attempt with zero
   retries, still carrying `timeoutSeconds = ALARM_TTL_SECS` (REQ-ALARM-035).

2. **`HealthRegistry`** — a cheap shared `Arc` structure updated at error sites and read
   by the reconciler. It holds only the state that cannot be re-derived from the database:
   - `providers: {name -> (last_success_at, consecutive_network_failures)}` updated where
     `ProviderError::Network` is produced/observed (`providers/mod.rs:186`) and on any
     provider success — feeds REQ-ALARM-020;
   - `all_providers_down: flag + since` set when a chain fetch records every
     `AttemptRecord.outcome == Failure` (`providers/mod.rs:431`, and the inline chains at
     `live_poller.rs:345-360`, `collection_queue.rs` dispatch), cleared on any chain
     success — feeds REQ-ALARM-022;
   - `worker_restarts: {worker -> timestamped restart events}` (a decaying event set, NOT
     a monotonic counter) pushed in the supervisor restart arms
     (`collectors/mod.rs:192-208, 243-260, 294-311`); the reconciler counts only events
     within `ALARM_WORKER_CRASHLOOP_WINDOW_SECS` so the rate-based signal recovers
     (REQ-ALARM-034 — a monotonic counter could never fall below threshold) — feeds
     REQ-ALARM-034;
   - `upsert_failure_streak` incremented on `sqlx::Error` from upsert callers (e.g.
     `live_poller.rs:310`) and reset on success — feeds REQ-ALARM-042.
   Everything else is derived at sweep time from durable/system state (see Reconciler). The
   registry drives DETECTION only (whether a condition is active, i.e. whether to keep
   refreshing its alarm); it is never a clear mechanism, so losing it (e.g. on restart) can
   no longer strand an alarm — recovery is server-driven via TTL.

3. **Reconciler worker (near-stateless)** — a sweep loop keyed on
   `ALARM_RECONCILE_INTERVAL_SECS`. Each sweep it builds a `DesiredState` (the set of active
   `(fingerprint, AlarmSpec)`), then `raise()`s every active condition with
   `timeoutSeconds = ALARM_TTL_SECS` (a create on first raise, a dedup heartbeat that
   refreshes the deadline thereafter). For a condition that is no longer active it simply
   STOPS calling `raise()`; the Alarm Center auto-clears the fingerprint once the TTL lapses.
   There is no reported-set to own and no startup seeding: an alarm a previous instance
   raised auto-expires if this instance does not refresh it. The reconciler MAY keep a small
   in-memory `previously_active` set used ONLY to spot active→inactive transitions and, for
   Critical/Error severities, fire an immediate optional fast-clear so those resolve without
   waiting for TTL (REQ-ALARM-014); losing that set does not affect correctness because TTL
   still clears everything. Restart-on-panic supervision is added to `spawn_workers` exactly
   like the existing three workers, and the reconciler is only spawned when `ALARM_CENTER_URL`
   is set; on shutdown it simply stops between sweeps — no mass-clear, since every alarm it
   raised carries a TTL and auto-expires unless refreshed (REQ-ALARM-018). On its first sweep
   it may also best-effort fast-clear `crypto-collector:startup-config-error` (Critical),
   though that alarm auto-expires by TTL regardless.

Desired-state derivation per sweep:
- **Registry-derived** (in-memory): provider-unreachable, all-providers-down,
  worker-crash-looping, upsert-failure-streak.
- **Pacer-derived** (`SELECT ... FROM upstream_request_pacer`): rate-limited/cooldown,
  credit-exhausted, missing-pacer-row (configured providers minus present rows).
- **Queue/backfill-derived** (windowed `SELECT count(*) WHERE status='failed' AND
  updated_at > now() - '{window}'` / stall check): queue-failures and backfill-failed are
  windowed rates (`'failed'` is terminal and never reset — a cumulative count would latch
  DETECTION; see B1), backfill-stalled stays a level check on pending-not-advancing.
- **DB/pool-derived**: db-unreachable (reuse the readiness `SELECT 1` with a sustained
  timer), db-pool-exhausted (`pool.size()` / `pool.num_idle()` sampled with a saturation
  timer).
- **Coins-derived** (`SELECT count(*) FROM tracked_coins WHERE ... stale`): coins-stalled
  (single aggregated alarm).

The pure core (fingerprint construction, condition→`AlarmSpec` mapping, the `DesiredState`
active-set computation, and the active→inactive transition detection that drives the optional
fast-clear) is factored out as pure functions so it is unit-testable without a DB or HTTP —
mirroring how `pacer::pacer_decision` is a pure, separately-tested core.

## Files (anticipated; confirmed in run phase)

| File | Change |
|---|---|
| `src/alarm/mod.rs` (new) | `AlarmClient` (`raise` always with `timeoutSeconds`; optional `clear` fast-path), `AlarmSpec`, `Severity`, fingerprint builders, condition→spec mapping (pure), module wiring. |
| `src/alarm/registry.rs` (new) | `HealthRegistry` (atomics/maps) + cheap update methods for error sites. |
| `src/alarm/reconciler.rs` (new) | Sweep loop, `DesiredState` active-set computation, raise-with-TTL/heartbeat, optional Critical/Error fast-clear on active→inactive transition; supervised runner. |
| `src/collectors/mod.rs` | Spawn + supervise the reconciler as a fourth worker (gated on `ALARM_CENTER_URL`); push timestamped restart events in the existing restart arms. |
| `src/main.rs` | Build `AlarmClient` + `HealthRegistry` when configured; inject into workers/state; fatal startup-config raise (carrying `timeoutSeconds`) around `build_chain` (REQ-ALARM-035). |
| `src/config.rs` | New free functions: `alarm_center_url()`, `alarm_center_api_key()`, `alarm_center_timeout_ms()`, `alarm_center_max_retries()`, `alarm_reconcile_interval_secs()`, `alarm_ttl_secs()` (default `ceil(2.5 * alarm_reconcile_interval_secs())`), plus the per-condition thresholds and the two failure-window settings `alarm_queue_failed_window_secs()` / `alarm_backfill_failed_window_secs()` (REQ-ALARM-050/051/052). |
| `src/providers/mod.rs`, `src/collectors/live_poller.rs`, `src/collectors/collection_queue.rs`, `src/db/upserts.rs` callers | Cheap registry updates at the network-error / chain-outcome / upsert-failure sites (no alarm-center calls). |
| `Cargo.toml` | (Confirm) `reqwest` is already a dependency; add nothing new unless a mock-server test crate (e.g. `wiremock`) is needed as a dev-dependency. |
| `docs/alarms.md` (new) | Operator-facing catalogue: overview (feature gate; TTL-driven self-clearing model — raise/heartbeat with a server-side auto-clear TTL, stop-refresh on recovery so alarms auto-expire, optional Critical/Error fast-clear; best-effort delivery; fingerprint scheme) + one entry per fingerprint (title/description, code/severity/component, fingerprint template, active-signal + clear trigger, governing env vars + defaults, remediation hint). Maintained in lockstep with the code condition→spec mapping (REQ-ALARM-070). |

## Milestones (priority-ordered, no time estimates)

### Milestone 1 — config + feature gate (Priority High)
- RED: `alarm_center_url()` returns `None` for unset/empty; the reconciler is not spawned
  and no alarm-center request is made when unset; `alarm_ttl_secs()` defaults to
  `ceil(2.5 * alarm_reconcile_interval_secs())` and exceeds the reconcile interval by the
  safety margin (REQ-ALARM-001/002/050/051/052).
- GREEN: config functions + gating in `main`/`spawn_workers`.

### Milestone 2 — AlarmClient contract (Priority High)
- RED (mock server): a raise POSTs the correct body with `sourceService=crypto-collector`
  AND `timeoutSeconds=ALARM_TTL_SECS` (never omitted); a new fingerprint (201) and a repeat
  (200) are both accepted; the optional fast-clear POSTs to the fingerprint path; a clear 404
  is treated as success; timeout + bounded retry then swallow-error-and-log on persistent
  failure; the startup fatal raise uses exactly one attempt with zero retries and still
  carries `timeoutSeconds`; no error propagates
  (REQ-ALARM-005/006/007/008/009/052/053/035).
- GREEN: `AlarmClient` with shared `reqwest::Client`, `raise` (always with `timeoutSeconds`),
  optional `clear` fast-path, retry/timeout, error swallowing, and the zero-retry
  startup-raise path.

### Milestone 3 — fingerprint scheme + condition mapping + operator docs (Priority High)
- RED (pure): every condition maps to the exact fingerprint/component/severity/code in the
  Condition Catalogue; templated fingerprints substitute provider/worker names correctly
  (REQ-ALARM-003/004, catalogue rows 1–13).
- GREEN: pure fingerprint builders + condition→`AlarmSpec` mapping.
- DOCS: author `docs/alarms.md` from the same catalogue — overview block (TTL self-clearing
  model) + one entry per fingerprint with all six required fields (REQ-ALARM-070). This is
  the source-of-truth pairing: any later change to a fingerprint/code/severity/threshold
  updates both the code mapping AND `docs/alarms.md` in the same change.

### Milestone 4 — desired-active-set + reconciler loop (Priority High)
- RED (pure + mock): given a new desired active set, every active condition is raised with
  `timeoutSeconds = ALARM_TTL_SECS`; a condition that drops out of the set is simply no
  longer raised (no clear call in the normal path) and relies on TTL expiry; unchanged sweeps
  produce no duplicates (server-side dedup); a Critical/Error condition observed transitioning
  active→inactive additionally fires one optional fast-clear; a raise dropped while still
  active is re-attempted next sweep; shutdown stops between sweeps and does NOT mass-clear
  (REQ-ALARM-011..018).
- GREEN: `DesiredState` computation + transition detection + reconciler runner + supervised
  spawn in `collectors/mod.rs`.

### Milestone 5 — health registry + Tier 1 conditions (Priority High)
- RED: registry counters update at the network-error/chain-outcome sites; provider
  unreachable (sustained), all-providers-down, provider rate-limited (cooldown),
  credit-exhausted each become active/clear correctly (REQ-ALARM-019/020/021/022/023).
- GREEN: `HealthRegistry` + Tier 1 desired-state derivations (registry + pacer table).

### Milestone 6 — Tier 2 conditions (Priority High)
- RED (`#[ignore]` DB): db-unreachable (sustained readiness fail), missing-pacer-row,
  windowed collection-queue-failures and backfill-failed (detection recovers once no new
  failures land in the window, terminal `'failed'` rows notwithstanding), backfill-stalled,
  worker-crash-looping (windowed restart events fall below threshold), startup-config fatal
  raise carrying `timeoutSeconds` (REQ-ALARM-030..035).
- GREEN: windowed SQL-derived desired state + restart-event wiring + the `main` fatal raise.

### Milestone 7 — Tier 3 conditions (Priority Medium)
- RED (`#[ignore]` DB): coins-stalled aggregation, db-pool-exhausted sampling,
  upsert-failure-streak (REQ-ALARM-040/041/042).
- GREEN: coin-staleness query + pool sampling + upsert-streak wiring.

### Milestone 8 — multi-replica + NFR review (Priority Medium)
- Tests/review: identical fingerprints from two reconcilers dedup to one alarm, which stays
  active while either replica keeps refreshing and expires only when both stop; the normal
  path issues no explicit clears so there are no clear races (a stray Critical/Error
  fast-clear is self-healed by the other replica's next re-raise); alarm delivery never
  blocks a collector (structural review that error sites only touch the registry).
  (REQ-ALARM-060, and REQ-ALARM-007 revalidated end-to-end.)

## Risks

- **Raised-but-never-cleared — now structurally eliminated (was the highest risk).** The
  entire clear-path defect class the reconciler was originally built to dodge is removed by
  moving clearing to the server: every raise/heartbeat carries `timeoutSeconds =
  ALARM_TTL_SECS` (REQ-ALARM-052/053) and recovery is simply "stop refreshing", so a
  condition that recovers — or a raise that is dropped, or a whole instance that restarts —
  can no longer strand an alarm. The former latch traps are correspondingly defused: a
  **terminal-state cumulative count** (`'failed'` rows never reset) is still windowed so
  DETECTION recovers (REQ-ALARM-032/033), and restart signal is still a **windowed event
  set** not a monotonic counter (REQ-ALARM-019/034) — but even if detection mis-fires, TTL
  clears the alarm; and the **restart-wipes-the-reported-set** orphan concern (former auditor
  M1) disappears entirely, because there is no reported-set and no startup seed. The one
  remaining discipline is TTL sizing: `ALARM_TTL_SECS` must stay above
  `ALARM_RECONCILE_INTERVAL_SECS` by the safety margin (REQ-ALARM-052) so a single slow or
  missed sweep cannot let an alarm expire and flap.
- **Startup-config-error timing (OR-ALARM-1 — RESOLVED).** `build_chain` fails fast
  (`providers/mod.rs:357-363`) before the reconciler is spawned. The fatal best-effort raise
  in `main` is bounded (short timeout, zero retries) so a slow/unreachable alarm center never
  delays a crash-looping pod, and it carries `timeoutSeconds` so the alarm auto-clears once a
  healthy start stops re-raising it — no in-process clear path before exit is required
  (REQ-ALARM-035).
- **Delivery must never degrade collection (REQ-ALARM-007).** The client must be
  swallow-error and off the hot path; only the reconciler and the one fatal startup raise
  ever touch the network. Error sites must do O(1) registry updates only.
- **Reconciler DB load (OR-ALARM-4).** Several `count(*)`/staleness queries per interval.
  At the default 30 s interval this should be negligible, but must be confirmed against
  production table sizes (`collection_queue`, `backfill_chunks`, `tracked_coins`); widen
  the interval or add indexes if measurable. Note: widening the interval also requires
  raising `ALARM_TTL_SECS` to preserve the safety margin (REQ-ALARM-052).
- **Multi-replica heartbeat multiplication (OR-ALARM-5 — DOWNGRADED, REQ-ALARM-060).**
  With N replicas every fingerprint is heartbeated N times per interval. Because the normal
  path has no explicit clears (recovery is stop-refresh + server TTL), there are no
  clear/re-raise races and no need for a single-writer election; the only residual is
  duplicate heartbeat traffic, which server-side dedup collapses to one alarm. A single-writer
  refinement is a possible later optimisation purely for load, not correctness — not in this
  SPEC.
- **Threshold false-positives.** Defaults (provider-unreachable 300 s, queue/backfill
  failed 10, coin-staleness 900 s / 5 coins, pool-saturation 60 s, upsert-streak 20) are
  first guesses; too-tight thresholds flap alarms, too-loose ones hide incidents. Tunable
  via env; defaults documented in `spec.md` REQ-ALARM-051.
- **All-providers-down granularity (OR-ALARM-2).** A single flag collapses distinct
  capability outages into one alarm; confirm whether per-capability fingerprints are
  wanted before shipping.

## Open Decisions (carried from spec.md)

OR-ALARM-1 (startup timing — RESOLVED by the TTL-carrying fatal raise; residual is only the
run-phase scope question of whether to fold other fatal config failures into
`STARTUP_CONFIG_ERROR`), OR-ALARM-2 (all-down granularity), OR-ALARM-3 (coin-staleness
thresholds), OR-ALARM-4 (reconciler DB load), OR-ALARM-5 (multi-replica heartbeat volume —
DOWNGRADED to a load-only question; no clear races remain), OR-ALARM-6 (auth header format).
Resolve or explicitly defer with user sign-off before Definition of Done.

- **OR-ALARM-7 (OPTIONAL, nice-to-have — not a hard REQ):** a docs-parity test that asserts
  every fingerprint/code in the code condition catalogue has a matching entry in
  `docs/alarms.md` (and vice versa), mirroring the project's existing
  `commands_audit_test.go`-style enforcement (see `.claude/rules/moai/development/coding-standards.md`
  "Thin Command Pattern"). Cheap to add if the catalogue is enumerable in code; keeps the
  operator doc from silently drifting. Proposed, not mandated — REQ-ALARM-070 makes the
  lockstep obligation normative regardless of whether this test is added.

## Definition of Done

- Whole feature is a no-op when `ALARM_CENTER_URL` is unset; identical behaviour to today.
- `AlarmClient` uses one shared `reqwest::Client`, sends `timeoutSeconds = ALARM_TTL_SECS`
  on EVERY raise (never omitted — REQ-ALARM-052/053), enforces timeout + bounded retry,
  treats clear-404 as success, and never propagates errors or blocks a caller.
- Reconciler spawned + supervised alongside the existing workers; near-stateless; each sweep
  raises/heartbeats every active condition with the TTL and stops refreshing recovered ones
  so the server auto-clears them; unchanged sweeps create no duplicates; no mass-clear on
  shutdown; no startup seeding of open alarms.
- Recovery clears via TTL expiry after the last refresh for Warning severities, and
  immediately via the optional fast-clear for Critical/Error severities; a dropped fast-clear
  still falls back to TTL.
- All 14 fingerprints (13 conditions; backfill contributes 2 — `backfill-failed` and
  `backfill-stalled`) raise with the correct fingerprint/severity/component and clear on
  recovery (TTL, plus fast-clear for Critical/Error), including across a reconciler/pod
  restart (a pre-restart orphan auto-expires via TTL with no startup action).
- Multi-replica behaviour verified: dedup collapses identical fingerprints; the alarm stays
  active while any replica refreshes and expires when all stop; no clear races.
- `docs/alarms.md` exists with the overview block (TTL self-clearing model) and an entry for
  all 14 fingerprints (each with the six required fields), consistent with the code
  catalogue; the lockstep maintenance obligation is stated (REQ-ALARM-070).
- All EARS REQ-ALARM-001..070 (excluding the superseded 018a) covered by tests (pure unit /
  mock-server / `#[ignore]` DB split matching the project's existing test conventions);
  REQ-ALARM-070 is verified by a docs-existence-and-completeness check.
- Open items OR-ALARM-1..6 resolved or explicitly deferred with user sign-off (OR-ALARM-1
  resolved, OR-ALARM-5 downgraded).
