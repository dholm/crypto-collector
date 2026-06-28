# Acceptance Criteria — SPEC-DB-001 (PostgreSQL Schema & Coordination Tables)

Each scenario maps to EARS requirements in `spec.md`. DB-backed scenarios apply the
migration set to a live (or ephemeral container) Postgres and inspect the catalog;
they may be gated (`#[ignore]`) when no database is available, mirroring
`ticker-collector` integration tests.

## Scenario 1 — Two registries created with correct keys (REQ-DB-001/002)

- **Given** an empty database
- **When** migration `0001_registries.sql` is applied
- **Then** `tracked_coins` exists with `coin_id TEXT` primary key; `tracked_markets`
  exists with a surrogate `id` primary key, `base`/`quote` `TEXT NOT NULL`, nullable
  `venue`, nullable `coin_id` FK to `tracked_coins`, and a `kind` column.

## Scenario 2 — Pair uniqueness allows NULL-venue + named-venue coexistence (REQ-DB-003)

- **Given** the `tracked_markets` table
- **When** inserting `(BTC, USD, NULL)`, then `(BTC, USD, binance)`, then a second
  `(BTC, USD, NULL)`
- **Then** the first two inserts succeed (aggregator and venue rows coexist) and the
  third is rejected by the `(base, quote, COALESCE(venue,''))` unique index.

## Scenario 3 — No equities machinery present (REQ-DB-004)

- **Given** the full migration set applied
- **When** the catalog is inspected
- **Then** there is no exchange/MIC table, no holiday/calendar table, no market-phase
  or trading-halt table, and no market-open/close or close-grace column on any table.

## Scenario 4 — Time-series tables are monthly RANGE-partitioned with both indexes (REQ-DB-014/015)

- **Given** the migration set applied
- **When** inspecting `live_quotes`, `candles`, `coin_market_snapshots`, and
  `derivatives_quotes`
- **Then** each parent is `PARTITION BY RANGE (ts)` with monthly UTC partitions, and
  each declares a `btree(<key>, ts DESC)` index and a `BRIN(ts)` index inherited by
  child partitions.

## Scenario 5 — Candle PK includes interval; volume nullable (REQ-DB-011)

- **Given** the `candles` table
- **When** inserting a `1m` and a `1d` candle for the same `(market_id, ts)`, and a
  candle with `volume = NULL`
- **Then** both intervals coexist (PK is `(market_id, interval, ts)`) and the
  NULL-volume row is accepted.

## Scenario 6 — Derivatives observables in one tick (REQ-DB-013)

- **Given** the `derivatives_quotes` table
- **When** inspecting its columns
- **Then** `funding_rate`, `open_interest`, `open_interest_usd`, `mark_price`,
  `index_price`, and `basis` are all present as `NUMERIC` on the single
  `(market_id, ts)`-keyed row (no separate funding/OI tables).

## Scenario 7 — Coin aggregates are time-series, not revisions (REQ-DB-012/022)

- **Given** `coin_market_snapshots` and `coin_metadata`
- **When** inspecting their columns
- **Then** `market_cap`, `fully_diluted_valuation`, `circulating_supply`,
  `total_supply`, and price live on the partitioned `coin_market_snapshots`
  (keyed `(coin_id, vs_currency, ts)`), and `coin_metadata` carries no such
  continuously-changing aggregate column.

## Scenario 8 — Revision table shape and as-of index (REQ-DB-020/023)

- **Given** `coin_metadata`
- **When** inspecting it
- **Then** the PK is `(coin_id, revision)`, `first_seen_at`/`last_seen_at` are
  `TIMESTAMPTZ`, and an as-of index `btree(coin_id, first_seen_at DESC)` exists.

## Scenario 9 — collection_queue dedup + claim indexes (REQ-DB-030/031/032/036)

- **Given** `collection_queue`
- **When** enqueuing two live items with the same `(target_kind, target_id, kind)`
- **Then** the second insert is rejected by the partial unique index restricted to
  `status IN ('pending','claimed','running')`; a partial claim index on
  `(enqueued_at) WHERE status='pending'` (pending path) and a partial re-claim index on
  `(lease_expires_at) WHERE status IN ('claimed','running')` (lease-expired path) both
  exist; once the first item reaches `done`/`failed`, a new live item for the same key
  can be enqueued.

