---
id: ALARM-001
version: 1.3.0
status: completed
created: 2026-07-08
updated: 2026-07-09
author: dholm
priority: high
issue_number: null
---

# SPEC-ALARM-001 — Alarm Center Integration

Integrates Crypto Collector with the external **Alarm Center** microservice so that
serious abnormal operational conditions raise alarms and are automatically cleared when
the condition recovers. Full lifecycle management (raise → heartbeat → clear) is a
first-class requirement: the design deliberately avoids the "raised but never cleared"
failure mode of scattered inline calls by centralising all alarm state in a single
periodic **reconciler** worker.

Consumers/contracts: [SPEC-PROV-001](../SPEC-PROV-001/spec.md) (`ProviderError`,
`build_chain`, chain outcomes), [SPEC-SCHED-001](../SPEC-SCHED-001/spec.md) (worker
supervision/`spawn_workers`, queue/backfill state), [SPEC-OBS-001](../SPEC-OBS-001/spec.md)
(readiness DB-ping, statelessness/multi-replica NFRs), [SPEC-DB-001](../SPEC-DB-001/spec.md)
(`PgPool`, `upstream_request_pacer`). External contract: Alarm Center OpenAPI at
`../alarm-center/api/alarm-center.yaml` (SPEC-ALARM-API-002) — treated as fixed, not
redesigned here.

## HISTORY

- 2026-07-08 (v1.3.0): Major simplification — adopted the Alarm Center's server-side TTL
  auto-clear (`POST /api/v1/alarms` optional `timeoutSeconds`, alarm-center.yaml:34-37,
  426-437, 512-524) in HYBRID form. Clearing is now server-driven: every raise/heartbeat
  carries `timeoutSeconds = ALARM_TTL_SECS`, and when a condition recovers the reconciler
  simply STOPS refreshing so the server auto-clears after the TTL lapses — this makes
  "raised but never cleared" structurally impossible. Explicit `clear()` is retained only
  as an OPTIONAL fast-path for Critical/Error severities on an observed active→inactive
  transition (Warning relies on TTL alone). New REQ-ALARM-052 (every raise includes the TTL;
  TTL sized above the reconcile interval by a safety margin) and REQ-ALARM-053 (never omit
  `timeoutSeconds`); new env var `ALARM_TTL_SECS` (default `ceil(2.5 *
  ALARM_RECONCILE_INTERVAL_SECS)` ≈ 75 s). Reworked REQ-ALARM-011/012/014/015/016/018/035/060.
  REMOVED REQ-ALARM-018a (startup `list_open()` reconciliation) — SUPERSEDED: a pre-restart
  orphan now auto-expires via TTL if the new instance does not refresh it, so the auditor's
  M1 orphan concern is structurally resolved without seeding. B1 windowing (032/033) and
  M2 windowed restart events (019/034) are KEPT for detection, with a note that detection
  imperfection can no longer strand an alarm (clearing is server-driven). Resolves OR-ALARM-1
  (fatal startup raise now carries a TTL, no in-process clear path needed) and downgrades
  OR-ALARM-5 (no explicit clears in the normal path → no clear races; leader election
  unnecessary).
- 2026-07-08 (v1.2.0): Added REQ-ALARM-070 — the project shall maintain `docs/alarms.md`
  as the canonical operator-facing catalogue of every alarm the service can raise, updated
  in lockstep with any alarm add/remove/change. Specifies per-entry content (title,
  description, code, severity, component, fingerprint template, active-signal, clear
  trigger, governing env vars + defaults, remediation hint) and a document overview
  (feature gate, reconciler self-clearing model, best-effort delivery, fingerprint scheme).
  Reflected as a deliverable/DoD item in plan.md (with an OPTIONAL parity-test open item)
  and a new acceptance scenario + quality-gate item.
- 2026-07-08 (v1.1.0): Plan-auditor fixes. Redefined `collection-queue-failures`
  (REQ-ALARM-032) and `backfill-failed` (REQ-ALARM-033 row 8a) as **windowed** failure-rate
  signals — `'failed'` is a terminal state that no production path resets, so a cumulative
  `count(*)` is monotonic and would latch the alarm forever (B1); added
  `ALARM_QUEUE_FAILED_WINDOW_SECS` / `ALARM_BACKFILL_FAILED_WINDOW_SECS`. Added
  REQ-ALARM-018a: startup reconciliation seeds the reported-set from the alarm center's
  currently-open alarms and clears orphans, so an alarm raised before a reconciler/pod
  restart still clears if the condition recovered during the restart gap (M1). Amended
  REQ-ALARM-019 so the registry holds timestamped/decaying restart **events** (not a
  monotonic counter), making the rate-based clear in REQ-ALARM-034 derivable (M2). Pinned
  the fatal startup-config raise to a single bounded attempt with zero retries
  (REQ-ALARM-035, m1). Reworded REQ-ALARM-009 so the auth header is optional/best-effort
  with the scheme left unspecified by the contract (m2). Disambiguated REQ-ALARM-018
  shutdown ("finish the in-flight sweep, then stop", m7).
- 2026-07-08 (v1.0.0): Initial greenfield SPEC. Alarm-center HTTP client (single shared
  `reqwest::Client`, per-request timeout + bounded retry, swallow-error contract);
  periodic reconciler worker (desired-state sweep, diff, raise/clear/heartbeat) spawned
  and supervised alongside the existing collectors; in-memory health registry; full
  Tier 1+2+3 condition catalogue (13 conditions across REQ-ALARM-020..042); deterministic
  fingerprint scheme; whole feature gated on `ALARM_CENTER_URL`.

---

## Goal

When a serious operational condition arises (a provider is unreachable, the whole
fallback chain is down, the database is unreachable, a pacer row is missing, queue or
backfill work is permanently failing, a worker is crash-looping, coins have stopped
advancing, the DB pool is saturated, or upserts are failing in a streak), raise an alarm
in the Alarm Center; and when the condition recovers, clear it automatically — the
reconciler simply stops refreshing the alarm so the Alarm Center auto-clears it once its TTL
lapses (with an optional immediate fast-clear for Critical/Error severities). Alarm delivery
must never block, panic, or degrade a collector, and the whole feature must be a no-op when
unconfigured.

