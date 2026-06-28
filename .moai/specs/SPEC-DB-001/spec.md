---
id: DB-001
version: 1.1.0
status: planned
created: 2026-06-28
updated: 2026-06-28
author: dholm
priority: high
issue_number: null
---

# SPEC-DB-001 — PostgreSQL Schema & Multi-Replica Coordination Tables

Foundation SPEC for the Crypto Collector data layer. Defines every persistent table:
the two asset registries, the four partitioned time-series tables, the revisioned
coin-metadata table, and the three coordination tables (collection queue, backfill,
upstream pacer).

Sibling reference (patterns adapted, never copied): `ticker-collector`
SPEC-DB-002 (partitioned candles/live_quotes), SPEC-FUND-002 (revision pattern),
SPEC-BACKFILL-001 (backfill queue), SPEC-COLL-004 (pacer), SPEC-SCHED-004
(collection_queue).
Research: [../SPEC-PROV-001/research.md](../SPEC-PROV-001/research.md) (§4 schema
rationale, §1.5/§3.4 precision).
Consumers: [SPEC-PROV-001](../SPEC-PROV-001/spec.md), [SPEC-SCHED-001](../SPEC-SCHED-001/spec.md),
[SPEC-API-001](../SPEC-API-001/spec.md).

## HISTORY

- 2026-06-28 (v1.1.0): Defined the live-poller contract columns on `tracked_markets`
  (`last_polled_at`, `live_poll_claimed_until`, `live_poll_interval`) that SPEC-SCHED-001's
  poller claim query consumes, plus the partial claim index `(last_polled_at)
  WHERE status = 'active'`; enumerated the `tracked_markets.status` domain
  (`active`/`paused`/`error`); added a `collection_queue` lease-expired re-claim index
  `(lease_expires_at) WHERE status IN ('claimed','running')`. (audit C1, m2, m7)
- 2026-06-28 (v1.0.0): Initial greenfield schema SPEC. Establishes two registries
  (`tracked_coins`, `tracked_markets`); four monthly RANGE-partitioned time-series
  tables (`live_quotes`, `candles`, `coin_market_snapshots`, `derivatives_quotes`)
  with `btree(key, ts DESC)` + `BRIN(ts)` indexes; one revisioned `coin_metadata`
  table (`first_seen_at`/`last_seen_at`/`revision`); and three coordination tables
  (`collection_queue`, `backfill_jobs`+`backfill_chunks`, `upstream_request_pacer`).
  All monetary/quantity columns are `NUMERIC` (not `DOUBLE PRECISION`) for crypto
  precision. No equities calendar/MIC/market-phase tables (deliberately dropped).

---

## Goal

Provide a complete, normalised, multi-replica-safe PostgreSQL schema (as sqlx
migrations executed on startup) that stores all three crypto data domains (spot +
OHLCV, metadata + tokenomics, derivatives) with exact decimal precision, supports
keyset-paginated time-range reads efficiently, and gives stateless replicas the
coordination tables they need to divide work without duplication.

## Scope

In scope:
- Two asset registries: `tracked_coins` (coin-keyed) and `tracked_markets`
  (pair-keyed, `base`/`quote`/optional `venue`).
- Four monthly RANGE-partitioned time-series tables: `live_quotes`, `candles`,
  `coin_market_snapshots`, `derivatives_quotes`.
- One revisioned table: `coin_metadata`.
- Three coordination tables: `collection_queue`, `backfill_jobs` + `backfill_chunks`,
  `upstream_request_pacer`.
- Partitioning strategy, index strategy, primary/foreign keys, and the precision
  (`NUMERIC`) and time (`TIMESTAMPTZ`) type rules.
- Migrations runnable on startup, compatible with sqlx compile-time-checked queries.

Out of scope: see Exclusions. Query implementations, worker logic, and API handlers
belong to SPEC-SCHED-001 / SPEC-API-001 / SPEC-PROV-001.

## Decisions Restated (authoritative, from product brief + research)

