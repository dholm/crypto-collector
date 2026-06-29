---
id: SPEC-API-002
type: plan
updated: 2026-06-29
---

# SPEC-API-002 — Implementation Plan

Brownfield change to `src/api/` + `src/models/` + `migrations/` + `api/crypto-collector.yaml`.
Methodology per `quality.yaml` (brownfield: characterization first where existing handlers are
re-based). Commit directly to `main` (no feature branches). Quality gate after each phase:
`cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test`.

## Milestones (priority-ordered, no time estimates)

### Phase 1 — Database migrations (foundation; Priority High)

- `migrations/0010_coin_live_poll_interval.sql`
  - `ALTER TABLE tracked_coins ADD COLUMN live_poll_interval INTERVAL` (nullable; null = global).
  - No backfill; existing coins default to null (global cadence). (REQ-API-110)
- `migrations/0011_remove_markets.sql`
  - `DROP TABLE` for `derivatives_quotes`, market-scoped `candles`, market-scoped `quotes`,
    then `tracked_markets` — order respects FK dependencies; cascade accepted. (REQ-API-104, D2)
  - `CREATE TABLE coin_quotes (coin_id TEXT REFERENCES tracked_coins, ts TIMESTAMPTZ, price
    NUMERIC, vs_currency TEXT, source TEXT, PRIMARY KEY ...)` — PK per OR-API2-1. (REQ-API-122)
  - `CREATE TABLE coin_candles (coin_id TEXT REFERENCES tracked_coins, interval TEXT, ts
    TIMESTAMPTZ, open/high/low/close/volume NUMERIC, vs_currency TEXT, source TEXT, PRIMARY KEY
    ...)`. (REQ-API-132)
  - Migrations run at startup via `sqlx::migrate!()` — confirm both files validated by the
    `migration_files` integration test (`tests/`).
- Gate: `cargo test --test migration_files`.

### Phase 2 — Remove the market-keyed surface (Priority High)

- Delete `src/api/markets.rs` and `src/api/derivatives.rs`.
- Remove market route registrations from `src/api/mod.rs` `build_api_router`. (REQ-API-100/101)
- Remove market DTOs from `src/api/dto.rs` and `MarketListKey` from `src/api/cursor.rs`.
  (REQ-API-105)
- Remove `/markets*` paths and `Market`/`Derivative` schemas from `api/crypto-collector.yaml`,
  and drop their operationIds from the `openapi_yaml_contains_all_operation_ids` test in
  `src/api/mod.rs`. (REQ-API-106/107)
- Gate: `cargo check --all-targets` (catch dangling references), then the doc-parity test.
- Note: `src/api/markets.rs` currently has uncommitted modifications (git `M`) — the deletion
  supersedes them; confirm no other module imports the removed symbols before deleting.

### Phase 3 — Coin model + DTOs gain `live_poll_interval` (Priority High)

- `src/models/coin.rs` — add `live_poll_interval` to `TrackedCoin` (sqlx INTERVAL → PgInterval or
  chrono::Duration per OR-API2-6). (REQ-API-110)
- `src/api/dto.rs` — add the field to `CoinDto` (out: H/M/S string), `RegisterCoinRequest` (in:
  optional H/M/S string), `UpdateCoinRequest` (in: nullable H/M/S string; null = reset).
  (REQ-API-111/113/115)
- `src/api/coins.rs` — `register_coin` validates + persists; `update_coin` applies set/change/
  reset and resets `last_polled_at` + the in-flight marker on any change. Bounds check against
  `max(LIVE_POLL_MIN_INTERVAL_SECS, global tick)` / `LIVE_POLL_MAX_INTERVAL_SECS`; 422 on
  violation. (REQ-API-112/113/114)
- Shared H/M/S parse/format/validate helper — single source of truth, marked `@MX:ANCHOR`.
- Gate: `cargo test` (coin DTO serde + handler unit tests).

### Phase 4 — Re-base quotes.rs / candles.rs to coin-keyed reads (Priority High)

- `src/api/quotes.rs` — `GET /v1/coins/{coin_id}/quotes/latest` and `/quotes` (keyset-paginated,
  `vs_currency` default `usd`, time-range). Reads `coin_quotes`. 404 on unknown coin.
  (REQ-API-120/121/122)
- `src/api/candles.rs` — `GET /v1/coins/{coin_id}/candles` (`interval` required + validated, 400
  on absent/invalid; keyset-paginated; `vs_currency`; time-range). Reads `coin_candles`.
  (REQ-API-130/131/132)
- New coin keyset cursor key(s) in `src/api/cursor.rs` (e.g. `(ts, coin_id)` / `(ts, coin_id,
  interval)`), opaque base64url-no-pad; 400 on undecodable cursor. (REQ-API-150)
- Register the new routes in `src/api/mod.rs` `build_api_router`.
- Gate: `cargo test` (cursor round-trip + handler tests).

### Phase 5 — WebSocket streams (Priority High)

- `src/api/websocket.rs` — two handlers using axum 0.8 built-in
  `axum::extract::ws::WebSocketUpgrade`.
  - Per-connection in-memory subscription set; control-frame parse → subscribe/unsubscribe;
    malformed → `{"error":"invalid_message",...}`, connection stays open. (REQ-API-141/143/146/147)
  - Cross-replica delivery: a PostgreSQL LISTEN task (`sqlx` `PgListener`) bridges NOTIFY payloads
    into a broadcast that each connection filters by its subscriptions; use `tokio::select!` over
    the socket recv and the broadcast recv. (REQ-API-142/145, D5)
  - Quote push shape and candle push shape per spec Module 5.
