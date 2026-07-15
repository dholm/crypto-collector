# Seed Data Strategy — crypto-collector

_Manually maintained — not auto-updated by the `moai-domain-db-docs` hook._

## What migrations seed

Only **one** table is seeded by migrations: `upstream_request_pacer`. `0009_upstream_pacer.sql`
inserts 4 rows (one per original upstream provider) and `0018_bitstamp_pacer_seed.sql` adds a
5th (`bitstamp`), so consumers can `UPDATE ... RETURNING` without needing a prior `INSERT`:

| `provider` | `min_gap_ms` | `credit_limit` | Seeded by |
|---|---|---|---|
| `coingecko` | 2000 | 10000 (monthly cap, Demo tier) | 0009 |
| `binance` | 100 | NULL (unlimited) | 0009 |
| `coinbase` | 500 | NULL (unlimited) | 0009 |
| `kraken` | 500 | NULL (unlimited) | 0009 |
| `bitstamp` | 500 | NULL (unlimited) | 0018 |

Both use `INSERT ... ON CONFLICT (provider) DO NOTHING`, making them idempotent across repeated
migration runs.

No other migration contains seed data. (`grep -rl "INSERT INTO" migrations/` also matches
`0020_coin_candles_departition.sql`, but that `INSERT ... SELECT` is a row-copy during the
de-partition, not seed data.)

## What is runtime-populated (not seeded)

- **`tracked_coins`** — the coin registry is **not** seeded by any migration. Rows are inserted at
  runtime via the REST API: `POST` handlers in `src/api/coins.rs`
  (`INSERT INTO tracked_coins (coin_id, symbol, name, status, registered_at, live_poll_interval)`)
  and `src/api/metadata.rs`. Test fixtures (e.g. `src/collectors/backfill.rs` tests) also insert
  rows directly for isolated test setup, noting that `bitcoin` "must exist in `tracked_coins`
  (seeded by migrations or prior test data)" in comments — but this refers to test-time setup,
  not a migration-level seed.
- **`coin_quotes`, `coin_candles`, `coin_market_snapshots`, `coin_metadata`** — populated entirely
  by the collectors (`live_poller`, `collection_queue` worker, `backfill` worker) via the upsert
  helpers in `src/db/upserts.rs`. No seed data.
- **`collection_queue`, `backfill_jobs`, `backfill_chunks`** — populated at runtime as work items
  are enqueued; empty on a fresh database.
- **`cycle_overlay_points`** — populated by a periodic recompute (`src/collectors/cycle_overlay.rs`)
  derived from `coin_candles`; empty until the first recompute runs and sufficient `1d` candle
  history exists (see `src/api/cycle_overlay.rs` comment: "insufficient seeded data" refers to
  candle history, not a migration seed).

## Dev vs. prod

There is no separate dev/prod seed fixture set in this repository — the same migrations apply in
both environments (`sqlx::migrate!()` runs at startup regardless of environment). Coin tracking
and all time-series data are populated identically by the application in both environments; the
only difference is which coins operators choose to register via the API.
