---
id: SPEC-API-002
version: 0.1.0
status: completed
created: 2026-06-29
updated: 2026-06-29
author: dholm
priority: high
issue_number: 0
---

# SPEC-API-002 — Markets Removal, Coin Live-Poll Interval, and Coin-Level Quote/Candle/WebSocket APIs

Brownfield evolution of the REST surface defined in
[SPEC-API-001](../SPEC-API-001/spec.md). This SPEC **removes the entire market-keyed
surface** (markets CRUD/search, market-scoped quotes/candles/derivatives) and **re-bases
spot quotes and OHLCV candles onto the coin** (coin-keyed reads), adds a **per-coin live
poll interval** mirroring ticker-collector's per-ticker cadence, and introduces
**WebSocket streams** for live quotes and candles. Binance becomes the preferred upstream
for spot quotes and OHLCV (CoinGecko carries a monthly credit cap and is retained only for
coin search and metadata).

Pattern source: ticker-collector (`../ticker-collector/api/ticker-collector.yaml`) —
`live_poll_interval` per-resource cadence and the `/quotes/stream` WebSocket contract.
Schema/data contract base: [SPEC-DB-001](../SPEC-DB-001/spec.md). Collection semantics and
provider chain / pacing: [SPEC-PROV-001](../SPEC-PROV-001/spec.md),
[SPEC-SCHED-001](../SPEC-SCHED-001/spec.md). Cross-replica delivery and observability:
[SPEC-OBS-001](../SPEC-OBS-001/spec.md).

## HISTORY

- 2026-06-29 (v0.1.0): Initial draft. Five modules: (1) remove the market-keyed surface,
  derivatives, and their tables; (2) add per-coin `live_poll_interval` (register + update,
  null = reset to global, resets `last_polled_at` + in-flight marker), mirroring
  ticker-collector; (3) coin-level spot quote reads (`/v1/coins/{coin_id}/quotes` +
  `/quotes/latest`); (4) coin-level OHLCV reads (`/v1/coins/{coin_id}/candles`, `interval`
  required); (5) WebSocket streams `/v1/coins/stream/quotes` and `/v1/coins/stream/candles`
  delivered cross-replica via PostgreSQL LISTEN/NOTIFY. Binance preferred upstream for spot
  quotes and OHLCV. New `REQ-API-1NN` range; SPEC-API-001 retains `REQ-API-0NN`.

---

## Goal

Retire the market-keyed API entirely and present a single coin-keyed read model. A client
that knows a `coin_id` (e.g. `bitcoin`) can: register/update it with an optional per-coin
live poll cadence; read its latest spot quote and quote history; read its OHLCV candles by
interval and time range; and subscribe over WebSocket to live quote and candle pushes — all
sourced preferentially from Binance and described by the OpenAPI v3.1 document at
`api/crypto-collector.yaml`, kept in parity with the handlers.

## Scope

In scope:
- **Removal** of every market-keyed surface element: markets CRUD + search, market-scoped
  quotes/candles/derivatives reads, the derivatives module, the market DTOs/cursor keys, the
  `/markets` OpenAPI paths and Market/Derivative schemas, and the related doc-parity
  operationIds. Drop of `tracked_markets`, market-scoped `quotes`, market-scoped `candles`,
  and `derivatives_quotes` tables.
- **Per-coin `live_poll_interval`** on `tracked_coins`: optional at registration, settable
  / resettable on update (null = global cadence), validated against configured min/max,
  surfaced in the Coin response as a canonical H/M/S string.
- **Coin-keyed spot reads**: `GET /v1/coins/{coin_id}/quotes/latest` and
  `GET /v1/coins/{coin_id}/quotes` (keyset-paginated, time-range, `vs_currency`), backed by a
  new `coin_quotes` table.
- **Coin-keyed OHLCV reads**: `GET /v1/coins/{coin_id}/candles` (`interval` required,
  keyset-paginated, time-range, `vs_currency`), backed by a new `coin_candles` table.
- **WebSocket streams**: `GET /v1/coins/stream/quotes` and `GET /v1/coins/stream/candles`
  (RFC 6455 upgrade, per-connection in-memory subscriptions, cross-replica delivery via
  PostgreSQL LISTEN/NOTIFY, no auth, no backfill on connect).
- **Binance** as the preferred upstream for coin spot quotes and OHLCV candles.
- OpenAPI v3.1 parity for all of the above (add/remove paths and schemas).

