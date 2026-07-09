# Alarm Catalogue

SPEC-ALARM-001 — canonical operator-facing catalogue of every alarm Crypto Collector can
raise in the Alarm Center. This document and the code condition→spec mapping
(`src/alarm/catalog.rs`) are maintained in lockstep (REQ-ALARM-070): any change to a
fingerprint, code, severity, component, or governing threshold/window is reflected in
both places in the same change.

> **Note:** SPEC-ALARM-001 is fully implemented (Batches 1–3): the fingerprint/mapping
> catalogue, the `AlarmClient` HTTP contract, the health registry, the reconciler sweep
> (Tier 1 + Tier 2 + Tier 3 desired-state derivation), and the fatal startup-config raise
> are all wired and spawned from `main`/`collectors`. Every fingerprint, threshold, and
> env var below is live in the running service.

## Overview

**Feature gate.** The whole alarm feature is gated on `ALARM_CENTER_URL`. When unset or
empty, no `AlarmClient` is constructed, no reconciler runs, and no request is ever sent
to the Alarm Center — the service behaves identically to a build without this feature
(REQ-ALARM-001/002).

**Lifecycle model: TTL-driven self-clearing.** A periodic reconciler sweep computes the
current set of active conditions and raises/heartbeats each one, always including
`timeoutSeconds = ALARM_TTL_SECS` on the request body. The Alarm Center uses this value
to set/refresh a server-side auto-clear deadline (`expiresAt`). When a condition recovers,
the reconciler simply **stops refreshing** that fingerprint; the alarm auto-expires once
its TTL lapses. This makes "raised but never cleared" structurally impossible — there is
no in-process reported-set whose loss (e.g. on restart) could strand an alarm.

As a latency optimisation layered on top of the TTL safety net, Critical- and
Error-severity conditions may additionally receive an immediate **fast-clear** — an
explicit `POST /api/v1/alarms/{fingerprint}/clear` call — the moment the reconciler
observes an active→inactive transition. Warning-severity conditions rely on TTL expiry
alone. A dropped or lost fast-clear is not a correctness problem: TTL expiry still clears
the alarm.

**Best-effort delivery.** Every raise and fast-clear applies a per-attempt timeout
(`ALARM_CENTER_TIMEOUT_MS`) and retries up to `ALARM_CENTER_MAX_RETRIES` times. On
persistent failure the client logs an `error!` and returns without propagating — alarm
delivery must never block, panic, or degrade a collector or the reconciler. A fast-clear
that receives HTTP 404 (fingerprint never raised, or already TTL-expired) is treated as
success, not an error.

**Fingerprint scheme.** Every fingerprint follows
`crypto-collector:{condition-slug}[:{provider-or-component}]`, so a given
condition-and-target combination always maps to the same fingerprint and each
provider's/component's alarm raises and clears independently. `sourceService` is always
`"crypto-collector"`.

---

## Alarm Entries

### `provider-unreachable`

- **Title / description:** Provider unreachable — a single upstream provider has produced
  only network failures with no success for a sustained period.
- **Code / Severity / Component:** `PROVIDER_UNREACHABLE` / Warning / `providers`
- **Fingerprint template:** `crypto-collector:provider-unreachable:{provider}`
- **Active signal:** consecutive `ProviderError::Network` failures with no success for at
  least `ALARM_PROVIDER_UNREACHABLE_SECS`.
- **Clear trigger:** a success refreshes last-success within the threshold; the reconciler
  stops refreshing and the alarm TTL-clears (Warning — no fast-clear).
- **Governing env vars:** `ALARM_PROVIDER_UNREACHABLE_SECS` (default `300`).
- **Remediation hint:** check the provider's public status page and this service's egress
  connectivity; confirm the fallback chain is still serving the affected capability from
  another provider.

### `provider-rate-limited`

- **Title / description:** Provider rate-limited — the fleet-wide pacer cooldown for a
  provider is currently active.
- **Code / Severity / Component:** `PROVIDER_RATE_LIMITED` / Warning / `pacer`
- **Fingerprint template:** `crypto-collector:provider-rate-limited:{provider}`
- **Active signal:** `upstream_request_pacer.cooldown_until` for the provider is in the
  future.
- **Clear trigger:** `cooldown_until` is `NULL` or in the past; the reconciler stops
  refreshing and it TTL-clears (Warning — no fast-clear).
- **Governing env vars:** none provider-specific; cooldown duration is set via
  `PACER_{PROVIDER}_COOLDOWN_MS` (SPEC-PROV-001).
- **Remediation hint:** confirm request volume is within the provider's rate limits;
  consider widening `PACER_{PROVIDER}_COOLDOWN_MS` or reducing poll frequency.

