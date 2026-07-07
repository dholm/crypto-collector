---
id: SCHED-001
version: 1.1.0
status: planned
created: 2026-06-28
updated: 2026-06-28
author: dholm
priority: high
issue_number: null
---

# SPEC-SCHED-001 — Background Collection Workers & Multi-Replica Coordination

Foundation SPEC for continuous data collection. Defines the three background workers
(live-quote poller, collection-queue worker, historical backfill worker), their
lease/heartbeat/retry coordination, and the multi-replica safety (exactly-once
persistence) guarantees that let N stateless replicas divide work without duplication.

Schema contract: [SPEC-DB-001](../SPEC-DB-001/spec.md) (`collection_queue`,
`backfill_*`, the data tables, the registries). Upstream contract:
[SPEC-PROV-001](../SPEC-PROV-001/spec.md) (the `Provider` chain + pacer).
Research: [../SPEC-PROV-001/research.md](../SPEC-PROV-001/research.md) (§6 worker
safety, §1.1 24/7 no-calendar).

## HISTORY

- 2026-07-07 (v1.2.0): Added a **deep-history daily backfill** alongside the once-per-coin
  startup backfill. `enqueue_deep_history_backfills` enqueues an idempotent `1d` job (new
  dataset tag `candles_deep_1d`, distinct from `candles` under the
  `ON CONFLICT (coin_id, dataset)` key) for configured coins (default `bitcoin`) over
  `[DEEP_BACKFILL_START_DATE, now]` — a complete standalone daily series: a deep-history
  source serves the pre-2017 years Binance cannot (Bitstamp BTC/USD daily from 2011-08; see
  SPEC-PROV-001 v1.2.0) and Binance serves `1d` from 2017-08, overlapping the 5m→1d rollup
  harmlessly. Running to `now` (not `now − lookback`) means the daily series does not depend
  on the fine-grained 5m backfill having reached back via Bitstamp. Two supporting changes:
  (1) `process_chunk` now honours a chunk's explicit `interval` column (previously ignored)
  via `resolve_interval_secs`, so the deep job can pin `1d` while startup/legacy chunks keep
  the per-coin poll interval; (2) daily granularity is the only one that reaches 2011 (Bitstamp
  intraday starts ~2013). Env:
  `DEEP_BACKFILL_COINS`, `DEEP_BACKFILL_START_DATE`. Wired into `main.rs` startup (Step 8c),
  fail-soft like the regular backfill.
- 2026-06-28 (v1.1.0): Clarified the "no new schema" Exclusion — the poller's
  `last_polled_at`/`live_poll_claimed_until`/`live_poll_interval` columns are now defined
  by SPEC-DB-001 (REQ-DB-002) and are *consumed* here, not introduced. Relabelled
  REQ-SCHED-008 from Unwanted to Ubiquitous (unconditional negative constraint). (audit C1, m1)
- 2026-06-28 (v1.0.0): Initial greenfield worker SPEC. Three workers spawned at
  startup: a live-quote poller (continuous, cadence-driven, with the SPEC-RT-002
  self-expiring in-flight marker), a collection-queue worker (`FOR UPDATE SKIP LOCKED`
  + lease/heartbeat/attempts), and a backfill worker (claimable chunks with a durable
  resume cursor). No market-hours gate (24/7 markets). Exactly-once persistence via
  idempotent upserts + dedup indexes (research §6).

---

## Goal

Continuously collect all three data domains into PostgreSQL across N stateless
replicas with no duplicated work and no data loss on replica crash, pacing every
upstream call through SPEC-PROV-001, and running 24/7 with no market-hours or calendar
gating.

## Scope

In scope:
- **Live-quote poller**: claims due tracked markets, fetches spot quotes via the
  provider chain, inserts `live_quotes`; per-market optional cadence with a global
  default; self-expiring in-flight marker for cross-replica dedup.
- **Collection-queue worker**: claims `collection_queue` rows
  (`FOR UPDATE SKIP LOCKED`), executes the work (candles / metadata / market /
  derivatives), upserts results, with lease/heartbeat/attempts and failure handling.
- **Backfill worker**: claims `backfill_chunks`, fetches historical OHLC ranges,
  upserts `candles`, advances a durable `cursor`, with the same lease pattern.
- Graceful start (spawn at startup) and stop (cancellation on shutdown — drain handled
  by SPEC-OBS-001).
