# Migration History — crypto-collector

Tool: `sqlx migrate` (sequential `migrations/NNNN_name.sql` files, embedded at compile time via
`sqlx::migrate!()`, applied automatically at process startup). All 15 migrations below are
**applied** in the current codebase.

Cumulative note: several later migrations `ALTER`/`DROP` tables created by earlier migrations
(most significantly `0011`, which removes the entire market-keyed data model). See `schema.md`
for the final, cumulative table state — this file documents intent per migration, in order.

| Version | Filename | Description | Destructive? |
|---|---|---|---|
| 0001 | `0001_registries.sql` | Creates `tracked_coins` (coin-keyed registry) and `tracked_markets` (pair-keyed registry, surrogate PK, unique `(base, quote, COALESCE(venue,''))`), plus the live-poller partial claim index on `tracked_markets`. | No |
| 0002 | `0002_live_quotes.sql` | Creates `live_quotes`, a market-keyed spot-price time-series partitioned `RANGE(ts)` monthly, with static partitions 2024-01..2027-12, btree + BRIN indexes. | No (table later dropped in 0011) |
| 0003 | `0003_candles.sql` | Creates market-keyed `candles` (OHLCV, PK includes `interval`), partitioned monthly 2024-01..2027-12, btree + BRIN indexes. | No (table later dropped in 0011) |
| 0004 | `0004_coin_market_snapshots.sql` | Creates `coin_market_snapshots`, a coin-keyed (not market-keyed) time-series of continuously-changing market aggregates (price, market cap, FDV, supply, volume), partitioned monthly 2024-01..2027-12. | No |
| 0005 | `0005_derivatives_quotes.sql` | Creates market-keyed `derivatives_quotes` consolidating funding rate, open interest, mark/index price, basis into one row per tick, partitioned monthly 2024-01..2027-12. | No (table later dropped in 0011, not recreated) |
| 0006 | `0006_coin_metadata.sql` | Creates `coin_metadata`, a revisioned table for slowly-changing descriptive coin metadata (PK `(coin_id, revision)`), with an as-of index on `(coin_id, first_seen_at DESC)`. | No |
| 0007 | `0007_collection_queue.sql` | Creates `collection_queue`, a durable work queue with a lease/heartbeat claim pattern, a dedup partial unique index, and two claim indexes (pending-path, lease-expired). | No |
| 0008 | `0008_backfill.sql` | Creates market-keyed `backfill_jobs` (`UNIQUE(market_id, dataset)`) and `backfill_chunks` (crash-resumable via lease + heartbeat + cursor), with a pending-claim index. | No (both tables later dropped in 0011) |
| 0009 | `0009_upstream_pacer.sql` | Creates `upstream_request_pacer`, one row per upstream provider for credit-aware rate pacing. **Seeds 4 rows** (`coingecko`, `binance`, `coinbase`, `kraken`) via `INSERT ... ON CONFLICT DO NOTHING`. | No |
| 0010 | `0010_coin_live_poll_interval.sql` | `ALTER TABLE tracked_coins ADD COLUMN` for `live_poll_interval`, `last_polled_at`, `live_poll_claimed_until` (mirrors the live-poller contract columns already on `tracked_markets`); adds a matching partial claim index. | No (additive) |
| 0011 | `0011_remove_markets.sql` | **Major restructure.** Drops `derivatives_quotes`, `candles`, `live_quotes`, `backfill_chunks`, `backfill_jobs`, and `tracked_markets CASCADE`. Creates coin-keyed `coin_quotes` and `coin_candles` (both `RANGE(ts)` partitioned monthly, 2024-01..2027-12, with btree + BRIN indexes). | **Yes — drops 6 tables** (`derivatives_quotes`, `candles`, `live_quotes`, `backfill_chunks`, `backfill_jobs`, `tracked_markets`); `derivatives_quotes` is not recreated in any later migration. |
| 0012 | `0012_coin_backfill.sql` | Recreates `backfill_jobs` and `backfill_chunks`, now coin-keyed (FK to `tracked_coins` instead of the removed `tracked_markets`), with `UNIQUE(coin_id, dataset)` and a pending-claim index. | No (recreation after 0011's drop) |
| 0013 | `0013_cycle_overlay.sql` | Creates `cycle_overlay_points`, a materialised, idempotently-rebuilt derived-analytics table for the Bitcoin halving-cycle overlay, with a read-route pagination index. | No |
| 0014 | `0014_collection_queue_cycle_overlay_kind.sql` | Drops and recreates the `collection_queue_kind_check` CHECK constraint on `collection_queue.kind` to widen the enum with `'cycle_overlay'` (fixes a runtime constraint-violation bug that silently prevented the overlay recompute from being enqueued). | Constraint replace (not data-destructive) |
| 0015 | `0015_cycle_overlay_projected.sql` | `ALTER TABLE cycle_overlay_points ADD COLUMN projected BOOLEAN NOT NULL DEFAULT FALSE` to distinguish real vs. projected (next-cycle-repeat) overlay points. | No (additive) |

## Destructive operations summary

| Migration | Destructive action |
|---|---|
| `0011_remove_markets.sql` | Drops 6 tables outright: `derivatives_quotes`, `candles`, `live_quotes`, `backfill_chunks`, `backfill_jobs`, `tracked_markets` (CASCADE). Of these, only `candles` and `backfill_jobs`/`backfill_chunks` have coin-keyed replacements (`coin_candles` in the same migration; `backfill_jobs`/`backfill_chunks` in 0012). `live_quotes` is replaced by `coin_quotes`. `derivatives_quotes` and `tracked_markets` have **no replacement** in the final schema. |
| `0014_collection_queue_cycle_overlay_kind.sql` | Drops and replaces a CHECK constraint (no data loss; existing rows already satisfy the widened check). |

## Ambiguous / notable migrations

- **`0011_remove_markets.sql`** — the single largest-impact migration; not ambiguous in intent
  (well-documented in its header comment) but worth flagging because it silently removes
  derivatives-quotes support entirely — there is no coin-keyed derivatives table in the current
  schema, and no SPEC or code references a replacement.
- **`0014`** — fixes a previously-shipped bug (an unnamed CHECK constraint auto-named
  `collection_queue_kind_check` by Postgres never had `'cycle_overlay'` added when that queue kind
  was introduced by SPEC-CYCLE-001). Documented directly in the migration's header comment.