## Scope

In scope:
- An `AlarmClient` wrapping one shared `reqwest::Client` with a `raise(spec)` method that
  always sends `timeoutSeconds = ALARM_TTL_SECS` and an optional `clear(fingerprint)`
  fast-path method, both with a short per-request timeout, a small bounded number of
  retries, and a swallow-error-and-log contract.
- A dedicated **reconciler** worker spawned alongside `live_poller`/`collection_queue`/
  `backfill` via `spawn_workers`, supervised/restarted identically.
- A shared in-memory **health registry** that provider/collector/db error sites update
  cheaply and the reconciler reads.
- The full condition catalogue (Tier 1 provider reachability + Tier 2 major issues +
  Tier 3 lower priority), each mapped to a component, severity, code, and deterministic
  fingerprint.
- The deterministic fingerprint scheme and the TTL-driven lifecycle model
  (raise-with-TTL → heartbeat-refresh → stop-refresh so the server auto-clears, plus an
  optional immediate fast-clear for Critical/Error).
- All configuration via env vars, with the whole feature gated on `ALARM_CENTER_URL`.

Out of scope: see Exclusions. The Alarm Center service itself and its API design; alarm
acknowledgement/suppression flows (server-side lifecycle); Helm/env plumbing of the new
vars (SPEC-DEPLOY-001); Prometheus emission of new alarm metrics (optional, deferred).

## Decisions Restated (authoritative)

- **D1 — Full catalogue.** Every condition in the Condition Catalogue below is in scope
  (Tier 1 + Tier 2 + Tier 3). No conditions are deferred.
- **D2 — Periodic reconciler drives lifecycle; server TTL clears.** A single background
  worker runs every `ALARM_RECONCILE_INTERVAL_SECS`. Each sweep it (a) computes the CURRENT
  set of active conditions from observable state, (b) raises/heartbeats every active
  condition with `timeoutSeconds = ALARM_TTL_SECS`, and (c) for a condition no longer
  active, simply STOPS refreshing it — the Alarm Center then auto-clears it once the TTL
  lapses. The reconciler is therefore near-stateless: it needs no correctness-critical
  record of what it raised, because the server (not the reconciler) owns what is currently
  active. This makes "raised but never cleared" structurally impossible.
- **D2a — Hybrid fast-clear.** Layered on the TTL safety net, the reconciler MAY issue an
  explicit `clear()` on an observed active→inactive transition, but ONLY for Critical and
  Error severities, so those clear immediately instead of waiting up to a TTL. Warning
  alarms rely on TTL expiry alone. The fast-clear is a latency optimisation, never a
  correctness requirement: if it is dropped or the transition is missed, TTL still clears
  the alarm.
- **D3 — Best-effort delivery.** Raise (with TTL) AND the optional fast-clear use a short
  timeout and a small bounded retry count; on persistent failure they log `error!` and
  continue. Delivery MUST NEVER block, panic, or degrade a collector or the reconciler —
  mirroring the existing `let _ = pacer::signal_cooldown(...)` swallow-error idiom. There
  are no correctness-critical clears to drop: a dropped raise simply lets the TTL lapse
  (the desired recovery outcome), and a dropped fast-clear falls back to TTL expiry.
- **D4 — Per-condition + per-provider/component fingerprints.** Fingerprints are
  deterministic and specific under the scheme
  `crypto-collector:{condition-slug}[:{provider-or-component}]`, so each provider's or
  component's alarm raises and clears independently.
- **D5 — Feature gated on `ALARM_CENTER_URL`.** Unset/empty = the reconciler is never
  spawned, no alarm-center requests are made, and the service behaves identically to
  today (mirrors how OTLP export is gated by `OTEL_EXPORTER_OTLP_ENDPOINT`,
  SPEC-OBS-001 REQ-OBS-022).
- **D6 — `sourceService` is constant.** Every alarm carries
  `sourceService = "crypto-collector"`; `component` identifies the subsystem.
- **D7 — Level-state read from durable/system sources.** Where a condition is a level
  (cooldown active, credits exhausted, queue/backfill failed counts, missing pacer row,
  DB reachability, pool saturation, coin staleness), the reconciler derives it from the
  durable `upstream_request_pacer`/`collection_queue`/`backfill_chunks`/`tracked_coins`
  tables, the readiness DB-ping, or the live `PgPool` handle — NOT from the several
  Prometheus gauges that SPEC-OBS-001 describes but does not necessarily emit. Only
  edge/streak conditions with no durable signal (sustained provider-unreachable, chain
  all-down, worker restarts, upsert-failure streak) use the in-memory registry.

---

## Design Summary (WHAT, not HOW)

1. **AlarmClient.** A small client holding one shared `reqwest::Client` (built once,
   stored in an `Arc`, injected into the reconciler). `raise(AlarmSpec)` POSTs
   `/api/v1/alarms` and ALWAYS includes `timeoutSeconds = ALARM_TTL_SECS` (so the server
   sets/refreshes an auto-clear deadline; omitting it would revert the alarm to
   never-expire, contract alarm-center.yaml:34-37). The optional fast-path
   `clear(fingerprint)` POSTs `/api/v1/alarms/{fingerprint}/clear`. Both apply
   `ALARM_CENTER_TIMEOUT_MS` per attempt and up to `ALARM_CENTER_MAX_RETRIES` retries; on
   exhaustion they log and return without error. A `clear` that receives `404` (fingerprint
   never raised, or already TTL-expired) is treated as success (no-op). When
   `ALARM_CENTER_API_KEY` is set, an auth header is attached.

2. **AlarmSpec.** The raise payload carries the fixed `sourceService = "crypto-collector"`,
   the condition's `component`, `severity`, `code`, `title`, `description`,
   `timeoutSeconds = ALARM_TTL_SECS`, and optional `labels`/`details` (e.g. provider name,
   offending count, capability). The `fingerprint` follows the deterministic scheme.