- The exactly-once-persistence and multi-replica-safety requirements.

Out of scope: see Exclusions. The tables (SPEC-DB-001), the provider chain + pacer
(SPEC-PROV-001), the API (SPEC-API-001), and shutdown/drain orchestration
(SPEC-OBS-001).

## Decisions Restated (authoritative)

- **D1 — Three workers, spawned at startup**, each in its own Tokio task, on every
  replica. (product structure.md `collectors/`)
- **D2 — No market-hours gate.** Collection runs continuously; claim predicates are
  cadence-only. (research §1.1)
- **D3 — SKIP-LOCKED + lease/heartbeat/attempts** for queue and backfill claiming;
  self-expiring in-flight marker for the live poller. (research §6; ticker
  SPEC-RT-002/BACKFILL-001/SCHED-004 patterns)
- **D4 — Exactly-once persistence** via idempotent upserts keyed on
  `(market_id, ts[, interval])` / `(coin_id, vs_currency, ts)` / `(coin_id, revision)`,
  plus the `collection_queue` dedup index — not distributed-transaction delivery.
- **D5 — Every upstream call paces** through SPEC-PROV-001 `acquire_slot`; the claim
  transaction commits and releases locks **before** any network I/O. (ticker REQ-RT-005)

---

## Design Summary (WHAT, not HOW)

1. **Live-quote poller.** A replica-local interval loop ticking at
   `LIVE_QUOTE_POLL_INTERVAL_SECS`. Each tick claims due, active markets in a short
   transaction:
   - due predicate: `last_polled_at IS NULL OR last_polled_at +
     COALESCE(live_poll_interval, <global default>) <= now()`,
   - not-in-flight predicate: `live_poll_claimed_until IS NULL OR
     live_poll_claimed_until <= now()`,
   - `FOR UPDATE SKIP LOCKED`, and `SET live_poll_claimed_until = now() + claim_ttl`
     (NOT `last_polled_at`), committing before any fetch.
   For each claimed market, outside the claim tx: acquire pacer slot, fetch spot via
   the chain, insert `live_quotes`; on success set `last_polled_at = now()` and clear
   the marker; on transient failure clear the marker only (leaving the market due for
   fast retry). A crashed replica's marker self-expires at `claim_ttl`.

2. **Collection-queue worker.** A loop that claims the oldest pending
   `collection_queue` row via `SELECT … WHERE status='pending' OR (status IN
   ('claimed','running') AND lease_expires_at < now()) ORDER BY enqueued_at FOR UPDATE
   SKIP LOCKED LIMIT 1`, marks it `claimed`/`running` with `claimed_by` +
   `lease_expires_at`, and runs the work for its `kind` (candles / metadata / market /
   derivatives) via the provider chain, heartbeating `heartbeat_at`/renewing the lease
   periodically. On success → `done`; on transient failure → increment `attempts`,
   release for retry; on `attempts >= max` → `failed` with `last_error`.

3. **Backfill worker.** Claims a `backfill_chunks` row (same lease pattern), fetches
   the historical OHLC range for its `(market, interval)` via the chain (CoinGecko
   `/ohlc/range` where available, else exchange klines), upserts `candles`, and
   advances the durable `cursor` to the last persisted instant so a re-claim resumes
   rather than restarts. Whole-dataset chunks (NULL range bounds) are single-fetch.

4. **Enqueue triggers.** Registering a coin/market (SPEC-API-001) idempotently
   enqueues its initial collection work and, for markets, a backfill job
   (idempotent via the dedup/UNIQUE indexes). The collection-queue worker also
   re-enqueues periodic refresh work for slowly-changing data (metadata) on a cadence.

5. **Continuous operation.** No worker consults a calendar, market phase, or
   trading-hours gate; the only gates are cadence (poller) and queue contents
   (queue/backfill).

6. **Graceful stop.** Each worker honours a `CancellationToken`; on shutdown it stops
   claiming new work, lets the in-flight unit finish or releases its lease, and exits.
   The drain window and ordering are owned by SPEC-OBS-001.

---

## Requirements (EARS)

### Live-quote poller

- **REQ-SCHED-001** (Ubiquitous): The system shall spawn a live-quote poller at
  startup on every replica that polls tracked, active markets for spot quotes on a
  configurable cadence and inserts results into `live_quotes`.
