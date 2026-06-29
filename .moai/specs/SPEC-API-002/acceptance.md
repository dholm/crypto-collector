---
id: SPEC-API-002
type: acceptance
updated: 2026-06-29
---

# SPEC-API-002 — Acceptance Criteria

Given/When/Then scenarios covering all five modules. Each maps to one or more `REQ-API-1NN`.
Decimal fields are asserted as JSON strings (DecimalString, REQ-API-152). Unless stated, the
service is running with migrations `0010` and `0011` applied.

## Scenario 1 — Register a coin with `live_poll_interval` (REQ-API-111, 115)

- Given an empty `tracked_coins` table and `LIVE_POLL_MIN_INTERVAL_SECS`/global tick that admit
  `5m`,
- When the client POSTs `/v1/coins` with body `{"coin_id":"bitcoin","live_poll_interval":"5m"}`,
- Then the response is 201 and the returned Coin has `live_poll_interval == "5m"` (canonical
  H/M/S string),
- And a subsequent `GET /v1/coins/bitcoin` returns `live_poll_interval == "5m"`.

## Scenario 2 — Register a coin without `live_poll_interval` uses the global cadence (REQ-API-111, 115)

- Given an empty `tracked_coins` table,
- When the client POSTs `/v1/coins` with body `{"coin_id":"ethereum"}` (field omitted),
- Then the response is 201 and the returned Coin has `live_poll_interval` absent/null (global
  cadence in effect).

## Scenario 3 — Update `live_poll_interval` and reset to global (REQ-API-112, 113)

- Given `bitcoin` is registered with `live_poll_interval == "5m"` and has a non-null
  `last_polled_at` and a set in-flight marker,
- When the client PATCHes `/v1/coins/bitcoin` with `{"live_poll_interval":"1h30m"}`,
- Then the response is 200, `live_poll_interval == "1h30m"`, and the coin's `last_polled_at` and
  in-flight marker have been reset,
- And When the client then PATCHes `/v1/coins/bitcoin` with `{"live_poll_interval":null}`,
- Then the response is 200, `live_poll_interval` is absent/null (global cadence), and
  `last_polled_at` and the in-flight marker are reset again.

## Scenario 4 — Invalid `live_poll_interval` rejected (REQ-API-114)

- Given configured bounds where the effective minimum is `30s` and the maximum is `24h`,
- When the client POSTs `/v1/coins` with `{"coin_id":"solana","live_poll_interval":"5s"}` (below
  minimum),
- Then the response is 422 and no `tracked_coins` row is created for `solana`,
- And When the client POSTs `{"coin_id":"solana","live_poll_interval":"48h"}` (above maximum),
- Then the response is 422 and no row is created.

## Scenario 5 — Former market endpoints return 404 after removal (REQ-API-100, 101, 102)

- Given the service built from this SPEC,
- When the client requests `GET /v1/markets`, `POST /v1/markets`, `GET /v1/markets/1`,
  `GET /v1/markets/1/quotes/latest`, `GET /v1/markets/1/candles`, and
  `GET /v1/markets/1/derivatives/latest`,
- Then every one of these responses is 404 (no such route registered),
- And the OpenAPI document at `api/crypto-collector.yaml` contains no `/markets*` path and no
  `Market` or `Derivative` schema.

## Scenario 6 — Coin quotes latest + history pagination (REQ-API-120, 121, 122, 150)

- Given `bitcoin` is registered and `coin_quotes` holds 3 rows for `vs_currency=usd` at
  ascending `ts`,
- When the client requests `GET /v1/coins/bitcoin/quotes/latest`,
- Then the response is 200 with the newest row: `coin_id=="bitcoin"`, `price` a DecimalString,
  `vs_currency=="usd"`, and `source` present,
- And When the client requests `GET /v1/coins/bitcoin/quotes?limit=2`,
- Then the response returns 2 items and a non-null `next_cursor`,
- And When the client requests the same path with `cursor=<next_cursor>`,
- Then the response returns the remaining 1 item and a null `next_cursor`.

## Scenario 7 — Unknown coin on quote read returns 404 (REQ-API-120)

- Given no `tracked_coins` row for `dogecoin`,
- When the client requests `GET /v1/coins/dogecoin/quotes/latest`,
- Then the response is 404 with the uniform error body.

## Scenario 8 — Coin candles require a valid `interval` (REQ-API-130, 131, 132)

- Given `bitcoin` is registered and `coin_candles` holds rows for `interval=1m`,
- When the client requests `GET /v1/coins/bitcoin/candles` with no `interval` query param,
- Then the response is 400 and no query is executed,
- And When the client requests `GET /v1/coins/bitcoin/candles?interval=2m` (not in the supported
  set),