3. **Health registry.** A shared, cheap-to-update in-memory structure (behind `Arc`)
   holding exactly the counters/flags the reconciler cannot re-derive from the database:
   - per-provider reachability: last-success instant and consecutive network-failure
     count, updated at provider network-error sites (REQ-ALARM-020);
   - a chain-outcome flag: set when a full fallback chain attempt records every
     `AttemptRecord.outcome == Failure`, cleared on any chain success (REQ-ALARM-022);
   - per-worker timestamped restart events over a sliding window, pushed in the collectors'
     supervisor restart arms (REQ-ALARM-034);
   - a consecutive DB upsert-failure streak counter, reset on any successful upsert
     (REQ-ALARM-042).
   All other conditions are derived from durable/system state (D7). This registry drives
   DETECTION (when a condition is active, i.e. whether to keep refreshing); it is never the
   clear mechanism — recovery is server-driven via TTL, so an imperfect/lost registry can
   no longer strand an alarm.

4. **Reconciler worker (near-stateless).** A sweep loop on `ALARM_RECONCILE_INTERVAL_SECS`:
   compute the desired active-condition set, then `raise()` every active condition with
   `timeoutSeconds = ALARM_TTL_SECS` (a create on first raise, a dedup heartbeat that
   refreshes the deadline thereafter). For a condition that is no longer active, it simply
   STOPS calling `raise()` — the server auto-clears it once the TTL lapses. No
   correctness-critical reported-set is needed, and no startup seeding / `list_open()` is
   required: an alarm a previous instance raised auto-expires if the fresh instance does
   not refresh it. The reconciler MAY keep a small in-memory "previously-active" set used
   ONLY to spot active→inactive transitions for the optional fast-clear (D2a); losing that
   set (e.g. on restart) does not affect correctness because TTL still clears everything.
   It is spawned by `spawn_workers` and supervised (restart-on-panic) the same way as the
   existing three workers. Because a healthy start proves config parsed, the reconciler may
   also fast-clear `crypto-collector:startup-config-error` on its first sweep, though that
   alarm too auto-expires by TTL if left alone.

5. **Feature gate.** `main` builds the `AlarmClient` and spawns the reconciler only when
   `ALARM_CENTER_URL` is set. When unset, none of the above is constructed and no code
   path contacts the alarm center.

6. **Startup-config-error special case.** `build_chain` fails fast at startup (unknown
   provider name) BEFORE the reconciler exists. When `ALARM_CENTER_URL` is set and
   startup config is fatal, `main` makes a **single best-effort blocking raise — one
   attempt, bounded by `ALARM_CENTER_TIMEOUT_MS`, with zero retries — carrying
   `timeoutSeconds = ALARM_TTL_SECS`** — of `crypto-collector:startup-config-error` before
   exiting non-zero, so a crash-looping pod is never delayed by alarm delivery. Because the
   raise carries a TTL, the alarm auto-clears once a healthy start stops re-raising it; no
   in-process clear path before exit is needed, which resolves the earlier OR-ALARM-1
   timing concern.

## Condition Catalogue (severity / fingerprint / code mapping)

`sourceService = "crypto-collector"` for every row. Fingerprint templates substitute the
concrete provider/worker name where shown.

| # | Condition | REQ | Component | Severity | Code | Fingerprint template | Active signal (desired-state source) | Clear trigger |
|---|-----------|-----|-----------|----------|------|----------------------|--------------------------------------|---------------|
| 1 | Single provider unreachable, sustained | REQ-ALARM-020 | `providers` | Warning | `PROVIDER_UNREACHABLE` | `crypto-collector:provider-unreachable:{provider}` | registry: consecutive `ProviderError::Network` with no success for ≥ `ALARM_PROVIDER_UNREACHABLE_SECS` | a success refreshes last-success within threshold |
| 2 | Provider rate-limited / fleet cooldown active | REQ-ALARM-021 | `pacer` | Warning | `PROVIDER_RATE_LIMITED` | `crypto-collector:provider-rate-limited:{provider}` | `upstream_request_pacer.cooldown_until > now()` | `cooldown_until` NULL or in the past |
| 3 | All providers in chain failed (no data for a capability) | REQ-ALARM-022 | `providers` | Critical | `ALL_PROVIDERS_DOWN` | `crypto-collector:all-providers-down` | registry chain-outcome flag = down (a chain attempt where every `AttemptRecord` is `Failure`) | any subsequent chain success flips the flag |
| 4 | Provider credit / quota exhausted | REQ-ALARM-023 | `pacer` | Error | `PROVIDER_CREDIT_EXHAUSTED` | `crypto-collector:provider-credit-exhausted:{provider}` | `upstream_request_pacer`: `credits_used >= credit_limit` | `credits_used < credit_limit` (window reset) |
| 5 | Database unreachable / sustained readiness failure | REQ-ALARM-030 | `db` | Critical | `DB_UNREACHABLE` | `crypto-collector:db-unreachable` | readiness `SELECT 1` fails for ≥ `ALARM_DB_UNREACHABLE_SECS` | `SELECT 1` succeeds |
| 6 | Missing pacer row (config/seed error) | REQ-ALARM-031 | `pacer` | Error | `MISSING_PACER_ROW` | `crypto-collector:missing-pacer-row:{provider}` | configured provider has no `upstream_request_pacer` row (or `AcquireSlotError::NotFound`) | row present for the provider |
| 7 | Recent collection-queue failures (windowed rate) | REQ-ALARM-032 | `collection_queue` | Warning | `COLLECTION_QUEUE_FAILURES` | `crypto-collector:collection-queue-failures` | `count(*) FROM collection_queue WHERE status='failed' AND updated_at > now() - ALARM_QUEUE_FAILED_WINDOW_SECS` ≥ `ALARM_QUEUE_FAILED_THRESHOLD` | no new failures land in the window (windowed count below threshold) |
| 8a | Recent backfill-chunk failures (windowed rate) | REQ-ALARM-033 | `backfill` | Warning | `BACKFILL_FAILED` | `crypto-collector:backfill-failed` | `count(*) FROM backfill_chunks WHERE status='failed' AND updated_at > now() - ALARM_BACKFILL_FAILED_WINDOW_SECS` ≥ `ALARM_BACKFILL_FAILED_THRESHOLD` | no new failures land in the window (windowed count below threshold) |
| 8b | Backfill stalled (pending, no progress) | REQ-ALARM-033 | `backfill` | Warning | `BACKFILL_STALLED` | `crypto-collector:backfill-stalled` | pending chunks exist but none advanced for ≥ `ALARM_BACKFILL_STALL_SECS` | progress resumes or no pending chunks remain |
| 9 | Worker crash-looping | REQ-ALARM-034 | `collectors` | Error | `WORKER_CRASH_LOOPING` | `crypto-collector:worker-crash-looping:{worker}` | registry timestamped restart events for the worker ≥ `ALARM_WORKER_CRASHLOOP_THRESHOLD` within `ALARM_WORKER_CRASHLOOP_WINDOW_SECS` | in-window restart events fall below threshold |
| 10 | Unknown provider name at startup (fatal config error) | REQ-ALARM-035 | `config` | Critical | `STARTUP_CONFIG_ERROR` | `crypto-collector:startup-config-error` | `build_chain` fails fast at startup | next healthy start stops re-raising → TTL auto-clears (or fast-clear on first sweep) |
| 11 | Individual coins not advancing (aggregated) | REQ-ALARM-040 | `live_poller` | Warning | `COINS_STALLED` | `crypto-collector:coins-stalled` | count of `tracked_coins` whose `last_polled_at`/`last_collected_at` is stale ≥ `ALARM_COIN_STALENESS_SECS` reaches `ALARM_COINS_STALLED_THRESHOLD` | count below threshold |
| 12 | DB pool exhaustion | REQ-ALARM-041 | `db` | Error | `DB_POOL_EXHAUSTED` | `crypto-collector:db-pool-exhausted` | `pool.num_idle()==0 && pool.size()==MAX_CONNECTIONS` sustained ≥ `ALARM_DB_POOL_SATURATION_SECS` | idle connections available |
| 13 | Sustained DB upsert-failure streak | REQ-ALARM-042 | `db` | Error | `DB_UPSERT_FAILURES` | `crypto-collector:db-upsert-failures` | registry consecutive upsert-failure streak ≥ `ALARM_UPSERT_FAILURE_STREAK` | a successful upsert resets the streak |