### `all-providers-down`

- **Title / description:** All providers in the fallback chain failed for a requested
  capability.
- **Code / Severity / Component:** `ALL_PROVIDERS_DOWN` / Critical / `providers`
- **Fingerprint template:** `crypto-collector:all-providers-down` (single aggregated
  flag; the affected capability is carried in `details`, not the fingerprint —
  OR-ALARM-2).
- **Active signal:** the most recent chain attempt recorded every `AttemptRecord.outcome
  == Failure`.
- **Clear trigger:** any subsequent chain success; being Critical, the reconciler
  fast-clears immediately (TTL as fallback).
- **Governing env vars:** none (derived from the in-memory chain-outcome flag).
- **Remediation hint:** treat as a potential multi-provider outage or a shared root cause
  (e.g. DNS/egress); check each provider's status individually before assuming a single
  root cause.

### `provider-credit-exhausted`

- **Title / description:** A provider's request-credit budget is exhausted.
- **Code / Severity / Component:** `PROVIDER_CREDIT_EXHAUSTED` / Error / `pacer`
- **Fingerprint template:** `crypto-collector:provider-credit-exhausted:{provider}`
- **Active signal:** `upstream_request_pacer.credits_used >= credit_limit`.
- **Clear trigger:** the credit window resets (`credits_used < credit_limit`); being
  Error, the reconciler fast-clears immediately (TTL as fallback).
- **Governing env vars:** none (credit limits are seeded/managed in
  `upstream_request_pacer`, SPEC-DB-001).
- **Remediation hint:** check whether the provider's plan/tier needs an upgrade, or
  whether request volume can be reduced until the window resets.

### `db-unreachable`

- **Title / description:** The readiness DB-ping has failed for a sustained period.
- **Code / Severity / Component:** `DB_UNREACHABLE` / Critical / `db`
- **Fingerprint template:** `crypto-collector:db-unreachable`
- **Active signal:** readiness `SELECT 1` fails for at least `ALARM_DB_UNREACHABLE_SECS`.
- **Clear trigger:** `SELECT 1` succeeds again; being Critical, the reconciler
  fast-clears immediately (TTL as fallback).
- **Governing env vars:** `ALARM_DB_UNREACHABLE_SECS` (default `60`).
- **Remediation hint:** check the PostgreSQL instance's health, network path, and
  connection pool exhaustion; this is a top-severity condition that likely also blocks
  reads/writes across the service.

### `missing-pacer-row`

- **Title / description:** A configured provider has no corresponding
  `upstream_request_pacer` row (config/seed error).
- **Code / Severity / Component:** `MISSING_PACER_ROW` / Error / `pacer`
- **Fingerprint template:** `crypto-collector:missing-pacer-row:{provider}`
- **Active signal:** the provider is present in `PROVIDERS` but absent from
  `upstream_request_pacer` (detected by comparison, or surfaced as
  `AcquireSlotError::NotFound`).
- **Clear trigger:** the row exists; being Error, the reconciler fast-clears immediately
  (TTL as fallback).
- **Governing env vars:** none (compares `PROVIDERS` against `upstream_request_pacer`
  rows).
- **Remediation hint:** run the pacer seed migration/insert for the missing provider, or
  remove it from `PROVIDERS` if it is no longer in use.

### `collection-queue-failures`

- **Title / description:** Recent `collection_queue` rows are failing at an elevated
  rate.
- **Code / Severity / Component:** `COLLECTION_QUEUE_FAILURES` / Warning /
  `collection_queue`
- **Fingerprint template:** `crypto-collector:collection-queue-failures`
- **Active signal:** `count(*) FROM collection_queue WHERE status='failed' AND
  updated_at > now() - ALARM_QUEUE_FAILED_WINDOW_SECS` is at least
  `ALARM_QUEUE_FAILED_THRESHOLD`. This is a **windowed** rate, not a cumulative count,
  because `'failed'` is a terminal state no production path resets — an unwindowed count
  would latch the alarm forever.
- **Clear trigger:** no new failures land within the window (the windowed count drops
  below the threshold, even though the terminal rows persist); the reconciler stops
  refreshing and it TTL-clears (Warning — no fast-clear).
- **Governing env vars:** `ALARM_QUEUE_FAILED_THRESHOLD` (default `10`),
  `ALARM_QUEUE_FAILED_WINDOW_SECS` (default `3600`).
- **Remediation hint:** inspect recently failed `collection_queue` rows for a common
  cause (provider outage, schema drift, bad coin id); consider requeueing after the fix.

### `backfill-failed`