- **D1 — Two registries.** Coin-level aggregates (metadata/tokenomics) are keyed by
  `coin_id`; pair-level data (spot/candles/derivatives) is keyed by a `market_id`
  identifying `(base, quote, venue?)`. (research §1.2–1.3, §4.1)
- **D2 — Precision: `NUMERIC`, never `DOUBLE PRECISION`,** for all prices, OHLC,
  volume, supply, market cap, FDV, funding, open interest, mark/index. (research
  §1.5, §3.4)
- **D3 — Partitioning:** monthly RANGE on `ts` for the four time-series tables, with
  `btree(key, ts DESC)` + `BRIN(ts)` parent indexes inherited by child partitions.
  (research §4.2)
- **D4 — Revision vs time-series split:** slowly-changing descriptive coin metadata
  uses the `first_seen_at`/`last_seen_at`/`revision` pattern; continuously-changing
  supply/cap/FDV are a partitioned time-series, NOT revisioned. (research §4.3)
- **D5 — Per-provider, credit-aware pacer:** one `upstream_request_pacer` row per
  provider, tracking `next_allowed_at`/`min_gap_ms`/`cooldown_until` and a monthly
  credit budget. (research §2.3, §4.4)
- **D6 — No equities machinery:** no exchanges/MIC, holidays, market-phase, or
  trading-halt tables. (research §1.1)

---

## Design Summary (WHAT, not HOW)

### Registries

1. **`tracked_coins`** — coin-keyed registry. Key `coin_id TEXT PK` (provider coin id,
   e.g. CoinGecko `"bitcoin"`). Columns: `symbol`, `name`, `status`
   (`active`/`paused`/`error`), `registered_at TIMESTAMPTZ`, `last_collected_at
   TIMESTAMPTZ NULL`, `error TEXT NULL`. The unit of metadata/tokenomics collection.

2. **`tracked_markets`** — pair-keyed registry. `id BIGSERIAL PK`; `base TEXT`,
   `quote TEXT`, `venue TEXT NULL` (NULL = aggregator/cross-venue), `coin_id TEXT NULL
   REFERENCES tracked_coins(coin_id) ON DELETE SET NULL` (links the base asset to its
   coin record), `kind TEXT` (`spot`/`derivative`), `status` (`active`/`paused`/`error`),
   `registered_at`, `last_collected_at`, `error`, plus the **live-poller contract
   columns** consumed by SPEC-SCHED-001's poller: `last_polled_at TIMESTAMPTZ NULL`,
   `live_poll_claimed_until TIMESTAMPTZ NULL` (self-expiring in-flight marker), and
   `live_poll_interval INTERVAL NULL` (per-market cadence override; NULL = use the global
   `LIVE_QUOTE_POLL_INTERVAL_SECS`). **Unique** on `(base, quote, COALESCE(venue, ''))` so
   an aggregator row (NULL venue) and venue-specific rows coexist for the same pair.
   A partial claim index `(last_polled_at) WHERE status = 'active'` serves the poller's
   due-and-not-in-flight claim query.

### Time-series tables (monthly RANGE-partitioned by `ts`)

3. **`live_quotes`** — latest spot snapshots. PK `(market_id, ts)`. Columns:
   `as_of TIMESTAMPTZ NULL` (provider quote instant, distinct from capture `ts`),
   `price NUMERIC`, `bid NUMERIC NULL`, `ask NUMERIC NULL`, `bid_size NUMERIC NULL`,
   `ask_size NUMERIC NULL`, `volume_24h NUMERIC NULL`, `vs_currency TEXT`,
   `source TEXT` (provider name). Indexes: `btree(market_id, ts DESC)`, `BRIN(ts)`.

4. **`candles`** — historical OHLCV. PK `(market_id, interval, ts)` (interval in PK so
   `1m` and `1d` coexist for the same market). Columns: `open NUMERIC`, `high NUMERIC`,
   `low NUMERIC`, `close NUMERIC`, `volume NUMERIC NULL` (NULL when source lacks
   per-candle volume — CoinGecko OHLC; research §2.2), `vs_currency TEXT`,
   `source TEXT`. Indexes: `btree(market_id, interval, ts DESC)`, `BRIN(ts)`.