- **REQ-SCHED-002** (State-Driven): While a market's per-market `live_poll_interval` is
  NULL, the poller shall use the global `LIVE_QUOTE_POLL_INTERVAL_SECS` as that
  market's effective cadence.
- **REQ-SCHED-003** (Event-Driven): When the poller claims due markets, it shall select
  only markets that are due and not in flight, using `FOR UPDATE SKIP LOCKED`, and set
  `live_poll_claimed_until = now() + claim_ttl` within the claim transaction without
  advancing `last_polled_at`.
- **REQ-SCHED-004** (Event-Driven): When the claim transaction completes, it shall
  commit and release row locks before any outbound provider request.
- **REQ-SCHED-005** (Event-Driven): When a claimed market's spot fetch succeeds, the
  poller shall insert the quote, set `last_polled_at = now()`, and clear
  `live_poll_claimed_until`.
- **REQ-SCHED-006** (If/Unwanted): If a claimed market's spot fetch fails transiently,
  then the poller shall clear `live_poll_claimed_until`, leave `last_polled_at`
  unchanged, and leave the market due for re-poll on the next cycle.
- **REQ-SCHED-007** (State-Driven): While a claimed market's `live_poll_claimed_until`
  has not been cleared (owning replica crashed), no other replica shall re-poll it
  until the marker self-expires (`live_poll_claimed_until <= now()`).
- **REQ-SCHED-008** (Ubiquitous): The poller shall not consult any market-hours,
  calendar, market-phase, or trading-halt condition; collection is continuous.

### Collection-queue worker

- **REQ-SCHED-010** (Ubiquitous): The system shall spawn a collection-queue worker at
  startup that claims the oldest pending (or lease-expired) `collection_queue` row via
  `FOR UPDATE SKIP LOCKED` and executes the work for its `kind`.
- **REQ-SCHED-011** (Event-Driven): When a row is claimed, the worker shall set
  `claimed_by`, `lease_expires_at`, and `status='claimed'/'running'`, and shall renew
  `heartbeat_at`/`lease_expires_at` periodically while the work runs.
- **REQ-SCHED-012** (Event-Driven): When the work completes successfully, the worker
  shall upsert the collected data and set the row `status='done'`.
- **REQ-SCHED-013** (If/Unwanted): If the work fails, then the worker shall increment
  `attempts`, record `last_error`, and either release the row for retry (when
  `attempts < max`) or mark it `failed` (when `attempts >= COLLECTION_MAX_ATTEMPTS`).
- **REQ-SCHED-014** (State-Driven): While a claimed row's `lease_expires_at` is in the
  past (owning replica crashed), another replica shall be able to re-claim it via the
  claim predicate.
- **REQ-SCHED-015** (Ubiquitous): The claim shall guarantee that a given queue row is
  executed by at most one replica at a time (`FOR UPDATE SKIP LOCKED` + lease).

### Backfill worker

- **REQ-SCHED-020** (Ubiquitous): The system shall spawn a backfill worker at startup
  that claims `backfill_chunks` rows via the same lease pattern and fetches historical
  OHLC ranges for `(market, interval)` via the provider chain.
- **REQ-SCHED-021** (Event-Driven): When a chunk's range (or sub-range) is persisted,
  the worker shall advance the chunk's durable `cursor` to the last persisted instant.
- **REQ-SCHED-022** (State-Driven): While a previously-claimed chunk is re-claimed
  after a crash, the worker shall resume from the chunk's `cursor` rather than
  re-fetching the whole range.
- **REQ-SCHED-023** (Event-Driven): When a chunk is fully persisted, the worker shall
  mark it `done`; on repeated failure it shall mark it `failed` after
  `BACKFILL_MAX_ATTEMPTS`.

### Enqueue and cadence

- **REQ-SCHED-030** (Event-Driven): When a coin or market is registered (SPEC-API-001),
  the system shall idempotently enqueue its initial collection work (and, for markets,
  a backfill job), relying on the `collection_queue` dedup index and the
  `backfill_jobs` UNIQUE constraint to avoid duplicates.
- **REQ-SCHED-031** (Ubiquitous): The system shall periodically enqueue refresh work
  for slowly-changing data (coin metadata) on a configurable cadence, deduplicated per
  target+kind.

### Exactly-once persistence and pacing