- **Title / description:** Recent `backfill_chunks` rows are failing at an elevated rate.
- **Code / Severity / Component:** `BACKFILL_FAILED` / Warning / `backfill`
- **Fingerprint template:** `crypto-collector:backfill-failed`
- **Active signal:** `count(*) FROM backfill_chunks WHERE status='failed' AND updated_at
  > now() - ALARM_BACKFILL_FAILED_WINDOW_SECS` is at least
  `ALARM_BACKFILL_FAILED_THRESHOLD` — a windowed rate for the same terminal-state reason
  as `collection-queue-failures`.
- **Clear trigger:** no new failures land within the window; the reconciler stops
  refreshing and it TTL-clears (Warning — no fast-clear).
- **Governing env vars:** `ALARM_BACKFILL_FAILED_THRESHOLD` (default `10`),
  `ALARM_BACKFILL_FAILED_WINDOW_SECS` (default `3600`).
- **Remediation hint:** inspect recently failed backfill chunks for a common cause
  (provider outage, exhausted deep-history range, malformed cursor).

### `backfill-stalled`

- **Title / description:** Pending backfill chunks exist but none have advanced.
- **Code / Severity / Component:** `BACKFILL_STALLED` / Warning / `backfill`
- **Fingerprint template:** `crypto-collector:backfill-stalled`
- **Active signal:** pending chunks exist but none have advanced for at least
  `ALARM_BACKFILL_STALL_SECS` — a level condition, independent of the windowed
  `backfill-failed` fingerprint.
- **Clear trigger:** progress resumes, or no pending chunks remain; the reconciler stops
  refreshing and it TTL-clears (Warning — no fast-clear).
- **Governing env vars:** `ALARM_BACKFILL_STALL_SECS` (default `3600`).
- **Remediation hint:** check whether the backfill worker is running and holding leases
  correctly; look for a stuck lease or a worker crash-loop.

### `worker-crash-looping`

- **Title / description:** A supervised worker (`live_poller`, `collection_queue`, or
  `backfill`) is restarting repeatedly.
- **Code / Severity / Component:** `WORKER_CRASH_LOOPING` / Error / `collectors`
- **Fingerprint template:** `crypto-collector:worker-crash-looping:{worker}`
- **Active signal:** the worker's timestamped restart events within
  `ALARM_WORKER_CRASHLOOP_WINDOW_SECS` number at least
  `ALARM_WORKER_CRASHLOOP_THRESHOLD` — a decaying event set, not a monotonic counter, so
  the alarm can actually clear.
- **Clear trigger:** older restart events age out of the window and the in-window count
  falls below the threshold; being Error, the reconciler fast-clears (TTL as fallback).
- **Governing env vars:** `ALARM_WORKER_CRASHLOOP_THRESHOLD` (default `3`),
  `ALARM_WORKER_CRASHLOOP_WINDOW_SECS` (default `300`).
- **Remediation hint:** check the worker's panic/error logs for the underlying cause
  (unhandled DB error, unexpected upstream response shape, resource exhaustion).

### `startup-config-error`

- **Title / description:** `build_chain` failed fast at startup due to an unknown
  provider name in `PROVIDERS`.
- **Code / Severity / Component:** `STARTUP_CONFIG_ERROR` / Critical / `config`
- **Fingerprint template:** `crypto-collector:startup-config-error`
- **Active signal:** `build_chain` fails fast at startup (before the reconciler exists).
- **Clear trigger:** a subsequent healthy start stops re-raising it, so it TTL-clears (the
  reconciler may also fast-clear it on its first sweep once healthy).
- **Governing env vars:** none (triggered by an invalid `PROVIDERS` value). The fatal
  raise itself uses `ALARM_CENTER_TIMEOUT_MS` with zero retries, still carrying
  `timeoutSeconds = ALARM_TTL_SECS`.
- **Remediation hint:** fix the `PROVIDERS` env var to only list known provider names
  (`coingecko`, `binance`, `coinbase`, `kraken`, `bitstamp`) and redeploy.

### `coins-stalled`

- **Title / description:** A number of tracked coins have stopped advancing (single
  aggregated alarm, not per-coin — OR-ALARM-3).
- **Code / Severity / Component:** `COINS_STALLED` / Warning / `live_poller`
- **Fingerprint template:** `crypto-collector:coins-stalled` (the stalled-coin count is
  carried in `details`).
- **Active signal:** the count of `tracked_coins` whose `last_polled_at`/
  `last_collected_at` is stale beyond `ALARM_COIN_STALENESS_SECS` reaches
  `ALARM_COINS_STALLED_THRESHOLD`.
- **Clear trigger:** the count falls below the threshold; the reconciler stops
  refreshing and it TTL-clears (Warning — no fast-clear).