The **Active signal** column defines when a condition is active (so the reconciler keeps
refreshing its alarm with `timeoutSeconds = ALARM_TTL_SECS`). The **Clear trigger** column
describes when the condition stops being active; at that point the reconciler simply STOPS
refreshing and the Alarm Center auto-clears the fingerprint once the TTL lapses. For
Critical/Error rows the reconciler additionally MAY fast-clear on the observed transition
so they resolve immediately (D2a); Warning rows rely on TTL expiry alone.

Condition 3 records the affected capability in `labels`/`details` rather than in the
fingerprint (single chain-outcome flag; see OR-ALARM-2). Condition 11 is a deliberate
exception to the per-provider granularity default — one aggregated alarm carrying the
stalled-coin count avoids per-coin alarm spam (see OR-ALARM-3).

Conditions 7 and 8a are **windowed rates, not cumulative counts.** `'failed'` is a
terminal state (`collection_queue.rs:100-108`, `backfill.rs:224-233`) that no production
path ever resets or purges, so a plain `count(*) WHERE status='failed'` is monotonic
non-decreasing and would latch these alarms permanently. Bounding the count to failures
whose `updated_at` falls inside the configured window makes the signal recover once new
failures stop landing. Condition 8b (`backfill-stalled`) is already level-based
(pending-not-advancing) and needs no window.

---

## Requirements (EARS)

### Feature gating and client

- **REQ-ALARM-001** (State-Driven): While `ALARM_CENTER_URL` is set to a non-empty value,
  the system shall construct the `AlarmClient` and spawn the reconciler worker.
- **REQ-ALARM-002** (State-Driven): While `ALARM_CENTER_URL` is unset or empty, the
  system shall not spawn the reconciler, shall make no requests to the alarm center, and
  shall behave identically to a build without this feature.
- **REQ-ALARM-003** (Ubiquitous): The system shall set `sourceService` to
  `"crypto-collector"` on every alarm it raises.
- **REQ-ALARM-004** (Ubiquitous): The system shall derive every fingerprint
  deterministically under the scheme
  `crypto-collector:{condition-slug}[:{provider-or-component}]`, such that a given
  condition-and-target always maps to the same fingerprint.
- **REQ-ALARM-005** (Ubiquitous): The system shall use a single shared `reqwest::Client`
  (built once, stored in an `Arc`) for all alarm-center requests.
- **REQ-ALARM-006** (Event-Driven): When the system issues a raise or a fast-clear, it
  shall apply a per-request timeout of `ALARM_CENTER_TIMEOUT_MS` and retry up to
  `ALARM_CENTER_MAX_RETRIES` times on a failed attempt.
- **REQ-ALARM-007** (If/Unwanted): If a raise or fast-clear still fails after exhausting its
  retries, then the system shall log an `error!` and continue, and shall never block,
  panic, or degrade any collector or the reconciler as a result of alarm delivery; because
  clearing is server-driven via TTL, a dropped raise or fast-clear cannot strand an alarm.
- **REQ-ALARM-008** (Event-Driven): When a fast-clear receives HTTP 404 (the fingerprint
  was never raised, or has already TTL-expired), the system shall treat the clear as
  successful (a no-op), not an error.