- Then the response is 400,
- And When the client requests `GET /v1/coins/bitcoin/candles?interval=1m`,
- Then the response is 200 and each item carries `coin_id`, `interval=="1m"`, `ts`, and
  `open`/`high`/`low`/`close`/`volume` as DecimalStrings plus `vs_currency` and `source`.

## Scenario 9 — WebSocket quotes: subscribe, receive, unsubscribe (REQ-API-140, 141, 142, 148)

- Given the service is running and `bitcoin` is registered,
- When the client opens a WebSocket to `GET /v1/coins/stream/quotes`,
- Then the handshake returns 101 (Switching Protocols) and the path resolves to the stream
  handler (not a coin id of `stream`),
- And When the client sends `{"action":"subscribe","coin_id":"bitcoin"}` and a new `bitcoin`
  quote is then collected and persisted (emitting NOTIFY),
- Then the client receives a push frame
  `{"coin_id":"bitcoin","symbol":"BTC","ts":"...","price":"<decimal-string>","vs_currency":"usd","source":"binance"}`,
- And When the client sends `{"action":"unsubscribe","coin_id":"bitcoin"}` and another `bitcoin`
  quote is persisted,
- Then the client receives no further push for `bitcoin`.

## Scenario 10 — WebSocket candles: invalid control frame returns error frame (REQ-API-144, 146, 147)

- Given a client connected to `GET /v1/coins/stream/candles` (handshake returned 101),
- When the client sends a malformed control frame `{"action":"subscribe","coin_id":"bitcoin"}`
  (missing `interval`),
- Then the client receives `{"error":"invalid_message","message":"..."}` and the connection
  remains open,
- And When the client sends `{"action":"subscribe","coin_id":"bitcoin","interval":"2m"}` (unknown
  interval),
- Then the client receives an `invalid_message` error frame and the connection remains open,
- And no historical candles are pushed on connect (no backfill).

## Scenario 11 — WebSocket candle push on persisted candle (REQ-API-145)

- Given a client connected to `/v1/coins/stream/candles` and subscribed with
  `{"action":"subscribe","coin_id":"bitcoin","interval":"1m"}`,
- When a `(bitcoin, 1m)` candle is persisted (emitting NOTIFY),
- Then the client receives
  `{"coin_id":"bitcoin","interval":"1m","ts":"...","open":"...","high":"...","low":"...","close":"...","volume":"...","vs_currency":"usd"}`
  with all OHLCV fields as DecimalStrings.

## Scenario 12 — Binance preferred source recorded (REQ-API-116, 123, 133)

- Given Binance is healthy in the provider chain and serves `bitcoin`,
- When live quotes and `1m` candles for `bitcoin` are collected and persisted,
- Then the persisted `coin_quotes` and `coin_candles` rows carry `source == "binance"`,
- And the read endpoints surface `source == "binance"` for those rows.

## Edge Cases

- Undecodable `cursor` on `/quotes` or `/candles` ⇒ 400 (REQ-API-150).
- `limit` out of range (0 or above the documented maximum) ⇒ 400 (REQ-API-151).
- WebSocket handshake attempted with a plain `GET` (no Upgrade headers) ⇒ 400 (REQ-API-140/144).
- Malformed JSON (non-JSON bytes) on a stream control channel ⇒ `invalid_message` error frame,
  connection stays open (REQ-API-143/146).
- `vs_currency` omitted on `/quotes/latest` ⇒ defaults to `usd` (REQ-API-120, OR-API2-5).

## Definition of Done

- [ ] Migrations `0010` and `0011` present and validated by `cargo test --test migration_files`.
- [ ] `src/api/markets.rs` and `src/api/derivatives.rs` deleted; no dangling references
      (`cargo check --all-targets` clean).
- [ ] Market DTOs, `MarketListKey`, market routes, and `/markets*` OpenAPI paths/schemas removed.
- [ ] `live_poll_interval` present on `TrackedCoin`, `CoinDto`, `RegisterCoinRequest`,
      `UpdateCoinRequest`, with set/change/reset semantics and bounds validation.
- [ ] Coin quote (`/quotes`, `/quotes/latest`) and candle (`/candles`) endpoints implemented,
      keyset-paginated, DecimalString-serialised.
- [ ] WebSocket `/v1/coins/stream/quotes` and `/v1/coins/stream/candles` implemented with
      per-connection subscriptions and cross-replica LISTEN/NOTIFY delivery.
- [ ] `api/crypto-collector.yaml` updated and the doc-parity test in `src/api/mod.rs` passes with
      new operationIds present and market/derivative ones gone.
- [ ] Quality gate green: `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D
      warnings`, `cargo test`.
- [ ] No `f64` used for any price/quantity value (REQ-PROV-012 / REQ-API-152).
- [ ] All twelve scenarios above pass; DB-backed scenarios verified via
      `DATABASE_URL=... cargo test -- --ignored`.