5. **`coin_market_snapshots`** — continuously-changing coin aggregates. PK
   `(coin_id, vs_currency, ts)`. Columns: `price NUMERIC`, `market_cap NUMERIC NULL`,
   `fully_diluted_valuation NUMERIC NULL`, `circulating_supply NUMERIC NULL`,
   `total_supply NUMERIC NULL`, `volume_24h NUMERIC NULL`, `source TEXT`. Indexes:
   `btree(coin_id, vs_currency, ts DESC)`, `BRIN(ts)`. (research §4.3 — these are
   time-series, NOT revisioned.)

6. **`derivatives_quotes`** — perpetual/futures observables, captured together per
   tick (matches CoinGecko `/derivatives/tickers`). PK `(market_id, ts)`. Columns:
   `funding_rate NUMERIC NULL`, `open_interest NUMERIC NULL`,
   `open_interest_usd NUMERIC NULL`, `mark_price NUMERIC NULL`,
   `index_price NUMERIC NULL`, `basis NUMERIC NULL`, `volume_24h NUMERIC NULL`,
   `contract_type TEXT NULL`, `venue TEXT NULL`, `source TEXT`. Indexes:
   `btree(market_id, ts DESC)`, `BRIN(ts)`. This single table consolidates the
   "funding rate" and "open interest" deliverables (research §1.4) — one atomic tick
   per `(market, ts)` rather than two tables on the same key/cadence.

### Revisioned table

7. **`coin_metadata`** — slowly-changing descriptive metadata. PK
   `(coin_id, revision)`. Columns: `name TEXT`, `symbol TEXT`,
   `categories TEXT[] NULL`, `description TEXT NULL`, `homepage TEXT NULL`,
   `links JSONB NULL`, `contract_addresses JSONB NULL`, `max_supply NUMERIC NULL`,
   `genesis_date DATE NULL`, `first_seen_at TIMESTAMPTZ`, `last_seen_at TIMESTAMPTZ`.
   `revision` is 0-based, incremented only when a tracked value changes
   (`IS NOT DISTINCT FROM` comparison); `last_seen_at` advances on re-confirm without
   a new revision. As-of index `btree(coin_id, first_seen_at DESC)`.

### Coordination tables

8. **`collection_queue`** — durable work queue. `id BIGSERIAL PK`; `target_kind TEXT`
   (`coin`/`market`); `target_id TEXT` (coin_id or market_id as text); `kind TEXT`
   (`spot`/`candles`/`metadata`/`market`/`derivatives`); `status` (`pending`/
   `claimed`/`running`/`done`/`failed`); `claimed_by TEXT NULL`,
   `lease_expires_at TIMESTAMPTZ NULL`, `heartbeat_at TIMESTAMPTZ NULL`,
   `attempts INT DEFAULT 0`, `last_error TEXT NULL`, `enqueued_at`, `updated_at`.
   Partial unique dedup index on `(target_kind, target_id, kind) WHERE status IN
   ('pending','claimed','running')`; partial claim index on
   `(enqueued_at) WHERE status = 'pending'` (pending path) plus a partial re-claim index
   on `(lease_expires_at) WHERE status IN ('claimed','running')` (lease-expired path).

9. **`backfill_jobs`** + **`backfill_chunks`** — historical backfill. `backfill_jobs`:
   `id PK`, `market_id`, `dataset TEXT` (e.g. `candles:1h`), `status`, `requested_at`,
   `updated_at`, UNIQUE `(market_id, dataset)` (idempotent enqueue). `backfill_chunks`:
   `id PK`, `job_id FK`, `market_id`, `dataset`, `interval TEXT NULL`,
   `range_start TIMESTAMPTZ NULL`, `range_end TIMESTAMPTZ NULL`,
   `cursor TIMESTAMPTZ NULL` (durable resume marker), `status`, `claimed_by`,
   `lease_expires_at`, `heartbeat_at`, `attempts`, `last_error`, `created_at`,
   `updated_at`. Partial claim index on `(created_at) WHERE status = 'pending'`.