- **REQ-ALARM-009** (State-Driven): While `ALARM_CENTER_API_KEY` is set, the system shall
  attach it best-effort as an authentication header on every alarm-center request. The
  Alarm Center OpenAPI defines no `securityScheme`, so the exact header name/format is
  unspecified by the contract and authentication is likely enforced at an ingress/gateway
  layer rather than by the API itself; the concrete scheme is deferred (OR-ALARM-6) and a
  missing/rejected key MUST NOT break the swallow-error delivery contract (REQ-ALARM-007).

### Reconciler lifecycle

- **REQ-ALARM-010** (Ubiquitous): The system shall run the reconciler as a dedicated
  worker spawned alongside `live_poller`/`collection_queue`/`backfill` via
  `spawn_workers`, supervised and restarted on panic/error the same way as those workers.
- **REQ-ALARM-011** (Event-Driven): When each reconcile interval
  (`ALARM_RECONCILE_INTERVAL_SECS`) elapses, the system shall compute the current set of
  active alarm conditions from observable state, `raise()` every active condition (with
  `timeoutSeconds = ALARM_TTL_SECS`), and stop refreshing every condition that is no longer
  active so the server auto-clears it on TTL expiry; for Critical/Error conditions observed
  transitioning active→inactive it may additionally issue an immediate fast-clear.
- **REQ-ALARM-012** (Ubiquitous): The reconciler shall be near-stateless: the Alarm Center
  (via TTL) is the source of truth for what is currently active, so the reconciler needs no
  correctness-critical record of what it raised. It MAY keep a small in-memory
  "previously-active" set used ONLY to detect active→inactive transitions for the optional
  fast-clear (REQ-ALARM-014); losing that set (e.g. on restart) MUST NOT affect correctness
  because TTL still clears everything.
- **REQ-ALARM-013** (Event-Driven): When a condition becomes newly active (was not active
  in the previous sweep), the system shall raise its alarm with the mapped fingerprint,
  component, severity, code, and `timeoutSeconds = ALARM_TTL_SECS`.
- **REQ-ALARM-014** (Event-Driven): When a Critical- or Error-severity condition is
  observed transitioning from active to inactive, the system MAY issue an immediate
  `clear()` for its fingerprint as an optional fast-path so it resolves without waiting for
  TTL expiry; Warning-severity conditions shall rely on TTL expiry alone. Recovery
  correctness comes from stop-refresh + server TTL, never from this call.
- **REQ-ALARM-015** (State-Driven): While a condition remains active across sweeps, the
  system shall re-raise its fingerprint each sweep with `timeoutSeconds = ALARM_TTL_SECS`;
  this heartbeat both refreshes the server-side auto-clear deadline and advances
  `occurrenceCount`/`lastSeen` so the alarm neither expires prematurely nor appears stale.
- **REQ-ALARM-016** (If/Unwanted): If a raise or an optional fast-clear fails to deliver,
  then the system shall take no compensating action, because clearing is server-driven: a
  dropped raise simply lets the TTL lapse (the desired recovery outcome for a condition
  that is in fact recovering), and a dropped fast-clear falls back to TTL expiry. A raise
  that is dropped while the condition is still active is re-attempted on the next sweep.
- **REQ-ALARM-017** (Ubiquitous): The system shall not create duplicate alarms across
  sweeps with unchanged state; re-raising an unchanged active condition relies on
  server-side fingerprint deduplication (a 200 that bumps `occurrenceCount` and refreshes
  the TTL deadline).
- **REQ-ALARM-018** (Event-Driven): When a shutdown signal is received, the reconciler
  shall simply stop; it shall not mass-clear its active alarms. Every alarm it raised
  carries a TTL and auto-expires unless refreshed, so a still-true condition remains visible
  (another replica keeps refreshing it) and a recovered-during-shutdown condition
  auto-clears — no finish-sweep or mass-clear logic is required.
- **REQ-ALARM-018a** — REMOVED (SUPERSEDED by REQ-ALARM-015/018 under the TTL model,
  v1.3.0). It required a startup `list_open()` reconciliation to clear orphans left by a
  lost in-process reported-set (auditor M1). Under server-side TTL a pre-restart orphan
  auto-expires if the fresh instance does not refresh it, so startup seeding is no longer
  needed for correctness; the requirement is retired rather than renumbered.
- **REQ-ALARM-019** (Ubiquitous): The system shall maintain a shared in-memory health
  registry, updated cheaply at provider/collector/db error sites, holding exactly:
  per-provider last-success + consecutive-network-failure counts, a chain-all-down flag,
  per-worker **timestamped restart events evaluated over a sliding
  `ALARM_WORKER_CRASHLOOP_WINDOW_SECS` window** (a decaying event set, NOT a monotonic
  counter — so the rate-based clear in REQ-ALARM-034 is derivable), and a consecutive DB
  upsert-failure streak; the reconciler shall read this registry for the conditions that
  have no durable signal. This registry drives DETECTION only (whether a condition is
  active); it is never a clear mechanism, so its imperfection or loss can no longer strand
  an alarm (clearing is server-driven via TTL).

### Tier 1 — provider reachability

- **REQ-ALARM-020** (State-Driven): While a single provider has produced only
  `ProviderError::Network` failures with no success for at least
  `ALARM_PROVIDER_UNREACHABLE_SECS`, the system shall keep a Warning alarm active with
  component `providers`, code `PROVIDER_UNREACHABLE`, and fingerprint
  `crypto-collector:provider-unreachable:{provider}`; and shall clear it once that
  provider succeeds within the threshold.
- **REQ-ALARM-021** (State-Driven): While `upstream_request_pacer.cooldown_until` for a
  provider is in the future (fleet cooldown active), the system shall keep a Warning
  alarm active with component `pacer`, code `PROVIDER_RATE_LIMITED`, and fingerprint
  `crypto-collector:provider-rate-limited:{provider}`; and shall clear it once the
  cooldown has expired.
- **REQ-ALARM-022** (State-Driven): While the chain-outcome flag indicates all providers
  in the fallback chain failed (every `AttemptRecord.outcome == Failure` on the last
  chain attempt), the system shall keep a Critical alarm active with component
  `providers`, code `ALL_PROVIDERS_DOWN`, and fingerprint
  `crypto-collector:all-providers-down`; and shall clear it on the first subsequent chain
  success.