Out of scope: see Exclusions. Coin metadata and coin market-aggregate reads
(`/v1/coins/{coin_id}/metadata`, `/v1/coins/{coin_id}/market*`) are unchanged. The collection
machinery and provider chain themselves are SPEC-SCHED-001/PROV-001; this SPEC only states the
*preference* and the data contract the reads/streams consume.

## Decisions Restated (authoritative)

- **D1 — Coin is the only read key.** All spot/OHLCV reads and streams are coin-keyed.
  The pair/market registry and every market-scoped path are deleted, not deprecated.
- **D2 — Data loss on the market tables is accepted.** Migration `0011` drops
  `tracked_markets` (cascading its dependent `quotes`/`candles`/`derivatives_quotes` rows)
  and creates the fresh coin-keyed `coin_quotes` / `coin_candles` tables. No migration of
  legacy rows.
- **D3 — `live_poll_interval` mirrors ticker-collector** verbatim in semantics: optional
  H/M/S string at register, nullable on update where null resets to the global cadence, and
  any change also resets `last_polled_at` and the in-flight marker. Bounds: `>=
  max(LIVE_POLL_MIN_INTERVAL_SECS, global tick)` and `<= LIVE_POLL_MAX_INTERVAL_SECS`.
- **D4 — Binance preferred for spot + OHLCV.** CoinGecko's monthly credit cap makes it
  unsuitable for live cadence; CoinGecko is retained only for coin search and metadata.
- **D5 — WebSocket delivery is cross-replica via PostgreSQL LISTEN/NOTIFY**, matching
  ticker-collector. Subscriptions are per-connection, in-memory only; no auth; no backfill.
- **D6 — Keyset pagination, DecimalString, doc-parity carry over** unchanged from
  SPEC-API-001 (REQ-API-070/073/003): opaque base64url cursors, lossless `Decimal`→JSON
  string, OpenAPI kept in parity by a doc-parity test.

---

## Change Inventory (brownfield delta markers)

[REMOVE]
- `src/api/markets.rs` (market CRUD: 6 handlers + helpers + tests).
- `src/api/derivatives.rs` (market-scoped derivatives reads — no coin-level equivalent).
- Market route registrations in `src/api/mod.rs` (`build_api_router`).
- Market DTOs in `src/api/dto.rs`: `MarketDto`, `MarketPage`, `RegisterMarketRequest`,
  `UpdateMarketRequest`, `MarketSearchPage`, `MarketSearchResult`.
- Market cursor key `MarketListKey` in `src/api/cursor.rs`.
- `/markets*` paths and `Market`/`Derivative` schemas in `api/crypto-collector.yaml`.
- Market/derivative operationIds in the `openapi_yaml_contains_all_operation_ids` doc-parity
  test in `src/api/mod.rs`.

[MODIFY]
- `src/api/coins.rs` — `register_coin` / `update_coin` handle `live_poll_interval`.
- `src/api/dto.rs` — add `live_poll_interval` to `CoinDto`, `RegisterCoinRequest`, and
  `UpdateCoinRequest` (nullable on update = reset to global).
- `src/api/mod.rs` — drop market routes; add coin quote/candle/WebSocket routes; update the
  doc-parity test.
- `src/models/coin.rs` — add `live_poll_interval` to `TrackedCoin`.
- `src/api/quotes.rs` — re-base from market-scoped to coin-scoped reads.
- `src/api/candles.rs` — re-base from market-scoped to coin-scoped reads.
- `api/crypto-collector.yaml` — remove markets section; add `live_poll_interval` to coin
  schemas; add coin quote/candle/WebSocket paths.

[NEW]
- `src/api/websocket.rs` — WebSocket handlers for `/v1/coins/stream/quotes` and
  `/v1/coins/stream/candles`.
- `migrations/0010_coin_live_poll_interval.sql` — add `live_poll_interval` INTERVAL column.
- `migrations/0011_remove_markets.sql` — drop market tables; create `coin_quotes` and
  `coin_candles`.

---

## Design Summary (WHAT, not HOW)

### Module 1 — Markets removal

The `/v1/markets` resource and every path nested under it cease to exist; requests to any
former market path return 404 (no route registered). The derivatives domain is removed
outright. The schema drops `tracked_markets` and its dependents. The OpenAPI document and its
doc-parity test no longer mention markets or derivatives.

### Module 2 — Per-coin live poll interval

