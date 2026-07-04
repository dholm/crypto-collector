# Database Schema — crypto-collector

Engine: **PostgreSQL** · Access layer: **sqlx 0.9** (compile-time-checked queries + hand-written
upsert helpers in `src/db/upserts.rs`, no ORM) · Migrations: `sqlx migrate`, embedded via
`sqlx::migrate!()` and applied at process startup (see `sqlx-migrate-embed-rebuild` memory: a
migrations-only change requires rebuilding the binary).

This document reflects the schema **after all 15 migrations have been applied**, not the
intermediate state at any single migration. See `migrations.md` for the ordered history,
including the tables that were created and later dropped (`tracked_markets`, `live_quotes`,
market-keyed `candles`, `derivatives_quotes`, market-keyed `backfill_jobs`/`backfill_chunks`).

**Final table count: 10** (excluding monthly partition children).

---

## 1. Registries

### `tracked_coins`

Coin-keyed registry of assets to collect. Runtime-populated (see `seed-data.md`) — no rows are
inserted by any migration.

| Column | Type | Nullable | Default | Notes |
|---|---|---|---|---|
| `coin_id` | TEXT | NOT NULL | — | e.g. CoinGecko id `"bitcoin"` |
| `symbol` | TEXT | NOT NULL | — | |
| `name` | TEXT | NOT NULL | — | |
| `status` | TEXT | NOT NULL | `'active'` | CHECK IN (`active`, `paused`, `error`) |
| `registered_at` | TIMESTAMPTZ | NOT NULL | `now()` | |
| `last_collected_at` | TIMESTAMPTZ | NULL | — | |
| `error` | TEXT | NULL | — | |
| `live_poll_interval` | INTERVAL | NULL | — | per-coin cadence override; NULL = global default (0010) |
| `last_polled_at` | TIMESTAMPTZ | NULL | — | advanced only on poll success (0010) |
| `live_poll_claimed_until` | TIMESTAMPTZ | NULL | — | self-expiring in-flight claim marker (0010) |

- **PK**: `(coin_id)`
- **FK**: none
- **Indexes**: `tracked_coins_live_poll_claim_idx` — btree(`last_polled_at`) WHERE `status = 'active'` (0010)
- **Last migration touching this table**: `0010_coin_live_poll_interval.sql`

> `@MX:ANCHOR` in source: the live-poller claim contract columns (`last_polled_at`,
> `live_poll_claimed_until`, `live_poll_interval`) plus the partial index are invariant —
> renaming/retyping breaks `SPEC-SCHED-001` REQ-SCHED-003.

---

## 2. Coin-keyed time-series (spot quotes & candles)

> **Historical note**: migrations 0002/0003/0005 originally created **market-keyed**
> `live_quotes`, `candles`, and `derivatives_quotes` tables (FK to a `tracked_markets` table).
> Migration `0011_remove_markets.sql` dropped all of them and `tracked_markets` itself, replacing
> the price/candle tables with **coin-keyed** equivalents below. `derivatives_quotes` was **not**
> recreated — the final schema has no derivatives-quotes table.

### `coin_quotes`

Coin-keyed spot price time-series, one row per capture instant.

| Column | Type | Nullable | Default | Notes |
|---|---|---|---|---|
| `coin_id` | TEXT | NOT NULL | — | FK → `tracked_coins(coin_id)` ON DELETE CASCADE |
| `vs_currency` | TEXT | NOT NULL | — | quote currency, e.g. `usd` |
| `ts` | TIMESTAMPTZ | NOT NULL | — | partition key |
| `price` | NUMERIC | NOT NULL | — | never float (REQ-DB-040) |
| `source` | TEXT | NOT NULL | — | provider name |

- **PK**: `(coin_id, vs_currency, ts)`
- **FK**: `coin_id` → `tracked_coins.coin_id` ON DELETE CASCADE
- **Partitioning**: `PARTITION BY RANGE (ts)`, one partition per calendar month, UTC boundaries.
  Static partitions `coin_quotes_2024_01` … `coin_quotes_2027_12` created in the migration.