- Register `/v1/coins/stream/quotes` and `/v1/coins/stream/candles` in `build_api_router`
  **before** the `/{coin_id}` wildcard routes so `stream` is not captured as a `coin_id`.
  (REQ-API-148)
- Producer side: the live-poller / candle persistence path emits `NOTIFY` after a successful
  upsert (SPEC-SCHED-001 territory) — this plan wires the NOTIFY emission on the new
  `coin_quotes` / `coin_candles` writes. Channel names + payload encoding per OR-API2-2.
- Gate: `cargo test` for the control-frame parser + subscription filter (pure-logic unit tests);
  end-to-end socket behavior covered by acceptance scenarios.

### Phase 6 — OpenAPI document (Priority Medium)

- `api/crypto-collector.yaml`:
  - Add `live_poll_interval` to the `Coin`, coin registration, and coin update schemas, mirroring
    ticker-collector's wording (optional on register, nullable on update = reset to global, H/M/S
    string in responses). (REQ-API-115)
  - Add coin quote paths (`/coins/{coin_id}/quotes`, `/quotes/latest`), candle path
    (`/coins/{coin_id}/candles`), and the two WebSocket paths (`/coins/stream/quotes`,
    `/coins/stream/candles`) with `101`/`400` responses, control/push message schemas, and
    operationIds (e.g. `streamCoinQuotes`, `streamCoinCandles`), mirroring ticker-collector's
    `/quotes/stream`. (REQ-API-140/144/153)
  - Add `CoinQuote` / `CoinCandle` schemas (DecimalString fields).
- Gate: the doc-parity test passes with the new operationIds present and the market ones gone.

### Phase 7 — Doc-parity test + full suite (Priority Medium)

- Update `openapi_yaml_contains_all_operation_ids` in `src/api/mod.rs`: remove market/derivative
  operationIds, add the new coin quote/candle/stream operationIds. (REQ-API-107/153)
- Gate: full `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo test`; DB integration tests opt-in via `DATABASE_URL=... cargo test -- --ignored`.

## Technical Approach Notes

- **WebSocket**: axum 0.8 ships `axum::extract::ws::WebSocketUpgrade`; no `tokio-tungstenite`
  dependency needed. The upgrade extractor must run before any JSON body extractor — the handler
  signature takes `WebSocketUpgrade` and returns its `.on_upgrade(...)` response.
- **LISTEN/NOTIFY**: use `sqlx::postgres::PgListener` on a dedicated connection; one listener task
  per replica fans NOTIFY payloads into a `tokio::sync::broadcast` channel consumed by all live
  sockets, each filtering by its in-memory subscription set. NOTIFY payloads are capped at 8 KB in
  PostgreSQL — quote/candle JSON is well under that.
- **`live_poll_interval` round-trip**: stored as PostgreSQL INTERVAL; in Rust either `PgInterval`
  or `chrono::Duration` (OR-API2-6). API I/O is the canonical H/M/S string (parse on the way in,
  format on the way out) — a single shared helper, the `@MX:ANCHOR`, owns this conversion and the
  bounds check.
- **Reset semantics**: any successful `live_poll_interval` write (set, change, or reset-to-null)
  must clear `last_polled_at` and the in-flight marker in the same transaction so the next live
  tick re-evaluates cadence (mirrors ticker-collector).
- **Cursor keys**: `coin_quotes` ordered by `(ts, coin_id)`; `coin_candles` by `(ts, coin_id,
  interval)` (within a fixed `interval`/`vs_currency` filter). Reuse the SPEC-API-001 base64url-
  no-pad encoding.
- **Route ordering**: register `/coins/stream/*` literal routes before `/coins/{coin_id}` and its
  nested routes in axum's router to avoid the wildcard shadowing the stream paths.

## Risk Analysis

- **Cascade data loss (intended)**: dropping `tracked_markets` cascades to its dependent
  `quotes`/`candles`/`derivatives_quotes` rows. This is accepted (D2); the migration must DROP in
  FK-safe order and the deploy runbook should note the irreversible data loss.
- **WebSocket vs JSON extractor**: the upgrade handler must not sit behind a body extractor or the
  handshake fails — verified by the `101` acceptance scenario.
- **Route shadowing**: `/coins/{coin_id}` could capture `stream` as a coin id; mitigated by
  registration order (REQ-API-148) and a dedicated acceptance check that `stream` paths upgrade.
- **NOTIFY payload cap**: 8 KB PostgreSQL limit — non-issue for small quote/candle JSON; documented
  so future enriched payloads stay within bounds.
- **Uncommitted `markets.rs` changes**: the file is `M` in git; deletion discards those edits.
  Confirm with the user before deleting if those edits were intended for another purpose (this SPEC
  assumes deletion supersedes them).
- **sqlx INTERVAL mapping**: PgInterval vs chrono::Duration affects the formatter and any compile-
  time query checks; resolved at run (OR-API2-6) before wiring the handler.

## Dependencies / Sequencing

- Phase 1 (migrations) precedes Phases 3–5 (handlers read the new tables/column).
- Phase 2 (removal) is independent of 3–5 but should land first to keep `cargo check` green and
  avoid dangling market references while the new code is added.
- Phase 6 (OpenAPI) and Phase 7 (doc-parity test) close the loop after handlers exist.