- **REQ-ALARM-023** (State-Driven): While a provider's `upstream_request_pacer` credit
  budget is exhausted (`credits_used >= credit_limit`), the system shall keep an Error
  alarm active with component `pacer`, code `PROVIDER_CREDIT_EXHAUSTED`, and fingerprint
  `crypto-collector:provider-credit-exhausted:{provider}`; and shall clear it once the
  credit window resets.

### Tier 2 — other major issues

- **REQ-ALARM-030** (State-Driven): While the readiness DB-ping (`SELECT 1`) fails for at
  least `ALARM_DB_UNREACHABLE_SECS`, the system shall keep a Critical alarm active with
  component `db`, code `DB_UNREACHABLE`, and fingerprint `crypto-collector:db-unreachable`;
  and shall clear it once the ping succeeds.
- **REQ-ALARM-031** (State-Driven): While a configured provider has no row in
  `upstream_request_pacer` (detected by comparison, or surfaced as
  `AcquireSlotError::NotFound`), the system shall keep an Error alarm active with
  component `pacer`, code `MISSING_PACER_ROW`, and fingerprint
  `crypto-collector:missing-pacer-row:{provider}`; and shall clear it once the row exists.
- **REQ-ALARM-032** (State-Driven): While the count of `collection_queue` rows with
  `status='failed'` whose `updated_at` is within `ALARM_QUEUE_FAILED_WINDOW_SECS` is at
  least `ALARM_QUEUE_FAILED_THRESHOLD`, the system shall keep a Warning alarm active with
  component `collection_queue`, code `COLLECTION_QUEUE_FAILURES`, and fingerprint
  `crypto-collector:collection-queue-failures`; and shall clear it once no new failures
  land within the window (windowed count below the threshold). The signal is a windowed
  failure rate, not a cumulative count, because `'failed'` is a terminal state that is
  never reset (a cumulative count would latch the alarm permanently).
- **REQ-ALARM-033** (State-Driven): While the count of `backfill_chunks` rows with
  `status='failed'` whose `updated_at` is within `ALARM_BACKFILL_FAILED_WINDOW_SECS` is at
  least `ALARM_BACKFILL_FAILED_THRESHOLD` (fingerprint `crypto-collector:backfill-failed`,
  a windowed rate for the same terminal-state reason as REQ-ALARM-032), OR pending backfill
  chunks exist that have not advanced for at least `ALARM_BACKFILL_STALL_SECS` (fingerprint
  `crypto-collector:backfill-stalled`, a level condition), the system shall keep the
  corresponding Warning alarm active with component `backfill`; and shall clear each
  independently once its condition no longer holds.
- **REQ-ALARM-034** (State-Driven): While a worker's timestamped restart events within
  `ALARM_WORKER_CRASHLOOP_WINDOW_SECS` number at least `ALARM_WORKER_CRASHLOOP_THRESHOLD`,
  the system shall keep an Error alarm active (refreshed each sweep) with component
  `collectors`, code `WORKER_CRASH_LOOPING`, and fingerprint
  `crypto-collector:worker-crash-looping:{worker}`; once older events age out of the window
  and the in-window count falls below the threshold the reconciler stops refreshing (fast-
  clear permitted, Error severity) and the alarm clears.
- **REQ-ALARM-035** (Event-Driven): When `build_chain` fails fast at startup due to an
  unknown provider name and `ALARM_CENTER_URL` is set, the system shall make a single
  best-effort blocking raise — exactly one attempt, bounded by `ALARM_CENTER_TIMEOUT_MS`,
  with zero retries, carrying `timeoutSeconds = ALARM_TTL_SECS` — of a Critical alarm with
  component `config`, code `STARTUP_CONFIG_ERROR`, and fingerprint
  `crypto-collector:startup-config-error` before exiting non-zero, so a crash-looping pod is
  not delayed by alarm delivery. Because the raise carries a TTL, the alarm auto-clears once
  a healthy start stops re-raising it — no in-process clear path before exit is required
  (this resolves OR-ALARM-1).

### Tier 3 — lower priority

- **REQ-ALARM-040** (State-Driven): While the number of `tracked_coins` whose
  `last_polled_at`/`last_collected_at` is stale beyond `ALARM_COIN_STALENESS_SECS` is at
  least `ALARM_COINS_STALLED_THRESHOLD`, the system shall keep a single aggregated Warning
  alarm active with component `live_poller`, code `COINS_STALLED`, fingerprint
  `crypto-collector:coins-stalled`, and the stalled-coin count in `details`; and shall
  clear it once the count falls below the threshold.
- **REQ-ALARM-041** (State-Driven): While the DB pool is saturated
  (`num_idle()==0 && size()==MAX_CONNECTIONS`) continuously for at least
  `ALARM_DB_POOL_SATURATION_SECS`, the system shall keep an Error alarm active with
  component `db`, code `DB_POOL_EXHAUSTED`, and fingerprint
  `crypto-collector:db-pool-exhausted`; and shall clear it once idle connections are
  available.
- **REQ-ALARM-042** (State-Driven): While the consecutive DB upsert-failure streak is at
  least `ALARM_UPSERT_FAILURE_STREAK`, the system shall keep an Error alarm active with
  component `db`, code `DB_UPSERT_FAILURES`, and fingerprint
  `crypto-collector:db-upsert-failures`; and shall clear it once a successful upsert
  resets the streak.

### Configuration

- **REQ-ALARM-050** (Ubiquitous): The system shall read all alarm configuration from
  environment variables only (no config files, no secrets in code), using the existing
  free-function-per-setting pattern in `src/config.rs`, exposing: `ALARM_CENTER_URL`
  (`Option<String>`, unset = feature disabled), `ALARM_CENTER_API_KEY` (`Option<String>`),
  `ALARM_CENTER_TIMEOUT_MS` (default 5000), `ALARM_CENTER_MAX_RETRIES` (default 3),
  `ALARM_RECONCILE_INTERVAL_SECS` (default 30, aligned with `TRACKED_GAUGE_INTERVAL_SECS`),
  and `ALARM_TTL_SECS` (the server-side auto-clear deadline sent as `timeoutSeconds` on every
  raise; default `ceil(2.5 * ALARM_RECONCILE_INTERVAL_SECS)` ≈ 75 s at the default interval,
  sized above the reconcile interval so one slow/missed sweep cannot let an active alarm
  expire — see REQ-ALARM-052).