10. **`upstream_request_pacer`** — per-provider fleet-wide pacer. PK
    `provider TEXT`; `next_allowed_at TIMESTAMPTZ`, `min_gap_ms INT`,
    `cooldown_until TIMESTAMPTZ NULL`, `credit_window_start TIMESTAMPTZ`,
    `credits_used BIGINT DEFAULT 0`, `credit_limit BIGINT NULL` (NULL = unlimited),
    `updated_at`. Seeded with one row per known provider so consumers can `UPDATE …
    RETURNING` without a prior `INSERT`. (research §4.4)

---

## Requirements (EARS)

### Registries

- **REQ-DB-001** (Ubiquitous): The schema shall provide a `tracked_coins` table with
  `coin_id TEXT` primary key and columns `symbol`, `name`, `status`, `registered_at
  TIMESTAMPTZ`, nullable `last_collected_at TIMESTAMPTZ`, and nullable `error`.
- **REQ-DB-002** (Ubiquitous): The schema shall provide a `tracked_markets` table with
  a surrogate `id` primary key, `base TEXT`, `quote TEXT`, nullable `venue TEXT`,
  nullable `coin_id` referencing `tracked_coins(coin_id) ON DELETE SET NULL`,
  `kind TEXT`, a `status` over the domain `active`/`paused`/`error` (matching
  `tracked_coins.status`), `registered_at`, and the live-poller contract columns
  consumed by SPEC-SCHED-001's poller: `last_polled_at TIMESTAMPTZ NULL`,
  `live_poll_claimed_until TIMESTAMPTZ NULL`, and `live_poll_interval INTERVAL NULL`
  (nullable per-market cadence override; NULL means the poller uses the global
  `LIVE_QUOTE_POLL_INTERVAL_SECS`).
- **REQ-DB-003** (Ubiquitous): `tracked_markets` shall enforce uniqueness over
  `(base, quote, COALESCE(venue, ''))` so that an aggregator-level pair (NULL venue)
  and venue-specific pairs for the same `(base, quote)` can coexist without collision.
- **REQ-DB-004** (Ubiquitous): The schema shall not contain any equities-specific table
  or column — no exchange/MIC registry, no holiday/calendar table, no market-phase or
  trading-halt table, and no market-open/close-time columns.
- **REQ-DB-005** (Ubiquitous): `tracked_markets` shall declare a partial claim index on
  `(last_polled_at)` restricted to `WHERE status = 'active'`, serving the live-quote
  poller's due-and-not-in-flight claim query (SPEC-SCHED-001 REQ-SCHED-003).

### Time-series tables and partitioning

- **REQ-DB-010** (Ubiquitous): The schema shall provide a `live_quotes` table with
  primary key `(market_id, ts)`, a nullable `as_of TIMESTAMPTZ`, and `NUMERIC`
  price/bid/ask/size/volume columns.
- **REQ-DB-011** (Ubiquitous): The schema shall provide a `candles` table with primary
  key `(market_id, interval, ts)` and `NUMERIC` open/high/low/close columns, with the
  `volume` column nullable to accommodate sources that supply OHLC without volume.
- **REQ-DB-012** (Ubiquitous): The schema shall provide a `coin_market_snapshots`
  table with primary key `(coin_id, vs_currency, ts)` and `NUMERIC` columns for price,
  `market_cap`, `fully_diluted_valuation`, `circulating_supply`, and `total_supply`.
- **REQ-DB-013** (Ubiquitous): The schema shall provide a `derivatives_quotes` table
  with primary key `(market_id, ts)` and `NUMERIC` columns for `funding_rate`,
  `open_interest`, `open_interest_usd`, `mark_price`, `index_price`, and `basis`,
  capturing all per-tick derivative observables in a single row.
- **REQ-DB-014** (Ubiquitous): Each of `live_quotes`, `candles`,
  `coin_market_snapshots`, and `derivatives_quotes` shall be RANGE-partitioned by `ts`
  with one partition per calendar month on UTC boundaries.
