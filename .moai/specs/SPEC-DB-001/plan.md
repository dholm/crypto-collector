# Implementation Plan — SPEC-DB-001 (PostgreSQL Schema & Coordination Tables)

Research: [../SPEC-PROV-001/research.md](../SPEC-PROV-001/research.md).
Sibling patterns (adapted): `ticker-collector` migrations 0008/0009 (partitioned
time-series), 0010 (revision), 0016 (backfill), 0017 (pacer), 0019
(collection_queue).
Methodology: greenfield TDD — schema-shape tests + live-Postgres integration tests
that apply the migration set and assert table/column/index/partition existence.

## Technical Approach

Author an ordered set of sqlx SQL migrations under `migrations/`, executed on startup
by the pool initializer (SPEC-OBS-001 owns the runner). Each migration is
object-idempotent (`IF NOT EXISTS`). Group logically so a migration maps to one
cohesive concern, mirroring the ticker-collector numbering style.

Suggested migration ordering (numbers confirmed at run):

| Migration | Creates |
|---|---|
| `0001_registries.sql` | `tracked_coins`, `tracked_markets` (incl. live-poller columns `last_polled_at`/`live_poll_claimed_until`/`live_poll_interval INTERVAL` and `status` domain `active`/`paused`/`error`) + unique index on `(base,quote,COALESCE(venue,''))` + partial live-poll claim index `(last_polled_at) WHERE status='active'`. |
| `0002_live_quotes.sql` | `live_quotes` partitioned parent + monthly partitions + `btree(market_id,ts DESC)` + `BRIN(ts)`. |
| `0003_candles.sql` | `candles` partitioned parent (PK incl. `interval`) + monthly partitions + indexes. |
| `0004_coin_market_snapshots.sql` | `coin_market_snapshots` partitioned parent + monthly partitions + indexes. |
| `0005_derivatives_quotes.sql` | `derivatives_quotes` partitioned parent + monthly partitions + indexes. |
| `0006_coin_metadata.sql` | `coin_metadata` revisioned table + as-of index. |
| `0007_collection_queue.sql` | `collection_queue` + dedup partial-unique + pending-path claim index `(enqueued_at) WHERE status='pending'` + lease-expired re-claim index `(lease_expires_at) WHERE status IN ('claimed','running')`. |
| `0008_backfill.sql` | `backfill_jobs` (UNIQUE) + `backfill_chunks` + claim index. |
| `0009_upstream_pacer.sql` | `upstream_request_pacer` + seed rows per provider. |

All monetary/quantity columns are `NUMERIC`; all timestamps `TIMESTAMPTZ`.

## Files (anticipated; confirmed in run phase)

| File | Change |
|---|---|
| `migrations/0001_registries.sql` … `0009_upstream_pacer.sql` (new) | The DDL above. |
| `src/db/pool.rs` (new; runner owned by SPEC-OBS-001) | Apply migrations on startup. |
| `src/models/*.rs` (new) | Rust structs mirroring rows with `rust_decimal::Decimal` and `chrono::DateTime<Utc>` fields — defined here as the type contract, populated by SPEC-PROV/SCHED/API. |
| `.sqlx/` (generated) | `cargo sqlx prepare` offline cache, regenerated against a live DB after the migrations land. |

## Milestones (priority-ordered, no time estimates)

### Milestone 1 — registries (Priority High)
- RED: integration test applies `0001` and asserts `tracked_coins` PK, `tracked_markets`
  columns (including the live-poller columns `last_polled_at`/`live_poll_claimed_until`/
  `live_poll_interval INTERVAL`), the `(base,quote,COALESCE(venue,''))` unique constraint
  rejects a duplicate while allowing NULL-venue + named-venue coexistence, and the partial
  live-poll claim index `(last_polled_at) WHERE status='active'` exists
  (REQ-DB-001..003, REQ-DB-005).
- GREEN: author `0001_registries.sql`.

### Milestone 2 — partitioned time-series tables (Priority High)
- RED: per table, assert the parent is `PARTITION BY RANGE (ts)`, the PK matches the
  spec, every monetary column is `NUMERIC` (catalog query on `information_schema`), and
  both the `btree(key, ts DESC)` and `BRIN(ts)` indexes exist; assert a row lands in
  the correct monthly partition (REQ-DB-010..015, REQ-DB-040/041).
- GREEN: author `0002`–`0005`.
- Verify a write to a month with a seeded partition succeeds and a write to an
  unseeded month fails loudly (REQ-DB-017, drives OR-DB-3).

### Milestone 3 — revisioned coin_metadata (Priority High)
- RED: assert PK `(coin_id, revision)`, `first_seen_at`/`last_seen_at` present, and the
  as-of index exists (REQ-DB-020/023).
- GREEN: author `0006`.
- Document the revision-insert-vs-advance invariant for SPEC-PROV-001 to implement
  (REQ-DB-021).

### Milestone 4 — coordination tables (Priority High)
- RED: assert `collection_queue` dedup partial-unique (one live row per
  target+kind), the pending-path claim index `(enqueued_at) WHERE status='pending'`, and
  the lease-expired re-claim index `(lease_expires_at) WHERE status IN ('claimed','running')`;
  `backfill_jobs` UNIQUE `(market_id,dataset)` and `backfill_chunks` lease columns;
  `upstream_request_pacer` keyed by provider with seeded rows for all four providers
  (REQ-DB-030..036).
- GREEN: author `0007`–`0009`.

### Milestone 5 — precision & integrity sweep (Priority High)
- A catalog test enumerates every column of every data table and asserts no
  `double precision`/`real` type is used for monetary/quantity columns and that all
  timestamps are `timestamptz` (REQ-DB-040/041).
- Assert every FK declares an `ON DELETE` action (REQ-DB-042).

### Milestone 6 — sqlx offline cache (Priority Medium)
- Run `cargo sqlx prepare` against a **live** Postgres with the full migration set
  applied; commit `.sqlx/`. (Known ticker lesson: offline tests pass with stale column
  refs — must verify live.) (REQ-DB-043)

## Risks

- **sqlx offline drift (highest).** Compile-time-checked queries pass against a stale
  `.sqlx/` cache; every schema change requires `cargo sqlx prepare` against live
  Postgres before merge.
- **Partition coverage gap.** A write to an unseeded future month errors. Mitigation:
  REQ-DB-017 ensure-on-write or ops pre-creation (OR-DB-3); the Milestone 2 test makes
  the failure explicit rather than silent.
- **NUMERIC ↔ Decimal mapping.** Requires the sqlx `rust_decimal` feature; a missing
  feature surfaces as a compile error in SPEC-PROV/SCHED, not here — flagged so the
  Cargo feature set (research §3.1) is correct from the start.
- **Unique index with NULL venue.** Plain `UNIQUE(base,quote,venue)` would treat NULL
  as distinct (allowing duplicate NULL-venue rows); the `COALESCE(venue,'')` expression
  index is required (REQ-DB-003).

## Definition of Done

- All migrations `0001`–`0009` apply cleanly on an empty database and are
  object-idempotent on re-apply.
- All EARS requirements REQ-DB-001..043 covered by schema-shape and/or live-DB tests.
- Precision sweep confirms zero `DOUBLE PRECISION` monetary columns; all timestamps
  `TIMESTAMPTZ`.
- `cargo sqlx prepare` run and verified against a live Postgres; `.sqlx/` committed.
- Acceptance scenarios in `acceptance.md` pass.
- Open items OR-DB-1..4 resolved or explicitly deferred with user sign-off.