- **REQ-ALARM-051** (Ubiquitous): The system shall expose per-condition threshold
  settings as env vars with documented defaults: `ALARM_PROVIDER_UNREACHABLE_SECS`
  (default 300), `ALARM_DB_UNREACHABLE_SECS` (default 60), `ALARM_QUEUE_FAILED_THRESHOLD`
  (default 10), `ALARM_QUEUE_FAILED_WINDOW_SECS` (default 3600),
  `ALARM_BACKFILL_FAILED_THRESHOLD` (default 10), `ALARM_BACKFILL_FAILED_WINDOW_SECS`
  (default 3600), `ALARM_BACKFILL_STALL_SECS` (default 3600),
  `ALARM_WORKER_CRASHLOOP_THRESHOLD` (default 3), `ALARM_WORKER_CRASHLOOP_WINDOW_SECS`
  (default 300), `ALARM_COIN_STALENESS_SECS` (default 900), `ALARM_COINS_STALLED_THRESHOLD`
  (default 5), `ALARM_DB_POOL_SATURATION_SECS` (default 60), and
  `ALARM_UPSERT_FAILURE_STREAK` (default 20). The `*_WINDOW_SECS` settings bound the
  windowed failure-rate signals (REQ-ALARM-032/033) so terminal `'failed'` rows age out.
- **REQ-ALARM-052** (Ubiquitous): The system shall include `timeoutSeconds = ALARM_TTL_SECS`
  on every raise and heartbeat it sends to the alarm center, and shall size `ALARM_TTL_SECS`
  to exceed `ALARM_RECONCILE_INTERVAL_SECS` by a safety margin (default
  `ceil(2.5 * ALARM_RECONCILE_INTERVAL_SECS)`), so that a single slow or missed reconcile
  sweep cannot let an active alarm's auto-clear deadline lapse and cause a spurious
  expire→re-raise flap.
- **REQ-ALARM-053** (If/Unwanted): If the system raises any alarm, then it shall never omit
  `timeoutSeconds`; omitting it reverts the alarm to never-expire (alarm-center.yaml:34-37)
  and would defeat the server-driven auto-clear that makes "raised but never cleared"
  structurally impossible.

### Multi-replica behaviour

- **REQ-ALARM-060** (State-Driven): While more than one replica runs concurrently, the
  system shall rely on server-side fingerprint deduplication so that identical fingerprints
  raised by different replicas collapse to one alarm, which stays active while ANY replica
  keeps refreshing it (with `timeoutSeconds = ALARM_TTL_SECS`) and auto-expires only once
  ALL replicas have stopped refreshing. Because the normal path issues no explicit clears,
  there are no cross-replica clear races and no single-writer/leader election is required.
  The only clears are the optional Critical/Error fast-path (REQ-ALARM-014); a fast-clear
  issued by one replica while another still observes the condition active is self-healed by
  that replica's next-sweep re-raise, and server-side TTL guarantees eventual convergence to
  the true global state regardless.

### Operator documentation

- **REQ-ALARM-070** (Ubiquitous): The project shall maintain `docs/alarms.md` as the
  canonical operator-facing catalogue of every alarm this service can raise, and shall
  update it in lockstep whenever an alarm condition is added, removed, or changed (any
  change to its fingerprint, code, severity, component, or governing thresholds/windows) —
  the code catalogue (the condition→spec mapping) and `docs/alarms.md` shall never diverge.
  The document shall open with a short overview covering: the `ALARM_CENTER_URL` feature
  gate (unset = disabled), the reconciler's TTL-driven self-clearing lifecycle model (a
  periodic desired-state sweep that raises/heartbeats every active condition with a
  server-side auto-clear `timeoutSeconds` TTL and simply stops refreshing recovered
  conditions so they auto-expire, plus an optional immediate fast-clear for Critical/Error
  severities), the best-effort delivery semantics (bounded retry then log, never blocks a
  collector), and
  the fingerprint scheme `crypto-collector:{condition-slug}[:{provider-or-component}]`. For
  each of the 14 alarm fingerprints the document shall carry an entry containing: (1) a
  human title and one-line description of the condition; (2) `code`, `severity`, and
  `component`; (3) the fingerprint template (e.g.
  `crypto-collector:provider-unreachable:{provider}`); (4) the active-signal that makes it
  fire and the clear trigger that resolves it; (5) the governing env-var thresholds/windows
  with their defaults (e.g. `ALARM_QUEUE_FAILED_WINDOW_SECS`); and (6) an operator
  remediation hint (what to check or do when it fires).

## Exclusions (What NOT to Build)

- **No scattered inline raise/clear calls** — all lifecycle goes through the single
  reconciler so nothing can be raised without a clear path (D2). Error sites only update
  the cheap in-memory registry; they never call the alarm center directly (except the
  one fatal startup-config raise in `main`, REQ-ALARM-035).
- **No blocking or fallible alarm delivery on the hot path** — delivery is best-effort
  and swallow-error; a collector must never wait on or fail because of the alarm center
  (REQ-ALARM-007).
- **No behaviour change when unconfigured** — `ALARM_CENTER_URL` unset is a full no-op
  (REQ-ALARM-002).
- **No acknowledge/suppress/unsuppress flows** — those are server-side lifecycle states
  the collector does not drive; the collector only raises and clears.
- **No new Prometheus alarm metrics required** — the reconciler reads durable tables /
  the pool / the readiness ping and the in-memory registry, not the OBS gauges
  (D7); an optional alarm-delivery counter may be added later but is out of scope here.
