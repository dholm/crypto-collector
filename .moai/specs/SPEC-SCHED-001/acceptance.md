# Acceptance Criteria — SPEC-SCHED-001 (Background Collection Workers)

Each scenario maps to EARS requirements in `spec.md`. SQL-shape scenarios are
string-match tests (mirroring ticker `live_poller_claim_sql_*`); concurrency/persistence
scenarios are gated (`#[ignore]`) on a live DB.

## Scenario 1 — Three workers spawn at startup, run continuously (REQ-SCHED-001/010/020/008)

- **Given** a started replica
- **When** startup completes
- **Then** a live-quote poller, a collection-queue worker, and a backfill worker are
  running, none consults any market-hours/calendar/phase/halt gate, and the poller
  loops on its cadence regardless of wall-clock day or hour.

## Scenario 2 — Poller claim sets the marker (not the cursor) in a short tx (REQ-SCHED-003/004)

- **Given** the live-poller claim SQL and control flow
- **When** inspected (SQL-shape test)
- **Then** the claim contains `FOR UPDATE SKIP LOCKED`, `status='active'`, the
  due-and-not-in-flight predicate, and `SET live_poll_claimed_until = now() + $ttl`; it
  does NOT set `last_polled_at`; and the claim transaction commits/releases locks before
  the outbound fetch loop.

## Scenario 3 — NULL per-market interval falls back to global default (REQ-SCHED-002)

- **Given** an active market with `live_poll_interval = NULL`
- **When** due-ness is evaluated
- **Then** the effective cadence is `LIVE_QUOTE_POLL_INTERVAL_SECS` (due when
  `last_polled_at IS NULL OR last_polled_at + <global> <= now()`).

## Scenario 4 — Success advances cursor; transient failure fast-retries (REQ-SCHED-005/006)

- **Given** a claimed market
- **When** its spot fetch succeeds
- **Then** `live_quotes` gets the row, `last_polled_at = now()`, marker cleared; and
  **when** instead the fetch fails transiently, `last_polled_at` is unchanged, the
  marker is cleared, and the market is due again next cycle.

## Scenario 5 — No double-poll across replicas; expired marker re-claimable (REQ-SCHED-007/015)

- **Given** N due markets and two replicas running one poll cycle each concurrently
- **When** both run the marker-setting SKIP-LOCKED claim
- **Then** each due market is claimed by exactly one replica (disjoint sets); a market
  whose `live_poll_claimed_until <= now()` (crashed owner) is re-claimable by either.

## Scenario 6 — Queue worker claims oldest pending / lease-expired (REQ-SCHED-010/011/014)

- **Given** `collection_queue` with a pending row and a stale `claimed` row
  (`lease_expires_at < now()`)
- **When** the worker claims
- **Then** it selects the oldest eligible row via `FOR UPDATE SKIP LOCKED LIMIT 1`,
  sets `claimed_by`+`lease_expires_at`, and renews `heartbeat_at`/lease while running.

## Scenario 7 — Queue success/failure transitions (REQ-SCHED-012/013)

- **Given** a claimed queue row
- **When** the work succeeds
- **Then** the data is upserted and the row is `done`; **when** the work fails,
  `attempts` increments and `last_error` is recorded, the row releases for retry while
  `attempts < COLLECTION_MAX_ATTEMPTS`, and becomes `failed` at the max.

## Scenario 8 — Backfill resumes from cursor after crash (REQ-SCHED-021/022)

- **Given** a partially-completed `backfill_chunks` row with a `cursor` mid-range, whose
  lease has expired
- **When** another replica re-claims it
- **Then** it resumes fetching from `cursor` (not the range start), upserts the
  remaining candles, and advances `cursor`; on full completion the chunk is `done`.

## Scenario 9 — Idempotent enqueue on registration (REQ-SCHED-030)

- **Given** a coin/market registered twice
- **When** the registration path enqueues collection + backfill work
- **Then** the `collection_queue` dedup index and `backfill_jobs` UNIQUE constraint make
  the second enqueue a no-op (no duplicate work items).

## Scenario 10 — Metadata revision rule on refresh (REQ-SCHED-031/042)

- **Given** a coin whose metadata is re-collected
- **When** a tracked value (e.g. category) has changed
- **Then** a new `coin_metadata` revision is inserted; and **when** nothing tracked has
  changed, only `last_seen_at` advances on the existing revision (no new row).

## Scenario 11 — Exactly-once persistence on re-execution (REQ-SCHED-040)

- **Given** a work unit that crashed after fetching but before marking `done`, then is
  re-claimed and re-executed
- **When** it persists again
- **Then** the natural-key upserts overwrite the same rows (`(market_id, ts[,interval])`
  etc.), producing no duplicate rows.

## Scenario 12 — Every upstream call paces; none inside a claim tx (REQ-SCHED-041)

- **Given** any worker's collection path
- **When** inspected (structural test) and exercised
- **Then** each outbound provider call is preceded by an `acquire_slot`, and no
  upstream call occurs inside an open claim transaction.

## Scenario 13 — Worker failure isolation (REQ-SCHED-051)

- **Given** a forced panic/error in one work unit
- **When** it occurs
- **Then** the process does not crash, the other workers keep running, and the failing
  unit is recorded/retried per its queue semantics.

## Scenario 14 — Graceful stop on shutdown (REQ-SCHED-050)

- **Given** running workers
- **When** the process receives SIGTERM (cancellation token fired)
- **Then** each worker stops claiming new work, finishes or releases its in-flight unit,
  and exits cleanly within the drain window (owned by SPEC-OBS-001).

## Quality Gate / Definition of Done

- [ ] Three workers spawn at startup; continuous; no calendar gate (1).
- [ ] Poller claim: marker-on-claim, not cursor, short tx before I/O; NULL→global; 
      success advances, transient failure fast-retries; cross-replica dedup (2–5).
- [ ] Queue worker: SKIP-LOCKED oldest claim, lease/heartbeat, done/attempts/failed
      (6, 7).
- [ ] Backfill resumes from durable cursor after crash (8).
- [ ] Idempotent enqueue; metadata revision rule (9, 10).
- [ ] Exactly-once persistence via natural-key upserts (11).
- [ ] All upstream calls pace; none inside a claim tx (12).
- [ ] Worker failure isolation; graceful stop (13, 14).
- [ ] `cargo sqlx prepare` verified against live Postgres.
- [ ] Open items OR-SCHED-1..4 resolved or explicitly deferred with user sign-off.