`tracked_coins` gains a nullable `live_poll_interval` (PostgreSQL INTERVAL; null = global
cadence). Registration accepts an optional H/M/S string; update accepts a nullable H/M/S
string where null resets to the global cadence. Any change (set, change, or reset) also
resets `last_polled_at` and the in-flight marker so the new cadence takes effect on the next
tick. Out-of-bounds values are rejected (422). The Coin response carries the value as a
canonical H/M/S string (absent/null ⇒ global). Live polling resolves the coin to a Binance
symbol and prefers Binance upstream.

### Module 3 — Coin-level spot quote reads

- `GET /v1/coins/{coin_id}/quotes/latest?vs_currency=` — newest `coin_quotes` row for the
  coin (`vs_currency` defaults to `usd`), or 404 if the coin is unknown.
- `GET /v1/coins/{coin_id}/quotes?vs_currency=&start=&end=&cursor=&limit=` — keyset-
  paginated, time-range-filtered quote history with `next_cursor`.

Quote shape: `coin_id`, `ts`, `price` (DecimalString), `vs_currency`, `source`. Backed by
`coin_quotes` (`coin_id` TEXT FK → `tracked_coins`, `ts` TIMESTAMPTZ, `price` NUMERIC,
`vs_currency` TEXT, `source` TEXT). Binance is the preferred source.

### Module 4 — Coin-level OHLCV candle reads

- `GET /v1/coins/{coin_id}/candles?interval=&vs_currency=&start=&end=&cursor=&limit=` —
  keyset-paginated OHLCV for the interval and range. `interval` is **required** and validated
  against the supported set `1m, 5m, 15m, 1h, 4h, 1d, 1w` (the SPEC-API-001 OR-API-1 set);
  absent/invalid ⇒ 400 without querying.

Candle shape: `coin_id`, `interval`, `ts`, `open`, `high`, `low`, `close`, `volume` (all
DecimalString), `vs_currency`, `source`. Backed by `coin_candles` (`coin_id` TEXT FK,
`interval` TEXT, `ts` TIMESTAMPTZ, OHLCV NUMERIC columns, `vs_currency` TEXT, `source` TEXT).
Binance is the preferred source.

### Module 5 — WebSocket streaming

Two RFC 6455 endpoints reachable via HTTP Upgrade; on success the server returns 101. They
carry no authentication and perform no backfill on connect; subscriptions are per-connection,
in-memory only. Server pushes are delivered cross-replica via PostgreSQL LISTEN/NOTIFY.

- `GET /v1/coins/stream/quotes`
  - Control (client→server): `{"action":"subscribe"|"unsubscribe","coin_id":"bitcoin"}`
  - Push (server→client): `{"coin_id":"bitcoin","symbol":"BTC","ts":"...","price":"67123.45","vs_currency":"usd","source":"binance"}`
  - Error (server→client): `{"error":"invalid_message","message":"..."}`
- `GET /v1/coins/stream/candles`
  - Control: `{"action":"subscribe"|"unsubscribe","coin_id":"bitcoin","interval":"1m"}`
  - Push: `{"coin_id":"bitcoin","interval":"1m","ts":"...","open":"...","high":"...","low":"...","close":"...","volume":"...","vs_currency":"usd"}`
  - Error: `{"error":"invalid_message","message":"..."}`

The `stream` path segments must resolve to the WebSocket handlers and must not be captured by
the `{coin_id}` path parameter.

---

## Requirements (EARS)

### Module 1 — Markets removal

- **REQ-API-100** (Unwanted): The system shall not expose any market CRUD or search endpoint
  (`POST/GET/PATCH/DELETE /v1/markets`, `GET /v1/markets/{id}`, `GET /v1/markets/search`).
- **REQ-API-101** (Unwanted): The system shall not expose any market-scoped read endpoint
  (`/v1/markets/{id}/quotes`, `/quotes/latest`, `/candles`, `/derivatives`,
  `/derivatives/latest`).
- **REQ-API-102** (Event-Driven): When a client requests any former `/v1/markets*` path, the
  system shall respond 404 because no such route is registered.
- **REQ-API-103** (Ubiquitous): The system shall remove the derivatives module in its entirety;
  no coin-level derivatives endpoint replaces it.
- **REQ-API-104** (Ubiquitous): The schema shall drop the `tracked_markets`, market-scoped
  `quotes`, market-scoped `candles`, and `derivatives_quotes` tables in migration `0011`,
  accepting cascade deletion of their rows.
