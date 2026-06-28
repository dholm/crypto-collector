# Implementation Plan — SPEC-SCHED-001 (Background Collection Workers)

Schema: [../SPEC-DB-001/spec.md](../SPEC-DB-001/spec.md). Upstream:
[../SPEC-PROV-001/spec.md](../SPEC-PROV-001/spec.md).
Research: [../SPEC-PROV-001/research.md](../SPEC-PROV-001/research.md) (§6, §1.1).
Methodology: greenfield TDD — SQL-shape tests for claim queries (mirroring ticker
`live_poller_claim_sql_*`) + gated live-DB integration tests for concurrency.

## Technical Approach

Three Tokio tasks spawned at startup, each driven by a `CancellationToken`. Each
worker shares two disciplines proven in `ticker-collector`:

1. **Claim in a short transaction that commits before network I/O** (RT-005). The
   claim sets ownership (marker for the poller; `claimed_by`+`lease_expires_at` for the
   queue/backfill), commits, releases locks, then the fetch loop runs outside the tx.
2. **Pace every upstream call** via SPEC-PROV-001 `acquire_slot`, never inside the
   claim tx.

Persistence is idempotent upsert on natural keys, giving exactly-once persistence
across crashes/re-claims.

## Files (anticipated; confirmed in run phase)

| File | Change |
|---|---|
| `src/collectors/mod.rs` (new) | Worker spawn/registry; `CancellationToken` wiring; failure isolation. |
| `src/collectors/live_poller.rs` (new) | Claim SQL (due+not-in-flight, marker-on-claim), fetch loop, success/failure cursor+marker UPDATEs. |
| `src/collectors/collection_queue.rs` (new) | SKIP-LOCKED claim, lease/heartbeat renew, kind dispatch, attempts/failed handling, enqueue helpers. |
| `src/collectors/backfill.rs` (new) | Chunk claim, range fetch via chain, candle upsert, durable cursor advance. |
| `src/db/*.rs` (new) | Idempotent upsert helpers (natural-key conflict targets) + revision upsert for `coin_metadata`. |
| `src/config.rs` (shared) | Cadence/lease knobs (OR-SCHED-1). |

## Milestones (priority-ordered, no time estimates)

### Milestone 1 — live-quote poller (Priority High)
- RED: SQL-shape test asserts the claim has `FOR UPDATE SKIP LOCKED`, `status='active'`,
  the due+not-in-flight predicate, `SET live_poll_claimed_until = now()+$ttl` and does
  NOT set `last_polled_at`; the claim tx commits before the fetch loop (REQ-SCHED-003/004).
  Tests for success (cursor advances, marker cleared) vs transient failure (marker
  cleared, cursor unchanged) (REQ-SCHED-005/006). NULL interval → global default
  (REQ-SCHED-002).
- GREEN: implement the loop, claim, and post-attempt UPDATEs; pace each fetch.
- Gated DB test: two concurrent claimers produce disjoint sets via the marker; expired
  marker re-claimable (REQ-SCHED-007).

### Milestone 2 — collection-queue worker (Priority High)
- RED: claim SQL shape (oldest pending or lease-expired, SKIP LOCKED, LIMIT 1);
  lease/heartbeat renew; success→done; failure→attempts++/failed at max
  (REQ-SCHED-010..015).
- GREEN: claim + dispatch by kind (candles/metadata/market/derivatives) via the chain
  + idempotent upserts; metadata revision upsert (REQ-SCHED-042).

### Milestone 3 — backfill worker (Priority High)
- RED: chunk claim (lease pattern); cursor advance on partial persistence; resume from
  cursor on re-claim; done/failed transitions (REQ-SCHED-020..023).
- GREEN: range fetch via chain (`/ohlc/range` where available, else exchange klines);
  candle upsert; cursor advance.

### Milestone 4 — enqueue triggers + cadence (Priority Medium)
- RED: registering a coin/market idempotently enqueues initial work + backfill job
  (dedup/UNIQUE prevents duplicates) (REQ-SCHED-030); periodic metadata refresh
  enqueues deduplicated work (REQ-SCHED-031).
- GREEN: enqueue helpers wired into the API registration path and a refresh ticker.

### Milestone 5 — exactly-once persistence + pacing sweep (Priority High)
- RED: re-executing a claimed-then-crashed unit re-writes identical rows (no
  duplicates) via natural-key upsert (REQ-SCHED-040); a structural test asserts no
  worker issues an upstream call inside an open claim tx and every call paces
  (REQ-SCHED-041).
- GREEN: upsert helpers + pacing call sites.

### Milestone 6 — lifecycle + isolation (Priority Medium)
- RED: a forced panic in one work unit does not crash the process or stop other workers
  (REQ-SCHED-051); on cancellation each worker stops claiming and exits (REQ-SCHED-050).
- GREEN: `catch_unwind`/`JoinSet` supervision + `CancellationToken` handling.

## Risks

- **Short-tx discipline (highest).** An upstream call inside the claim tx holds locks
  across network I/O, serialising the fleet and risking lock pileups. SQL-shape +
  structural tests guard that the claim returns owned IDs and commits before fetching
  (REQ-SCHED-004/041) — the exact ticker RT-005 lesson.
- **Failure-path cursor correctness.** Advancing `last_polled_at` (or a chunk cursor)
  on a transient failure silently drops retry resilience; success-only advance is
  asserted distinctly (REQ-SCHED-005/006/021).
- **Duplicate work under races.** The dedup index (`collection_queue`) + UNIQUE
  (`backfill_jobs`) + SKIP-LOCKED make claiming and enqueue idempotent; concurrency
  tests assert disjoint claims (REQ-SCHED-015/030).
- **Pacer budget vs cadence.** Aggressive cadence defaults exhaust the CoinGecko Demo
  monthly credit; defaults (OR-SCHED-1) must respect the tier budget (SPEC-PROV-001).
- **Crash recovery latency.** Lease/marker TTLs trade re-claim delay against
  double-work risk; choose modest defaults exceeding one paced fetch.

## Definition of Done

- Three workers spawn at startup, run continuously with no calendar gate, and stop
  cleanly on cancellation.
- Claims use SKIP-LOCKED + lease/marker; claim tx commits before any paced upstream
  call; success-only schedule advance; transient failure fast-retry.
- Exactly-once persistence via natural-key upserts; metadata revision rule honoured.
- Enqueue on registration is idempotent; periodic metadata refresh deduplicated.
- Worker failures isolated.
- All EARS REQ-SCHED-001..051 covered by SQL-shape + gated concurrency tests.
- `cargo sqlx prepare` verified against live Postgres.
- Open items OR-SCHED-1..4 resolved or explicitly deferred with user sign-off.