- **Indexes** (parent-level, inherited by all partitions):
  - `coin_quotes_coin_id_vs_currency_ts_idx` — btree(`coin_id, vs_currency, ts DESC`)
  - `coin_quotes_ts_brin` — BRIN(`ts`)
- **Upsert**: `ON CONFLICT (coin_id, vs_currency, ts) DO UPDATE` (`src/db/upserts.rs::upsert_coin_quote`); also emits `pg_notify('coin_quote_updated', …)` in the same transaction.
- **Last migration touching this table**: `0011_remove_markets.sql`

### `coin_candles`

Coin-keyed OHLCV time-series.

| Column | Type | Nullable | Default | Notes |
|---|---|---|---|---|
| `coin_id` | TEXT | NOT NULL | — | FK → `tracked_coins(coin_id)` ON DELETE CASCADE |
| `vs_currency` | TEXT | NOT NULL | — | |
| `interval` | TEXT | NOT NULL | — | e.g. `1h`, `1d` — part of PK so intervals coexist |
| `ts` | TIMESTAMPTZ | NOT NULL | — | partition key |
| `open` | NUMERIC | NOT NULL | — | |
| `high` | NUMERIC | NOT NULL | — | |
| `low` | NUMERIC | NOT NULL | — | |
| `close` | NUMERIC | NOT NULL | — | |
| `volume` | NUMERIC | NULL | — | nullable: CoinGecko `/ohlc` returns no volume |
| `source` | TEXT | NOT NULL | — | |

- **PK**: `(coin_id, vs_currency, interval, ts)`
- **FK**: `coin_id` → `tracked_coins.coin_id` ON DELETE CASCADE
- **Partitioning**: `PARTITION BY RANGE (ts)`, monthly, UTC. Static partitions `coin_candles_2024_01` … `coin_candles_2027_12`.
  **Runtime partition creation**: `src/db/partitions.rs::ensure_candle_partition` creates
  `coin_candles_YYYY_MM` on demand (guarded by `pg_advisory_xact_lock`) for historical backfill
  reaching before 2024-01 or beyond the static range, called before every candle insert.
- **Indexes**:
  - `coin_candles_coin_id_vs_currency_interval_ts_idx` — btree(`coin_id, vs_currency, interval, ts DESC`)
  - `coin_candles_ts_brin` — BRIN(`ts`)
- **Upsert**: `ON CONFLICT (coin_id, vs_currency, interval, ts) DO UPDATE` (`upsert_coin_candle`); emits `pg_notify('coin_candle_updated', …)`.
- **Last migration touching this table**: `0011_remove_markets.sql` (runtime partitions added by application code, not migrations)

---

## 3. Coin market snapshots

### `coin_market_snapshots`

Continuously-changing coin market aggregates (price, market cap, FDV, supply, volume), stored as
time-series rows rather than as `coin_metadata` revisions to avoid revision churn.

| Column | Type | Nullable | Default | Notes |
|---|---|---|---|---|
| `coin_id` | TEXT | NOT NULL | — | FK → `tracked_coins(coin_id)` ON DELETE CASCADE |
| `vs_currency` | TEXT | NOT NULL | — | |
| `ts` | TIMESTAMPTZ | NOT NULL | — | partition key |
| `price` | NUMERIC | NOT NULL | — | |
| `market_cap` | NUMERIC | NULL | — | |
| `fully_diluted_valuation` | NUMERIC | NULL | — | |
| `circulating_supply` | NUMERIC | NULL | — | |
| `total_supply` | NUMERIC | NULL | — | |
| `volume_24h` | NUMERIC | NULL | — | |
| `source` | TEXT | NOT NULL | — | |

- **PK**: `(coin_id, vs_currency, ts)`
- **FK**: `coin_id` → `tracked_coins.coin_id` ON DELETE CASCADE
- **Partitioning**: `PARTITION BY RANGE (ts)`, monthly, UTC. Static partitions `coin_market_snapshots_2024_01` … `_2027_12`.
- **Indexes**:
  - `coin_market_snapshots_coin_id_vs_currency_ts_idx` — btree(`coin_id, vs_currency, ts DESC`)
  - `coin_market_snapshots_ts_brin` — BRIN(`ts`)