- **REQ-DB-015** (Ubiquitous): Each partitioned table shall declare parent-level
  indexes — a `btree` on `(<key columns>, ts DESC)` for key-scoped reads and a `BRIN`
  on `(ts)` for large append-ordered time-range scans — inherited by all child
  partitions.
- **REQ-DB-016** (Ubiquitous): The initial migration set shall create monthly
  partitions covering at least the current calendar year through the next calendar
  year; the policy for creating future-month partitions is recorded as an operational
  open item (OR-DB-3), not hard-coded behavior.
- **REQ-DB-017** (State-Driven): While a write targets a `ts` outside any existing
  partition, the schema design shall require that the owning partition be ensured to
  exist before the write (app-side ensure-on-write or operational pre-creation); the
  schema shall not silently drop such writes.

### Revisioned metadata

- **REQ-DB-020** (Ubiquitous): The schema shall provide a `coin_metadata` table keyed
  `(coin_id, revision)` with `first_seen_at` and `last_seen_at` `TIMESTAMPTZ` columns
  and a 0-based integer `revision`.
- **REQ-DB-021** (Event-Driven): When metadata is re-collected and a tracked value has
  changed, the design shall require inserting a new revision (incremented `revision`,
  fresh `first_seen_at`); when no tracked value has changed, it shall require advancing
  `last_seen_at` on the existing revision without inserting a new row.
- **REQ-DB-022** (Ubiquitous): Continuously-changing coin aggregates (market cap, FDV,
  circulating/total supply, current price) shall be stored in the time-series
  `coin_market_snapshots` table and shall not be stored as `coin_metadata` revisions,
  so the revision table does not churn on every poll.
- **REQ-DB-023** (Ubiquitous): `coin_metadata` shall carry an as-of index
  `btree(coin_id, first_seen_at DESC)` supporting "greatest `first_seen_at <= as_of`"
  reads.

### Coordination tables

- **REQ-DB-030** (Ubiquitous): The schema shall provide a `collection_queue` table
  with `status`, `claimed_by`, `lease_expires_at`, `heartbeat_at`, `attempts`, and
  `last_error` columns supporting `FOR UPDATE SKIP LOCKED` claiming.
- **REQ-DB-031** (Ubiquitous): `collection_queue` shall enforce a partial unique
  index over `(target_kind, target_id, kind)` restricted to live statuses
  (`pending`/`claimed`/`running`) so at most one live work item exists per target+kind.
- **REQ-DB-032** (Ubiquitous): `collection_queue` shall declare a partial claim index
  on `(enqueued_at) WHERE status = 'pending'` to support fair oldest-first claiming.
- **REQ-DB-033** (Ubiquitous): The schema shall provide `backfill_jobs` (UNIQUE
  `(market_id, dataset)`) and `backfill_chunks` (with `range_start`, `range_end`, a
  durable `cursor`, `claimed_by`, `lease_expires_at`, `heartbeat_at`, `attempts`,
  `last_error`) supporting crash-resumable lease-based claiming.
- **REQ-DB-034** (Ubiquitous): The schema shall provide an `upstream_request_pacer`
  table keyed by `provider TEXT`, with `next_allowed_at`, `min_gap_ms`,
  `cooldown_until`, `credit_window_start`, `credits_used`, and `credit_limit` columns.
- **REQ-DB-035** (Ubiquitous): The initial migration shall seed one
  `upstream_request_pacer` row per known provider (`coingecko`, `binance`, `coinbase`,
  `kraken`) so consumers can atomically `UPDATE … RETURNING` a slot without a prior
  `INSERT`.
- **REQ-DB-036** (Ubiquitous): `collection_queue` shall additionally declare a partial
  claim index on `(lease_expires_at)` restricted to `WHERE status IN ('claimed','running')`,
  serving the lease-expired re-claim path (SPEC-SCHED-001 REQ-SCHED-014) that complements
  the pending-path index of REQ-DB-032.

### Types and integrity

