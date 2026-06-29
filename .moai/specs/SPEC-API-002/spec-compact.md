---
id: SPEC-API-002
type: spec-compact
updated: 2026-06-29
---

# SPEC-API-002 (compact) — Markets removal, coin live-poll interval, coin quote/candle/WS APIs

## Requirements (EARS)

Module 1 — Markets removal
- REQ-API-100 (Unwanted): no market CRUD/search endpoints.
- REQ-API-101 (Unwanted): no market-scoped quote/candle/derivative reads.
- REQ-API-102 (Event): former `/v1/markets*` path → 404 (no route).
- REQ-API-103 (Ubiquitous): derivatives module removed entirely, no replacement.
- REQ-API-104 (Ubiquitous): drop `tracked_markets`, market `quotes`, market `candles`,
  `derivatives_quotes` (migration 0011; cascade accepted).
- REQ-API-105 (Ubiquitous): remove market DTOs + `MarketListKey`.
- REQ-API-106 (Ubiquitous): remove `/markets*` paths + Market/Derivative schemas; keep OpenAPI parity.
- REQ-API-107 (Ubiquitous): doc-parity test drops market/derivative operationIds.

Module 2 — Coin live-poll interval
- REQ-API-110 (Ubiquitous): `tracked_coins.live_poll_interval` INTERVAL nullable; null = global (mig 0010).
- REQ-API-111 (Event): POST coin with optional H/M/S `live_poll_interval` → validate + persist; omit = global.
- REQ-API-112 (Event): PATCH to non-null → persist + reset `last_polled_at` + in-flight marker.
- REQ-API-113 (Event): PATCH to null → reset to global + reset `last_polled_at` + in-flight marker.
- REQ-API-114 (If): value < max(LIVE_POLL_MIN_INTERVAL_SECS, global tick) or > LIVE_POLL_MAX_INTERVAL_SECS → 422, no persist.
- REQ-API-115 (Ubiquitous): Coin response includes `live_poll_interval` as canonical H/M/S string; absent/null = global.
- REQ-API-116 (State): while polling live quotes, prefer Binance; fall through chain only if Binance can't serve.

Module 3 — Coin spot quote reads
- REQ-API-120 (Event): GET `/v1/coins/{coin_id}/quotes/latest` (`vs_currency` default usd) → newest `coin_quotes` row, else 404.
- REQ-API-121 (Event): GET `/v1/coins/{coin_id}/quotes` (`vs_currency`/`start`/`end`/`cursor`/`limit`) → keyset page + `next_cursor`.
- REQ-API-122 (Ubiquitous): coin quote = {coin_id, ts, price(DecimalString), vs_currency, source} in `coin_quotes`.
- REQ-API-123 (Ubiquitous): Binance preferred `source` for coin spot quotes.

Module 4 — Coin OHLCV candle reads
- REQ-API-130 (Event): GET `/v1/coins/{coin_id}/candles` (`interval` required + `vs_currency`/`start`/`end`/`cursor`/`limit`) → keyset OHLCV page.
- REQ-API-131 (If): `interval` absent or not in {1m,5m,15m,1h,4h,1d,1w} → 400, no query.
- REQ-API-132 (Ubiquitous): coin candle = {coin_id, interval, ts, open, high, low, close, volume (DecimalString), vs_currency, source} in `coin_candles`.
- REQ-API-133 (Ubiquitous): Binance preferred `source` for coin OHLCV.

Module 5 — WebSocket streams (RFC 6455, no auth, no backfill, per-conn in-memory subs, cross-replica LISTEN/NOTIFY)
- REQ-API-140 (Event): Upgrade on GET `/v1/coins/stream/quotes` → 101; non-upgrade → 400.
- REQ-API-141 (Event): quote-stream control `{action: subscribe|unsubscribe, coin_id}` → add/remove sub.
- REQ-API-142 (Event): subscribed coin quote persisted → push {coin_id, symbol, ts, price, vs_currency, source} via NOTIFY.
- REQ-API-143 (If): malformed quote control → `{"error":"invalid_message","message":"..."}`, keep open.
- REQ-API-144 (Event): Upgrade on GET `/v1/coins/stream/candles` → 101; control carries coin_id + interval.
- REQ-API-145 (Event): subscribed (coin_id, interval) candle persisted → push {coin_id, interval, ts, open, high, low, close, volume, vs_currency} via NOTIFY.
- REQ-API-146 (If): malformed candle control (missing coin_id/interval or unknown interval) → error frame, keep open.
- REQ-API-147 (Unwanted): no auth, no backfill on connect; subs per-connection in-memory only.
- REQ-API-148 (Ubiquitous): `/coins/stream/*` resolves to WS handlers, not `{coin_id}=stream`.

