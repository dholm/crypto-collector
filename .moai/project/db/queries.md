# Common Query Patterns — crypto-collector

_Manually maintained — not auto-updated by the `moai-domain-db-docs` hook._

Source of truth for query patterns: `src/db/upserts.rs` (writes) and `src/api/` (reads). All
queries use `sqlx` (compile-time-checked where practical) against the tables documented in
`schema.md`.

## Write patterns (upserts)

All ingestion paths use natural-key `ON CONFLICT ... DO UPDATE` upserts so that re-processing a
crashed work unit overwrites identical rows rather than duplicating them (`SPEC-SCHED-001`
REQ-SCHED-040). Defined in `src/db/upserts.rs`:

| Function | Table | Conflict target | Side effect |
|---|---|---|---|
| `upsert_coin_quote` | `coin_quotes` | `(coin_id, vs_currency, ts)` | `pg_notify('coin_quote_updated', …)` in the same transaction |
| `upsert_coin_candle` | `coin_candles` | `(coin_id, vs_currency, interval, ts)` | Calls `ensure_candle_partition` first (creates the covering monthly partition if missing); `pg_notify('coin_candle_updated', …)` in the same transaction |
| `upsert_coin_market_snapshot` | `coin_market_snapshots` | `(coin_id, vs_currency, ts)` | none |
| `upsert_coin_metadata` | `coin_metadata` | N/A — revision-pattern insert/update, not a plain upsert | inserts a new revision only if `metadata_has_changed()` returns true; otherwise advances `last_seen_at` on the existing revision |

`pg_notify` payloads are relayed to WebSocket subscribers by `src/listener.rs` (`AppState.coin_quote_tx`).

## Read patterns (keyset pagination)

All `/v1` list endpoints use **keyset (cursor) pagination**, not `OFFSET`, for O(1)-deep stability
over the append-heavy partitioned tables. Implemented in `src/api/cursor.rs`
(`encode_keyset_cursor` / `decode_keyset_cursor`, `@MX:ANCHOR` — every list endpoint depends on
this contract).

| Endpoint | Key type | Ordering |
|---|---|---|
| `GET /v1/coins` | `CoinListKey { coin_id }` | `coin_id ASC` |
| `GET /v1/coins/{coin_id}/quotes` | `TsKey { ts }` | `ts DESC` |
| `GET /v1/coins/{coin_id}/candles` | `TsKey { ts }` | `ts DESC` |
| `GET /v1/coins/{coin_id}/market` | `TsKey { ts }` | `ts DESC` |
| `GET /v1/coins/{coin_id}/cycle-projection/{model}` | `CycleOverlayKey { cycle_number, days_since_halving }` | `(cycle_number ASC, days_since_halving ASC)` |

The resource-specific filter (`coin_id`, `vs_currency`, `interval`, optional `cycle_number`) is
kept in the `WHERE` clause, not embedded in the cursor, so cursors stay compact and stable.

## Worker claim queries

- **`collection_queue`** — two claim shapes, both `SELECT ... FOR UPDATE SKIP LOCKED LIMIT 1`:
  - Pending: `WHERE status = 'pending' ORDER BY enqueued_at` (uses `collection_queue_claim_pending_idx`)
  - Lease-expired reclaim: `WHERE status IN ('claimed','running') AND lease_expires_at < now() ORDER BY lease_expires_at` (uses `collection_queue_claim_lease_expired_idx`)
- **`backfill_chunks`** — same `FOR UPDATE SKIP LOCKED` shape, oldest-first pending claim via `backfill_chunks_claim_idx`.
- **Live poller** (`tracked_coins` / historically `tracked_markets`) — claims rows where
  `status = 'active' AND (last_polled_at IS NULL OR last_polled_at + live_poll_interval <= now())
  AND (live_poll_claimed_until IS NULL OR live_poll_claimed_until < now())`, using the partial
  index `tracked_coins_live_poll_claim_idx` (`last_polled_at`) WHERE `status = 'active'`.

## Cycle-overlay aggregation

`src/collectors/cycle_overlay.rs` derives `cycle_overlay_points` entirely from
`coin_candles` (`interval = '1d'`) for the configured coin/currency — no upstream provider call.
A recompute deletes all existing rows for `(coin_id, vs_currency)` and re-inserts (no `UPDATE`
path). Daily series aggregation across intervals is done in SQL (not fetched fully into memory) to
avoid OOM on large per-coin candle histories — see `coin-candles-full-fetch-ooms` memory.