- **REQ-DB-040** (Ubiquitous): Every monetary or quantity column (price, OHLC, volume,
  supply, market cap, FDV, funding rate, open interest, mark/index price, basis) shall
  be PostgreSQL `NUMERIC`; the schema shall not use `DOUBLE PRECISION` / `REAL` for any
  such value.
- **REQ-DB-041** (Ubiquitous): Every timestamp column shall be `TIMESTAMPTZ` (UTC).
- **REQ-DB-042** (Ubiquitous): All foreign keys from data/coordination tables to the
  registries shall declare an explicit `ON DELETE` action (`CASCADE` for owned data,
  `SET NULL` for the optional `tracked_markets.coin_id` link).
- **REQ-DB-043** (Ubiquitous): All migrations shall be idempotent at the object level
  (`CREATE TABLE IF NOT EXISTS`, `CREATE INDEX IF NOT EXISTS`) and runnable on startup,
  and shall be compatible with sqlx compile-time-checked queries.

## Exclusions (What NOT to Build)

- **No equities machinery.** No exchanges/MIC registry, no holiday/calendar table, no
  market-phase, trading-halt, market-open/close, or close-grace columns (research
  §1.1). This SPEC introduces no such structures under any name.
- **No `DOUBLE PRECISION`/`REAL`** for any monetary or quantity value — `NUMERIC` only
  (REQ-DB-040).
- **No tokenomics revision churn.** Supply/cap/FDV are time-series, not revisions
  (REQ-DB-022).
- **No query, worker, or handler code** — this SPEC defines tables, keys, indexes, and
  partitions only. Query/upsert logic is SPEC-SCHED-001 / SPEC-API-001 / SPEC-PROV-001.
- **No retention/partition-drop policy** baked into the schema — retention is a
  deployment decision (OR-DEPLOY-1).
- **No WebSocket/streaming tables** and no `pg_notify` fan-out schema (out of
  foundation scope).
- **No second derivatives table** — funding rate and open interest live together in
  `derivatives_quotes` (REQ-DB-013).

## @MX Annotation Targets (high fan_in)

- The `tracked_markets` live-poller contract columns (`last_polled_at`,
  `live_poll_claimed_until`, `live_poll_interval`) and the `(last_polled_at)
  WHERE status = 'active'` claim index — `@MX:ANCHOR`: this is the schema contract
  SPEC-SCHED-001's poller claim query depends on; renaming/retyping these columns or
  the index predicate breaks the poller (REQ-DB-002/005).
- The partitioned-table DDL (`live_quotes`, `candles`, `coin_market_snapshots`,
  `derivatives_quotes`) — `@MX:ANCHOR` on the partition + index contract (every read
  path depends on the `btree(key, ts DESC)` + `BRIN(ts)` shape).
- `upstream_request_pacer` seed + single-source contract — `@MX:NOTE` that all three
  workers consume it and none redefines it.
- `collection_queue` / `backfill_chunks` claim indexes — `@MX:NOTE` documenting the
  exact `FOR UPDATE SKIP LOCKED` query shape they serve.
- `coin_metadata` revision invariant — `@MX:WARN`/`@MX:REASON` that a new revision is
  inserted only on a real value change (`IS NOT DISTINCT FROM`), else `last_seen_at`
  advances.

## Open Items (do not guess)

- **OR-DB-1:** keep the metadata/market split (recommended, research §4.3) vs a single
  revisioned tokenomics table. Schema fixes the split; reversal is a run decision.
- **OR-DB-2:** candle `volume` provenance — NULL from CoinGecko, enrich from
  `/market_chart`, or require an exchange provider. Schema supports all three; policy
  is a run decision.
- **OR-DB-3:** future-month partition creation — app-side ensure-on-write vs
  operational cron. Mechanism is normative (REQ-DB-017); the automation choice is ops.
- **OR-DB-4:** exact `NUMERIC(precision, scale)` bounds per column vs unbounded
  `NUMERIC`. Recommend unbounded `NUMERIC` (research §3.4); confirm at run.