## Scenario 10 — Backfill idempotent enqueue + lease columns (REQ-DB-033)

- **Given** `backfill_jobs` and `backfill_chunks`
- **When** enqueuing the same `(market_id, dataset)` twice
- **Then** the UNIQUE constraint makes the second a no-op; and `backfill_chunks`
  carries `range_start`, `range_end`, `cursor`, `claimed_by`, `lease_expires_at`,
  `heartbeat_at`, `attempts`, and `last_error`.

## Scenario 11 — Per-provider pacer seeded (REQ-DB-034/035)

- **Given** migration `0009_upstream_pacer.sql` applied
- **When** selecting from `upstream_request_pacer`
- **Then** the PK is `provider`, the columns `next_allowed_at`, `min_gap_ms`,
  `cooldown_until`, `credit_window_start`, `credits_used`, `credit_limit` exist, and
  one row exists for each of `coingecko`, `binance`, `coinbase`, `kraken` so a consumer
  can `UPDATE … RETURNING` without a prior `INSERT`.

## Scenario 12 — Precision and time-type sweep (REQ-DB-040/041)

- **Given** the full migration set applied
- **When** enumerating every column of every data table via `information_schema`
- **Then** no monetary/quantity column uses `double precision` or `real` (all
  `numeric`), and every timestamp column is `timestamp with time zone`.

## Scenario 13 — Unseeded-month write fails loudly, not silently (REQ-DB-017)

- **Given** monthly partitions seeded only through next year
- **When** inserting a `live_quotes` row with a `ts` beyond the last partition
- **Then** the insert raises a partition-routing error (no default catch-all partition
  silently swallows it), confirming the ensure-partition-before-write requirement.

## Scenario 14 — Migrations idempotent and sqlx-prepared (REQ-DB-043)

- **Given** the migration set already applied
- **When** the set is re-applied
- **Then** every statement is a no-op (`IF NOT EXISTS`), and `cargo sqlx prepare`
  against the live schema produces a `.sqlx/` cache with no errors.

## Scenario 15 — Live-poller contract columns and claim index present (REQ-DB-002/005)

- **Given** migration `0001_registries.sql` applied
- **When** the `tracked_markets` catalog is inspected
- **Then** the nullable columns `last_polled_at` (`timestamptz`),
  `live_poll_claimed_until` (`timestamptz`), and `live_poll_interval` (`interval`) all
  exist, the `status` column accepts only `active`/`paused`/`error`, and a partial index
  on `(last_polled_at) WHERE status = 'active'` exists — so SPEC-SCHED-001's poller can
  run its due-and-not-in-flight claim query against defined columns and an index.

## Quality Gate / Definition of Done

- [ ] All migrations apply on an empty DB and are idempotent on re-apply (Scenario 14).
- [ ] Live-poller contract columns (`last_polled_at`, `live_poll_claimed_until`,
      `live_poll_interval`) + `(last_polled_at) WHERE status='active'` claim index present
      (15).
- [ ] Two registries with correct keys; pair uniqueness handles NULL venue (1, 2).
- [ ] No equities calendar/MIC/market-phase machinery (3).
- [ ] Four monthly-partitioned time-series tables with `btree(key, ts DESC)` + `BRIN(ts)`
      (4, 5, 6, 7).
- [ ] Revisioned `coin_metadata` with as-of index; aggregates kept time-series (7, 8).
- [ ] collection_queue dedup + claim indexes (pending + lease-expired); backfill lease
      columns; per-provider seeded pacer (9, 10, 11).
- [ ] Precision sweep: all monetary columns `NUMERIC`, all timestamps `TIMESTAMPTZ`
      (12).
- [ ] Unseeded-month write fails loudly (13).
- [ ] `cargo sqlx prepare` verified against a **live** Postgres; `.sqlx/` committed.
- [ ] Open items OR-DB-1..4 resolved or explicitly deferred with user sign-off.