- **REQ-SCHED-040** (Ubiquitous): All collected data shall be persisted via idempotent
  upserts keyed on the natural keys (`(market_id, ts)`, `(market_id, interval, ts)`,
  `(coin_id, vs_currency, ts)`, `(coin_id, revision)`), so re-executing a
  claimed-but-crashed unit re-writes identical rows rather than duplicating them.
- **REQ-SCHED-041** (Ubiquitous): Before any outbound provider request, every worker
  shall acquire a pacer slot via SPEC-PROV-001, and shall make no upstream call inside
  an open claim transaction.
- **REQ-SCHED-042** (Event-Driven): When coin metadata is upserted and a tracked value
  has changed, the worker shall insert a new `coin_metadata` revision; when unchanged,
  it shall advance `last_seen_at` only (SPEC-DB-001 REQ-DB-021).

### Lifecycle

- **REQ-SCHED-050** (Event-Driven): When the process receives a shutdown signal, each
  worker shall stop claiming new work, finish or release its in-flight unit, and exit
  cleanly under the drain window owned by SPEC-OBS-001.
- **REQ-SCHED-051** (Ubiquitous): Worker failures shall be isolated — a panic or error
  in one worker or one work unit shall not crash the process or stop the other workers.

## Exclusions (What NOT to Build)

- **No market-hours/calendar gating** — collection is continuous; no phase, holiday,
  halt, or close-grace logic (REQ-SCHED-008; research §1.1).
- **No distributed consensus / external lock service** — coordination is PostgreSQL
  `FOR UPDATE SKIP LOCKED` + leases only (research §6).
- **No new schema** — workers *consume* SPEC-DB-001 tables and columns exactly,
  including the live-poller columns (`last_polled_at`, `live_poll_claimed_until`,
  `live_poll_interval`) now defined in SPEC-DB-001 REQ-DB-002; they do not introduce
  them. Any schema change is a SPEC-DB-001 amendment.
- **No direct upstream HTTP** — all upstream access goes through the SPEC-PROV-001
  chain + pacer (REQ-SCHED-041); workers never call `reqwest` directly.
- **No WebSocket/streaming push** and **no `pg_notify` fan-out** in foundation scope.
- **No in-process cache of collected data** — statelessness requires all state in
  PostgreSQL (product.md).
- **No per-market background task per market** — one shared loop per worker with
  predicate-driven selection (the SPEC-RT-002 anti-pattern guard).

## @MX Annotation Targets (high fan_in)

- The live-poller claim SQL — `@MX:ANCHOR` + `@MX:WARN`/`@MX:REASON`: due+not-in-flight
  predicate, marker-on-claim (not cursor), short tx committing before network I/O
  (REQ-SCHED-003/004/007).
- The collection-queue / backfill claim SQL — `@MX:ANCHOR` on the
  `FOR UPDATE SKIP LOCKED` + lease-expiry claim shape (REQ-SCHED-010/014/020).
- The success/transient-failure cursor-and-marker UPDATEs — `@MX:WARN`/`@MX:REASON`:
  schedule advances only on reached success; transient failure clears the marker
  without advancing (REQ-SCHED-005/006).
- The idempotent-upsert helpers — `@MX:NOTE` on the natural-key conflict targets that
  give exactly-once persistence (REQ-SCHED-040).

## Open Items (do not guess)

- **OR-SCHED-1:** default numbers for cadence/lease knobs
  (`LIVE_QUOTE_POLL_INTERVAL_SECS`, `LIVE_POLL_MIN/MAX_INTERVAL_SECS`,
  `LIVE_POLL_CLAIM_TTL_SECS`, `COLLECTION_LEASE_SECONDS`,
  `COLLECTION_HEARTBEAT_INTERVAL_SECONDS`, `COLLECTION_MAX_ATTEMPTS`,
  `BACKFILL_LEASE_SECONDS`, `BACKFILL_MAX_ATTEMPTS`). Rules normative; numbers at run
  (must respect the CoinGecko tier budget — SPEC-PROV-001 OR-PROV-2).
- **OR-SCHED-2:** which `collection_queue` kinds the poller vs the queue worker own
  (recommend: spot → poller; candles/metadata/market/derivatives → queue worker).
  Confirm at run.
- **OR-SCHED-3:** metadata refresh cadence (REQ-SCHED-031) default. Run decision.
- **OR-SCHED-4 (= OR-DB-2/OR-PROV-4):** candle volume policy affects whether the
  backfill worker requires an exchange provider for volume-bearing candles. Run.