- **Upsert**: `ON CONFLICT (coin_id, vs_currency, ts) DO UPDATE` (`upsert_coin_market_snapshot`).
- **Last migration touching this table**: `0004_coin_market_snapshots.sql` (untouched by 0011 — it was already coin-keyed)

---

## 4. Coin metadata (revisioned)

### `coin_metadata`

Slowly-changing descriptive metadata, using a revision pattern: a new row is inserted only when a
tracked field actually changes; otherwise `last_seen_at` advances in place.

| Column | Type | Nullable | Default | Notes |
|---|---|---|---|---|
| `coin_id` | TEXT | NOT NULL | — | FK → `tracked_coins(coin_id)` ON DELETE CASCADE |
| `revision` | INTEGER | NOT NULL | `0` | 0-based counter |
| `name` | TEXT | NOT NULL | — | |
| `symbol` | TEXT | NOT NULL | — | |
| `categories` | TEXT[] | NULL | — | CoinGecko taxonomy |
| `description` | TEXT | NULL | — | |
| `homepage` | TEXT | NULL | — | |
| `links` | JSONB | NULL | — | structured external links |
| `contract_addresses` | JSONB | NULL | — | on-chain addresses |
| `max_supply` | NUMERIC | NULL | — | NULL for uncapped assets (ETH, DOGE, XMR) |
| `genesis_date` | DATE | NULL | — | |
| `first_seen_at` | TIMESTAMPTZ | NOT NULL | `now()` | |
| `last_seen_at` | TIMESTAMPTZ | NOT NULL | `now()` | advanced on unchanged re-poll |

- **PK**: `(coin_id, revision)`
- **FK**: `coin_id` → `tracked_coins.coin_id` ON DELETE CASCADE
- **Indexes**: `coin_metadata_coin_id_first_seen_at_idx` — btree(`coin_id, first_seen_at DESC`) — supports point-in-time "as of" lookups.
- **Change detection**: `src/db/upserts.rs::metadata_has_changed` compares name, symbol,
  categories, description, homepage, links, contract_addresses, max_supply, genesis_date.
- **Last migration touching this table**: `0006_coin_metadata.sql`

---

## 5. Collection queue

### `collection_queue`

Durable work queue for coin/market data collection tasks. Workers claim rows atomically via
`SELECT ... FOR UPDATE SKIP LOCKED`.

| Column | Type | Nullable | Default | Notes |
|---|---|---|---|---|
| `id` | BIGSERIAL | NOT NULL | — | |
| `target_kind` | TEXT | NOT NULL | — | CHECK IN (`coin`, `market`) |
| `target_id` | TEXT | NOT NULL | — | coin_id text, or market_id-as-text (legacy) |
| `kind` | TEXT | NOT NULL | — | CHECK IN (`spot`, `candles`, `metadata`, `market`, `derivatives`, `cycle_overlay`) — widened by 0014 |
| `status` | TEXT | NOT NULL | `'pending'` | CHECK IN (`pending`, `claimed`, `running`, `done`, `failed`) |
| `claimed_by` | TEXT | NULL | — | |
| `lease_expires_at` | TIMESTAMPTZ | NULL | — | |
| `heartbeat_at` | TIMESTAMPTZ | NULL | — | |
| `attempts` | INTEGER | NOT NULL | `0` | |
| `last_error` | TEXT | NULL | — | |
| `enqueued_at` | TIMESTAMPTZ | NOT NULL | `now()` | |
| `updated_at` | TIMESTAMPTZ | NOT NULL | `now()` | |

- **PK**: `(id)`
- **FK**: none (target is polymorphic via `target_kind`/`target_id`)
- **Constraints**:
  - `collection_queue_kind_check` — CHECK on `kind`, redefined by `0014` to add `'cycle_overlay'`
  - `target_kind` CHECK, `status` CHECK (both from 0007, unchanged)
- **Indexes**:
  - `collection_queue_dedup_idx` — UNIQUE(`target_kind, target_id, kind`) WHERE `status IN ('pending','claimed','running')` — at most one live item per target+kind
  - `collection_queue_claim_pending_idx` — btree(`enqueued_at`) WHERE `status = 'pending'`
  - `collection_queue_claim_lease_expired_idx` — btree(`lease_expires_at`) WHERE `status IN ('claimed','running')`