Carried-over (SPEC-API-001)
- REQ-API-150 keyset base64url cursor + null-terminating `next_cursor`; bad cursor → 400.
- REQ-API-151 `limit` validated vs documented max; out-of-range → 400.
- REQ-API-152 lossless Decimal→JSON string, no f64.
- REQ-API-153 new endpoints in `api/crypto-collector.yaml`, doc-parity enforced.

## Files

[REMOVE] `src/api/markets.rs`, `src/api/derivatives.rs`; market routes in `src/api/mod.rs`;
market DTOs in `src/api/dto.rs`; `MarketListKey` in `src/api/cursor.rs`; `/markets*` paths +
Market/Derivative schemas in `api/crypto-collector.yaml`; market operationIds in mod.rs doc-parity test.

[MODIFY] `src/api/coins.rs`, `src/api/dto.rs`, `src/api/mod.rs`, `src/models/coin.rs`,
`src/api/quotes.rs`, `src/api/candles.rs`, `api/crypto-collector.yaml`.

[NEW] `src/api/websocket.rs`, `migrations/0010_coin_live_poll_interval.sql`,
`migrations/0011_remove_markets.sql`.

## Scenarios (Given/When/Then, condensed)

1. POST coin `{coin_id:bitcoin, live_poll_interval:"5m"}` → 201, returns `"5m"`; GET confirms. (111,115)
2. POST `{coin_id:ethereum}` (omitted) → 201, `live_poll_interval` null (global). (111,115)
3. PATCH `5m`→`1h30m` → 200 + reset last_polled_at/in-flight; PATCH →null → 200 global + reset. (112,113)
4. POST `live_poll_interval:"5s"` (<min) → 422 no row; `"48h"` (>max) → 422 no row. (114)
5. GET/POST/PATCH on `/v1/markets*` (incl /quotes/latest, /candles, /derivatives/latest) → all 404; YAML has no markets/derivatives. (100,101,102)
6. 3 usd quotes: `/quotes/latest` → newest (price DecimalString); `/quotes?limit=2` → 2 + next_cursor; cursor → last 1 + null cursor. (120,121,122,150)
7. `/v1/coins/dogecoin/quotes/latest` unknown coin → 404. (120)
8. `/candles` no interval → 400; `interval=2m` → 400; `interval=1m` → 200 OHLCV DecimalStrings. (130,131,132)
9. WS `/coins/stream/quotes` → 101; subscribe bitcoin + quote persisted → push frame; unsubscribe → no further push. (140,141,142,148)
10. WS `/coins/stream/candles` 101; control missing interval → invalid_message, open; interval=2m → invalid_message, open; no backfill. (144,146,147)
11. WS candles subscribed (bitcoin,1m) + candle persisted → OHLCV push DecimalStrings. (145)
12. Binance healthy → persisted coin_quotes/coin_candles `source=="binance"`; reads surface it. (116,123,133)

Edge: bad cursor→400 (150); limit out-of-range→400 (151); WS plain GET→400 (140,144); non-JSON control→invalid_message frame, open (143,146); `vs_currency` omitted on latest→usd (120).

## Exclusions (What NOT to Build)

- No candle interval changes — set stays {1m,5m,15m,1h,4h,1d,1w}.
- No auth for WebSocket or REST.
- No backfill of history on WS connect.
- No migration of legacy market quote/candle/derivative data (data loss accepted).
- No new provider implementations — reuse existing Binance in chain.
- No CoinGecko provider changes (still used for coin search + metadata).
- No coin-level derivatives endpoints.
- No changes to `/coins/{coin_id}/metadata` or `/coins/{coin_id}/market*`.
- No changes to health/metrics/telemetry.
- No scheduler/collection redesign — reuse existing live-poller cadence.