- **Governing env vars:** `ALARM_COIN_STALENESS_SECS` (default `900`),
  `ALARM_COINS_STALLED_THRESHOLD` (default `5`).
- **Remediation hint:** check `live_poller` scheduling/health and whether the affected
  coins' upstream providers are returning data.

### `db-pool-exhausted`

- **Title / description:** The database connection pool is saturated (no idle
  connections at full size).
- **Code / Severity / Component:** `DB_POOL_EXHAUSTED` / Error / `db`
- **Fingerprint template:** `crypto-collector:db-pool-exhausted`
- **Active signal:** `pool.num_idle()==0 && pool.size()==MAX_CONNECTIONS` sustained for at
  least `ALARM_DB_POOL_SATURATION_SECS`.
- **Clear trigger:** idle connections become available; being Error, the reconciler
  fast-clears immediately (TTL as fallback).
- **Governing env vars:** `ALARM_DB_POOL_SATURATION_SECS` (default `60`).
- **Remediation hint:** check for slow queries or connection leaks holding pool slots;
  consider raising `MAX_CONNECTIONS` if load has genuinely grown.

### `db-upsert-failures`

- **Title / description:** A sustained streak of consecutive database upsert failures.
- **Code / Severity / Component:** `DB_UPSERT_FAILURES` / Error / `db`
- **Fingerprint template:** `crypto-collector:db-upsert-failures`
- **Active signal:** the consecutive DB upsert-failure streak (in-memory registry) is at
  least `ALARM_UPSERT_FAILURE_STREAK`.
- **Clear trigger:** a successful upsert resets the streak to zero; being Error, the
  reconciler fast-clears immediately (TTL as fallback).
- **Governing env vars:** `ALARM_UPSERT_FAILURE_STREAK` (default `20`).
- **Remediation hint:** check recent upsert error logs for a schema mismatch, constraint
  violation, or sustained DB unavailability (may correlate with `db-unreachable` or
  `db-pool-exhausted`).

---

## Configuration Reference

| Env var | Default | Purpose |
|---|---|---|
| `ALARM_CENTER_URL` | unset (disabled) | Feature gate; base URL of the Alarm Center. |
| `ALARM_CENTER_API_KEY` | unset | Best-effort `Authorization: Bearer` header. |
| `ALARM_CENTER_TIMEOUT_MS` | `5000` | Per-attempt HTTP timeout for raise/clear. |
| `ALARM_CENTER_MAX_RETRIES` | `3` | Bounded retry count before swallow-and-log. |
| `ALARM_RECONCILE_INTERVAL_SECS` | `30` | Reconciler sweep cadence. |
| `ALARM_TTL_SECS` | `ceil(2.5 * ALARM_RECONCILE_INTERVAL_SECS)` (≈`75`) | Server-side auto-clear TTL sent as `timeoutSeconds` on every raise. |
| `ALARM_PROVIDER_UNREACHABLE_SECS` | `300` | `provider-unreachable` sustained threshold. |
| `ALARM_DB_UNREACHABLE_SECS` | `60` | `db-unreachable` sustained threshold. |
| `ALARM_QUEUE_FAILED_THRESHOLD` | `10` | `collection-queue-failures` windowed count threshold. |
| `ALARM_QUEUE_FAILED_WINDOW_SECS` | `3600` | `collection-queue-failures` window. |
| `ALARM_BACKFILL_FAILED_THRESHOLD` | `10` | `backfill-failed` windowed count threshold. |
| `ALARM_BACKFILL_FAILED_WINDOW_SECS` | `3600` | `backfill-failed` window. |
| `ALARM_BACKFILL_STALL_SECS` | `3600` | `backfill-stalled` stall threshold. |
| `ALARM_WORKER_CRASHLOOP_THRESHOLD` | `3` | `worker-crash-looping` restart-count threshold. |
| `ALARM_WORKER_CRASHLOOP_WINDOW_SECS` | `300` | `worker-crash-looping` sliding window. |
| `ALARM_COIN_STALENESS_SECS` | `900` | `coins-stalled` per-coin staleness threshold. |
| `ALARM_COINS_STALLED_THRESHOLD` | `5` | `coins-stalled` aggregate count threshold. |
| `ALARM_DB_POOL_SATURATION_SECS` | `60` | `db-pool-exhausted` sustained saturation threshold. |
| `ALARM_UPSERT_FAILURE_STREAK` | `20` | `db-upsert-failures` consecutive-failure threshold. |

---

Version: 1.3.0 (Batches 1–3 — full implementation: config, `AlarmClient`, fingerprint
catalogue, health registry, reconciler Tier 1/2/3, fatal startup-config raise)
Source: SPEC-ALARM-001