- **Last migration touching this table**: `0014_collection_queue_cycle_overlay_kind.sql`

---

## 6. Backfill coordination

> **Historical note**: `0008_backfill.sql` originally created **market-keyed** `backfill_jobs`
> (FK → `tracked_markets`) and `backfill_chunks`. Both were dropped in `0011` along with
> `tracked_markets`, and recreated **coin-keyed** by `0012_coin_backfill.sql` below.

### `backfill_jobs`

One job per `(coin_id, dataset)`; fans out into `backfill_chunks`.

| Column | Type | Nullable | Default | Notes |
|---|---|---|---|---|
| `id` | BIGSERIAL | NOT NULL | — | |
| `coin_id` | TEXT | NOT NULL | — | FK → `tracked_coins(coin_id)` ON DELETE CASCADE |
| `dataset` | TEXT | NOT NULL | — | e.g. `candles`, `spot` |
| `status` | TEXT | NOT NULL | `'pending'` | no CHECK constraint defined |
| `requested_at` | TIMESTAMPTZ | NOT NULL | `now()` | |
| `updated_at` | TIMESTAMPTZ | NOT NULL | `now()` | |

- **PK**: `(id)`
- **FK**: `coin_id` → `tracked_coins.coin_id` ON DELETE CASCADE
- **Unique**: `(coin_id, dataset)` — makes enqueue idempotent
- **Last migration touching this table**: `0012_coin_backfill.sql`

### `backfill_chunks`

Claimable work unit; crash-resumable via lease + heartbeat + cursor.

| Column | Type | Nullable | Default | Notes |
|---|---|---|---|---|
| `id` | BIGSERIAL | NOT NULL | — | |
| `job_id` | BIGINT | NOT NULL | — | FK → `backfill_jobs(id)` ON DELETE CASCADE |
| `coin_id` | TEXT | NOT NULL | — | denormalized copy, not a declared FK |
| `dataset` | TEXT | NOT NULL | — | |
| `interval` | TEXT | NULL | — | candle granularity; NULL for non-candle datasets |
| `range_start` | TIMESTAMPTZ | NULL | — | NULL bounds = whole-dataset single-fetch chunk |
| `range_end` | TIMESTAMPTZ | NULL | — | |
| `cursor` | TIMESTAMPTZ | NULL | — | durable resume marker |
| `status` | TEXT | NOT NULL | `'pending'` | no CHECK constraint defined |
| `claimed_by` | TEXT | NULL | — | |
| `lease_expires_at` | TIMESTAMPTZ | NULL | — | |
| `heartbeat_at` | TIMESTAMPTZ | NULL | — | |
| `attempts` | INTEGER | NOT NULL | `0` | |
| `last_error` | TEXT | NULL | — | |
| `created_at` | TIMESTAMPTZ | NOT NULL | `now()` | |
| `updated_at` | TIMESTAMPTZ | NOT NULL | `now()` | |

- **PK**: `(id)`
- **FK**: `job_id` → `backfill_jobs.id` ON DELETE CASCADE
- **Indexes**: `backfill_chunks_claim_idx` — btree(`created_at`) WHERE `status = 'pending'`
- **Last migration touching this table**: `0012_coin_backfill.sql`

---

## 7. Upstream rate pacer

### `upstream_request_pacer`

Per-provider, credit-aware outbound rate pacer. One row per provider; seeded on creation for all
four known providers so consumers can `UPDATE ... RETURNING` without a prior `INSERT`. Shared
across `live_poller`, `collection_queue` worker, and `backfill` worker.