- **REQ-API-105** (Ubiquitous): The system shall remove the market DTOs (`MarketDto`,
  `MarketPage`, `RegisterMarketRequest`, `UpdateMarketRequest`, `MarketSearchPage`,
  `MarketSearchResult`) and the `MarketListKey` cursor key type.
- **REQ-API-106** (Ubiquitous): The OpenAPI document shall remove all `/markets*` paths and the
  `Market`/`Derivative` schemas, and shall remain in parity with the handlers (REQ-API-003).
- **REQ-API-107** (Ubiquitous): The OpenAPI doc-parity test shall no longer reference any market
  or derivative operationId.

### Module 2 — Per-coin live poll interval

- **REQ-API-110** (Ubiquitous): The `tracked_coins` table shall have a nullable
  `live_poll_interval` column (PostgreSQL INTERVAL) where null denotes the global cadence
  (migration `0010`).
- **REQ-API-111** (Event-Driven): When a client POSTs a coin registration carrying an optional
  `live_poll_interval` H/M/S duration string, the system shall validate it and persist it on the
  new `tracked_coins` record; when the field is omitted, the coin uses the global cadence.
- **REQ-API-112** (Event-Driven): When a client PATCHes a coin's `live_poll_interval` to a
  non-null H/M/S value, the system shall persist the new cadence and shall reset that coin's
  `last_polled_at` and in-flight marker.
- **REQ-API-113** (Event-Driven): When a client PATCHes a coin's `live_poll_interval` to null,
  the system shall reset the coin to the global cadence and shall reset its `last_polled_at` and
  in-flight marker.
- **REQ-API-114** (If/Unwanted): If a supplied `live_poll_interval` is less than
  `max(LIVE_POLL_MIN_INTERVAL_SECS, global tick)` or greater than `LIVE_POLL_MAX_INTERVAL_SECS`,
  then the system shall respond 422 and shall not persist the value.
- **REQ-API-115** (Ubiquitous): The Coin response schema shall include `live_poll_interval` as a
  canonical H/M/S string (e.g. `5m`, `1h30m`); absent/null means the global cadence is in effect.
- **REQ-API-116** (State-Driven): While a coin's live quotes are being polled, the system shall
  prefer the Binance provider as the upstream source and shall fall back through the configured
  provider chain only when Binance cannot serve the coin (CoinGecko carries a monthly credit cap).

### Module 3 — Coin-level spot quote reads

- **REQ-API-120** (Event-Driven): When a client requests `GET /v1/coins/{coin_id}/quotes/latest`
  (optional `vs_currency`, defaulting to `usd`), the system shall return the newest `coin_quotes`
  row for that coin and currency, or 404 if the coin is unknown.
- **REQ-API-121** (Event-Driven): When a client requests `GET /v1/coins/{coin_id}/quotes` with
  optional `vs_currency`/`start`/`end`/`cursor`/`limit`, the system shall return a keyset-
  paginated, time-range-filtered page of quote history with a `next_cursor`.
- **REQ-API-122** (Ubiquitous): A coin quote shall carry `coin_id`, `ts`, `price` (serialised
  losslessly as a DecimalString), `vs_currency`, and `source`, persisted in `coin_quotes`.
- **REQ-API-123** (Ubiquitous): The system shall record Binance as the preferred `source` for
  coin spot quotes.

### Module 4 — Coin-level OHLCV candle reads

- **REQ-API-130** (Event-Driven): When a client requests `GET /v1/coins/{coin_id}/candles` with
  `interval` (required) and optional `vs_currency`/`start`/`end`/`cursor`/`limit`, the system
  shall return a keyset-paginated page of OHLCV candles for that interval and range.
- **REQ-API-131** (If/Unwanted): If `interval` is absent or not in the supported set
  (`1m, 5m, 15m, 1h, 4h, 1d, 1w`), then the system shall respond 400 without querying.
- **REQ-API-132** (Ubiquitous): A coin candle shall carry `coin_id`, `interval`, `ts`, `open`,
  `high`, `low`, `close`, `volume` (all serialised losslessly as DecimalStrings), `vs_currency`,
  and `source`, persisted in `coin_candles`.
- **REQ-API-133** (Ubiquitous): The system shall record Binance as the preferred `source` for
  coin OHLCV candles.

### Module 5 — WebSocket streaming