- **No per-coin alarm fan-out** — coin staleness is a single aggregated alarm
  (REQ-ALARM-040), a documented exception to the per-target fingerprint default.
- **No mass-clear on shutdown** — active conditions must remain visible across a pod
  restart (REQ-ALARM-018).
- **No cross-replica coordination** — dedup is the only coordination; correctness under
  multiple replicas rests on fingerprint dedup + per-sweep re-derivation (REQ-ALARM-060).
- **No Alarm Center API redesign** — the OpenAPI contract is fixed.
- **No Helm/env YAML here** — var wiring is SPEC-DEPLOY-001.

## @MX Annotation Targets (high fan_in)

- `AlarmClient::raise`/`clear` — `@MX:ANCHOR` (the single egress point to the alarm
  center) + `@MX:WARN`/`@MX:REASON`: must enforce timeout + bounded retry + swallow-error
  and MUST NOT propagate errors to callers (REQ-ALARM-005/006/007/008).
- The reconciler sweep (desired-state computation + raise-with-TTL/heartbeat + optional
  fast-clear) — `@MX:ANCHOR` + `@MX:WARN`: every active condition MUST be re-raised each
  sweep with `timeoutSeconds = ALARM_TTL_SECS`, and a recovered condition MUST simply stop
  being refreshed so the server auto-clears it. This stop-refresh + server-TTL path is the
  load-bearing "raised but never cleared is structurally impossible" guarantee; it depends on
  no in-memory reported-set, so it survives restarts with no startup seeding
  (REQ-ALARM-011..018).
- The health registry type — `@MX:NOTE` enumerating the exact counters/flags and which
  condition each feeds, so error sites update the right field (REQ-ALARM-019).
- `spawn_workers` — `@MX:NOTE`/update: the reconciler is a fourth supervised worker gated
  on `ALARM_CENTER_URL` (REQ-ALARM-001/010).
- The startup path in `main` around `build_chain` — `@MX:WARN`/`@MX:REASON`: the fatal
  config raise happens before the reconciler exists and has no in-process clear
  (REQ-ALARM-035, OR-ALARM-1).

## Open Items (do not guess)

- **OR-ALARM-1 (RESOLVED, v1.3.0):** startup-config-error timing. `build_chain` aborts
  before the alarm subsystem is up, but the fatal raise now carries `timeoutSeconds =
  ALARM_TTL_SECS`, so the alarm auto-clears once a healthy start stops re-raising it and no
  in-process clear path before exit is required (REQ-ALARM-035). Residual (run-phase only):
  confirm whether the fatal raise should also cover other fatal startup-config failures
  (folding into `STARTUP_CONFIG_ERROR`) or stay unknown-provider-specific.
- **OR-ALARM-2:** all-providers-down fingerprint granularity. Default is a single
  `crypto-collector:all-providers-down` flag with capability in `details`. Confirm whether
  per-capability fingerprints (`...:{capability}`) are wanted instead (multiplies the flag
  set).
- **OR-ALARM-3:** coin-staleness aggregation threshold and window
  (`ALARM_COINS_STALLED_THRESHOLD`, `ALARM_COIN_STALENESS_SECS` defaults). Ops tuning.
- **OR-ALARM-4:** reconcile interval vs DB load. The sweep runs several `count(*)` /
  staleness queries each interval; confirm the default 30 s interval and these queries add
  negligible load at production table sizes, or add lightweight indexes / widen the
  interval.
- **OR-ALARM-5 (DOWNGRADED, v1.3.0):** multi-replica heartbeat multiplication. N replicas
  each heartbeat every interval. Because the normal path issues no explicit clears (recovery
  is stop-refresh + server TTL), there are no clear/re-raise races and leader election is
  unnecessary (REQ-ALARM-060); the only residual is duplicate heartbeat traffic (N× raises
  per interval), which server-side dedup collapses to one alarm. Confirm whether that traffic
  volume is acceptable or whether a single-writer refinement is wanted later purely for load.
- **OR-ALARM-6:** exact auth header name/format for `ALARM_CENTER_API_KEY` (the OpenAPI
  does not mandate a scheme). Confirm at run.

---

## Implementation Notes

**Status:** Completed 2026-07-09

**Commits:** `90c2b9f..c66c734`

**Summary:**
- Implementation matched the plan with zero scope divergence.
- New module `src/alarm/` with four submodules: `mod.rs` (AlarmClient), `catalog.rs` (condition catalogue), `registry.rs` (health registry), `reconciler.rs` (periodic sweep worker).
- Wiring: reconciler spawned as 4th supervised worker in `spawn_workers` (gated on `ALARM_CENTER_URL`); registry poke sites in `src/providers/mod.rs`, `src/collectors/{live_poller,collection_queue,backfill}.rs`, and upsert error paths; fatal startup-config raise in `src/main.rs`; `src/db/{pool.rs,mod.rs}` expose `max_connections()` for pool-saturation detection.
- Operator catalogue at `docs/alarms.md` with all 14 fingerprints and remediation guidance.
- Helm chart config: `charts/crypto-collector/values.yaml` alarm section, gated configmap block, deployment secret injection.
- New env vars: `ALARM_CENTER_URL`, `ALARM_CENTER_API_KEY`, `ALARM_CENTER_TIMEOUT_MS`, `ALARM_CENTER_MAX_RETRIES`, `ALARM_RECONCILE_INTERVAL_SECS`, `ALARM_TTL_SECS`, `ALARM_QUEUE_FAILED_WINDOW_SECS`, `ALARM_BACKFILL_FAILED_WINDOW_SECS`, plus 12 per-condition threshold vars (all in `src/config.rs` with free-function pattern).
- No new runtime dependencies: `reqwest` already present; `wiremock` already a dev-dependency (alarm client contract tests).
- Test coverage: 550 lib tests passing + ~40 alarm-specific unit/integration tests + docs-parity test; DB-integration tests marked `#[ignore]`.
- Feature is a complete no-op until `ALARM_CENTER_URL` is set.