| Column | Type | Nullable | Default | Notes |
|---|---|---|---|---|
| `provider` | TEXT | NOT NULL | — | e.g. `coingecko`, `binance`, `coinbase`, `kraken` |
| `next_allowed_at` | TIMESTAMPTZ | NOT NULL | `now()` | |
| `min_gap_ms` | INTEGER | NOT NULL | `1000` | per-provider default: coingecko 2000, binance 100, coinbase/kraken 500 |
| `cooldown_until` | TIMESTAMPTZ | NULL | — | set on HTTP 429 |
| `credit_window_start` | TIMESTAMPTZ | NOT NULL | `date_trunc('month', now())` | |
| `credits_used` | BIGINT | NOT NULL | `0` | |
| `credit_limit` | BIGINT | NULL | — | NULL = unlimited (only coingecko has a cap: 10000/month) |
| `updated_at` | TIMESTAMPTZ | NOT NULL | `now()` | |

- **PK**: `(provider)`
- **FK**: none
- **Seed data**: see `seed-data.md` — 4 rows inserted by `0009_upstream_pacer.sql`
- **Last migration touching this table**: `0009_upstream_pacer.sql`

---

## 8. Bitcoin halving-cycle overlay (derived analytics)

### `cycle_overlay_points`

Materialised, idempotently-rebuilt derived-analytics table. Every row is a pure function of the
persisted daily (`1d`) `coin_candles` history for the configured target coin/currency — nothing
is fetched from an upstream provider directly for this table. A recompute `DELETE`s all rows for
`(coin_id, vs_currency)` and re-`INSERT`s; there is no `UPDATE` path.

| Column | Type | Nullable | Default | Notes |
|---|---|---|---|---|
| `coin_id` | TEXT | NOT NULL | — | no declared FK |
| `vs_currency` | TEXT | NOT NULL | — | |
| `cycle_number` | INTEGER | NOT NULL | — | |
| `halving_date` | DATE | NOT NULL | — | |
| `days_since_halving` | INTEGER | NOT NULL | — | |
| `ts` | DATE | NOT NULL | — | |
| `price` | NUMERIC | NOT NULL | — | |
| `norm_halving` | NUMERIC | NOT NULL | — | |
| `norm_cycle_low` | NUMERIC | NOT NULL | — | |
| `halving_baseline_approximate` | BOOLEAN | NOT NULL | `FALSE` | |
| `updated_at` | TIMESTAMPTZ | NOT NULL | `now()` | |
| `projected` | BOOLEAN | NOT NULL | `FALSE` | added by 0015; `true` for points repeating the last completed cycle's shape onto the current cycle out to the next halving |

- **PK**: `(coin_id, vs_currency, cycle_number, days_since_halving)`
- **FK**: none (not linked to `tracked_coins`; `coin_id` is a plain text tag)
- **Indexes**: `cycle_overlay_points_read_idx` — btree(`coin_id, vs_currency, cycle_number, days_since_halving`) — mirrors PK order for the keyset read route
- **Last migration touching this table**: `0015_cycle_overlay_projected.sql`

---

## Partitioning summary

Four tables use PostgreSQL declarative `RANGE(ts)` partitioning, one partition per UTC calendar
month:

| Table | Static partition range (from migration) | Runtime partition creation? |
|---|---|---|
| `coin_quotes` | 2024-01 .. 2027-12 | No |
| `coin_candles` | 2024-01 .. 2027-12 | **Yes** — `src/db/partitions.rs::ensure_candle_partition`, called before every candle insert, guarded by `pg_advisory_xact_lock` |
| `coin_market_snapshots` | 2024-01 .. 2027-12 | No |
| `cycle_overlay_points` | not partitioned (single table) | N/A |

Historical (now-dropped) partitioned tables: `live_quotes`, market-keyed `candles`,
`derivatives_quotes` — all removed by `0011_remove_markets.sql`.

## Tables that existed but do not exist in the final schema

| Table | Created by | Dropped by | Replacement |
|---|---|---|---|
| `tracked_markets` | 0001 | 0011 (CASCADE) | none — coin-keyed model has no market registry |
| `live_quotes` | 0002 | 0011 | `coin_quotes` |
| `candles` (market-keyed) | 0003 | 0011 | `coin_candles` |
| `derivatives_quotes` | 0005 | 0011 | none — not recreated |
| `backfill_jobs` (market-keyed) | 0008 | 0011 | `backfill_jobs` (coin-keyed, 0012) |
| `backfill_chunks` (market-keyed) | 0008 | 0011 | `backfill_chunks` (coin-keyed, 0012) |