- **REQ-API-140** (Event-Driven): When a client performs an HTTP Upgrade handshake on
  `GET /v1/coins/stream/quotes`, the system shall switch protocols (101) per RFC 6455; a request
  that is not a valid WebSocket upgrade shall receive 400.
- **REQ-API-141** (Event-Driven): When a connected quote-stream client sends a control frame
  `{"action":"subscribe"|"unsubscribe","coin_id":"..."}`, the system shall add or remove a
  per-connection, in-memory subscription for that coin.
- **REQ-API-142** (Event-Driven): When a quote for a subscribed coin is collected and persisted,
  the system shall push `{coin_id, symbol, ts, price, vs_currency, source}` to that connection,
  delivered cross-replica via PostgreSQL LISTEN/NOTIFY.
- **REQ-API-143** (If/Unwanted): If a quote-stream control frame is malformed (bad JSON, unknown
  `action`, or missing `coin_id`), then the system shall send
  `{"error":"invalid_message","message":"..."}` and shall keep the connection open.
- **REQ-API-144** (Event-Driven): When a client performs an HTTP Upgrade handshake on
  `GET /v1/coins/stream/candles`, the system shall switch protocols (101) per RFC 6455; control
  frames carry both `coin_id` and `interval`.
- **REQ-API-145** (Event-Driven): When a candle for a subscribed `(coin_id, interval)` is
  persisted, the system shall push `{coin_id, interval, ts, open, high, low, close, volume,
  vs_currency}` to that connection, delivered cross-replica via PostgreSQL LISTEN/NOTIFY.
- **REQ-API-146** (If/Unwanted): If a candle-stream control frame is malformed (bad JSON, unknown
  `action`, missing `coin_id`, or an `interval` outside the supported set), then the system shall
  send `{"error":"invalid_message","message":"..."}` and shall keep the connection open.
- **REQ-API-147** (Unwanted): The WebSocket endpoints shall not require authentication and shall
  not backfill historical data on connect; subscriptions shall be per-connection and in-memory
  only.
- **REQ-API-148** (Ubiquitous): The `/v1/coins/stream/quotes` and `/v1/coins/stream/candles`
  paths shall resolve to the WebSocket handlers and shall not be interpreted as a `{coin_id}` of
  `stream`.

### Carried-over contracts (from SPEC-API-001, restated for the new endpoints)

- **REQ-API-150** (Ubiquitous): Every new list read (`/quotes`, `/candles`) shall use an opaque
  base64url-no-pad keyset cursor encoding the ordering-key tuple of the last returned row and
  shall return a `next_cursor` that is null when exhausted (per REQ-API-070/071); an undecodable
  cursor ⇒ 400.
- **REQ-API-151** (Ubiquitous): Every new list read shall accept a `limit` validated against a
  documented maximum, rejecting out-of-range values with 400 (per REQ-API-072).
- **REQ-API-152** (Ubiquitous): Every monetary/quantity value on the new endpoints and streams
  shall serialise losslessly from `Decimal` with no `f64` round-trip (per REQ-API-073,
  REQ-PROV-012).
- **REQ-API-153** (Ubiquitous): The new coin quote/candle/WebSocket endpoints shall be described
  in `api/crypto-collector.yaml` and kept in parity with the handlers by the doc-parity test (per
  REQ-API-003).

## Exclusions (What NOT to Build)

- **No candle interval changes** — the supported set stays `1m, 5m, 15m, 1h, 4h, 1d, 1w`
  (SPEC-API-001 OR-API-1). No new intervals, no interval-validation rewrite. (See OR-API2-3 on a
  wording discrepancy in the request brief.)
- **No authentication/authorization for WebSocket** connections (nor for the REST surface) — the
  service stays internal/unauthenticated, matching ticker-collector and SPEC-API-001.
- **No backfill of historical data on WebSocket connect** — streams deliver only post-connect
  pushes; clients use the REST history endpoints for backfill.
- **No migration of existing market-scoped quote/candle/derivative data** — `0011` drops the
  market tables; data loss on deploy is accepted (D2).
- **No new provider implementations** — Binance is already in the chain (SPEC-PROV-001); this SPEC
  only states the *preference*, not a new provider.
- **No changes to the CoinGecko provider** — it remains the source for coin search and metadata.
- **No coin-level derivatives endpoints** — derivatives are removed with no replacement.
- **No changes to coin metadata or coin market-aggregate reads** — `/v1/coins/{coin_id}/metadata`
  and `/v1/coins/{coin_id}/market*` are untouched.
- **No changes to health, metrics, or telemetry** endpoints (SPEC-OBS-001).
- **No collection/scheduler redesign** — `live_poll_interval` reuses the existing live-poller
  cadence machinery (SPEC-SCHED-001); only the per-coin override and its reset semantics are new.

## @MX Annotation Targets (high fan_in)

- The coin keyset cursor encode/decode helpers for `coin_quotes` / `coin_candles` —
  `@MX:ANCHOR` (every new list endpoint depends on the stable-under-appends contract) +
  `@MX:WARN`/`@MX:REASON`: keyset, not OFFSET, over append-heavy time-series tables
  (REQ-API-150).
- The `live_poll_interval` parse/format/validate path (H/M/S string ⇄ INTERVAL, bounds check) —
  `@MX:ANCHOR` (shared by register + update; bounds are a correctness invariant, REQ-API-114) +
  `@MX:NOTE` on the reset-of-`last_polled_at`/in-flight side effect (REQ-API-112/113).
- The WebSocket LISTEN/NOTIFY fan-out (channel → subscriber dispatch) — `@MX:WARN`/`@MX:REASON`:
  per-connection subscription state and cross-replica delivery; NOTIFY payload ≤ 8 KB.
- The OpenAPI doc-parity test — `@MX:NOTE` that adding/removing a coin stream/quote/candle handler
  requires updating `api/crypto-collector.yaml` (REQ-API-153).

## Open Items (do not guess)

- **OR-API2-1:** primary-key / partitioning strategy for `coin_quotes` and `coin_candles`.
  Recommend composite PK `(coin_id, vs_currency, ts)` for quotes and
  `(coin_id, vs_currency, interval, ts)` for candles, mirroring SPEC-DB-001's time-series tables;
  confirm at run.
- **OR-API2-2:** PostgreSQL LISTEN/NOTIFY channel naming and payload encoding for the two streams
  (e.g. `coin_quote` / `coin_candle` channels carrying the push JSON). Rule normative
  (REQ-API-142/145); exact channel names confirmed at run.
- **OR-API2-3:** the request brief's Exclusions wording lists the interval set without `1d`, while
  Module 4 and SPEC-API-001 OR-API-1 include `1d`. This SPEC treats `1m, 5m, 15m, 1h, 4h, 1d, 1w`
  as authoritative (REQ-API-131); confirm at run.
- **OR-API2-4:** the exact config env-var names for the bounds (`LIVE_POLL_MIN_INTERVAL_SECS`,
  `LIVE_POLL_MAX_INTERVAL_SECS`, global tick) must match `src/config.rs`; rule normative
  (REQ-API-114), names confirmed against config at run.
- **OR-API2-5:** `vs_currency` default and the set of supported currencies for the coin quote/
  candle reads. Recommend `usd` default (REQ-API-120); supported set confirmed at run.
- **OR-API2-6:** `live_poll_interval` Rust representation across the sqlx boundary (PgInterval vs
  chrono::Duration) and the H/M/S formatter. Rule normative (REQ-API-115); representation chosen
  at run.

---

## Implementation Notes

_Added at sync time (2026-06-29, commit `be27841`)._

All five modules implemented and quality-gate verified (cargo fmt, clippy -D warnings, cargo test — 315 tests pass). The complete API surface matches the SPEC.

**Deferred to follow-up:**
- **T-A (collector re-base)**: `src/collectors/live_poller.rs` was not rebased to emit `NOTIFY coin_quote_updated` / `NOTIFY coin_candle_updated` after upserts. The WebSocket stream infrastructure (`src/listener.rs`, `src/api/websocket.rs`, `AppState::coin_quote_tx`/`coin_candle_tx`) is fully in place; the broadcast will activate once T-A is done in a follow-up SPEC.
- **T-B (model cleanup)**: `src/models/derivatives.rs` and references in `tests/model_serde.rs` are still present. No compilation errors; deferred minor cleanup.

**Scenarios verified by static/unit tests:**
- Scenarios 1–8: handler unit tests and OpenAPI doc-parity tests in `src/api/mod.rs` confirm the endpoint contracts.
- Scenarios 9–12: require live DB (`cargo test -- --ignored`); stream scenarios additionally require T-A for the push path to be exercised end-to-end.
