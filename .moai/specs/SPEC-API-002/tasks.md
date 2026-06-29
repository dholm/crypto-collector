---
id: SPEC-API-002
type: tasks
updated: 2026-06-29
---

# SPEC-API-002 — Task Decomposition

Delta order is enforced: all `[REMOVE]` complete before `[MODIFY]`, all `[MODIFY]` before `[NEW]`.
TDD per task: write/extend the test first (RED), implement (GREEN), refactor. Each task must end
with the tree compiling AND `cargo clippy --all-targets --all-features -- -D warnings` clean (no
orphaned/unused items) AND `cargo test` (non-ignored) green.

Scope of this list = the five API-layer modules (src/api, src/models, migrations, OpenAPI) named in
the SPEC change inventory. The producer-side collector re-base required for full end-to-end function
is tracked separately in the "Producer-side dependency" section below (SCOPE DECISION REQUIRED).

## Core tasks (in-scope)

| ID | Δ | Description | REQ | Files | Pred | Done criterion |
|----|---|-------------|-----|-------|------|----------------|
| T-001 | REMOVE+NEW (DB) | Author migration `0010` (ALTER `tracked_coins` ADD `live_poll_interval` INTERVAL + `last_polled_at` TIMESTAMPTZ + `live_poll_claimed_until` TIMESTAMPTZ, all NULL; partial claim index on `(last_polled_at) WHERE status='active'`) and `0011` (DROP IF EXISTS `derivatives_quotes`, `candles`, `live_quotes`, `tracked_markets` in FK-safe order; CREATE IF NOT EXISTS `coin_quotes` PK `(coin_id,vs_currency,ts)` and `coin_candles` PK `(coin_id,vs_currency,interval,ts)`, RANGE(ts) monthly partitions 2024–2027, btree + BRIN, FK→`tracked_coins` ON DELETE CASCADE, NUMERIC columns, nullable `volume`). Update `tests/migration_files.rs`. | 104,110,122,132,150,OR-API2-1 | `migrations/0010_coin_live_poll_interval.sql`, `migrations/0011_remove_markets.sql`, `tests/migration_files.rs` | — | `cargo test --test migration_files` green; DROP uses IF EXISTS, CREATE uses IF NOT EXISTS, no DOUBLE PRECISION |
| T-002 | REMOVE | Delete market + derivatives API surface: remove `markets.rs`, `derivatives.rs`; drop their `pub mod` + route block in `mod.rs`; remove `MarketDto`/`MarketPage`/`RegisterMarketRequest`/`UpdateMarketRequest`/`MarketSearchPage`/`MarketSearchResult`/`DerivativesQuoteDto` (+`From` impls + imports) from `dto.rs`; remove `MarketListKey` (+test + doc row) from `cursor.rs`; update `all_routes_are_under_v1`, `no_v2_routes_exist` route tests. (Lands together with T-004/T-005 — see Risk R2.) | 100,101,102,103,105 | `src/api/markets.rs` (del), `src/api/derivatives.rs` (del), `src/api/mod.rs`, `src/api/dto.rs`, `src/api/cursor.rs` | T-001 | `cargo check --all-targets` clean; no market route registered; no `Market*`/`Derivative*` API symbol remains |
| T-003 | MODIFY | Per-coin `live_poll_interval` end-to-end. Add `LIVE_POLL_MIN_INTERVAL_SECS` (default 5) + `LIVE_POLL_MAX_INTERVAL_SECS` (default 3600) to `config.rs`. Port ticker-collector `poll_interval.rs` as `src/api/poll_interval.rs` (H/M/S parse/format/`normalize_pg_interval`), mapping parse/bounds failure to **422** (`UnprocessableEntity`). Add `live_poll_interval: Option<String>` to `TrackedCoin` (every SELECT casts `live_poll_interval::TEXT`). Add field to `CoinDto` (out: canonical H/M/S), `RegisterCoinRequest` (optional), `UpdateCoinRequest` (tri-state, null=reset). `register_coin` validates+persists; `update_coin` set/change/reset, and on any change resets `last_polled_at` + `live_poll_claimed_until` in one statement. | 110,111,112,113,114,115,OR-API2-4,OR-API2-6 | `src/config.rs`, `src/api/poll_interval.rs` (new), `src/models/coin.rs`, `src/api/dto.rs`, `src/api/coins.rs` | T-002 | Scenarios 1–4 unit/handler tests green; bounds violation → 422; reset clears `last_polled_at`+marker |
| T-004 | NEW | Re-base spot quotes to coin-keyed reads. Add `CoinQuote` model (`coin_id,ts,price,vs_currency,source`) + `CoinQuoteDto` (DecimalString). Rewrite `quotes.rs`: `GET /v1/coins/{coin_id}/quotes/latest` (`vs_currency` default `usd`, 404 unknown coin) and `GET /v1/coins/{coin_id}/quotes` (keyset via existing `TsKey`, `vs_currency`/`start`/`end`/`cursor`/`limit`). Rename `ensure_market_exists`→`ensure_coin_exists`; keep `paginate_ts`. Register routes in `build_api_router`. | 120,121,122,150,151,152,OR-API2-5 | `src/models/quote.rs` (add `CoinQuote`), `src/api/dto.rs`, `src/api/quotes.rs`, `src/api/mod.rs` | T-003 | Scenarios 6,7 green; reuses `TsKey` (no new cursor key); 404 on unknown coin |
| T-005 | NEW | Re-base candles to coin-keyed reads. Add `CoinCandle` model + `CoinCandleDto` (OHLCV DecimalString, nullable `volume`). Rewrite `candles.rs`: `GET /v1/coins/{coin_id}/candles` (`interval` required + validated against `SUPPORTED_INTERVALS`, 400 absent/invalid; keyset; `vs_currency`; time-range). Register route. Keep/relocate `validate_interval` + `SUPPORTED_INTERVALS`. | 130,131,132,133,150,151,152,OR-API2-3 | `src/models/quote.rs` (add `CoinCandle`), `src/api/dto.rs`, `src/api/candles.rs`, `src/api/mod.rs` | T-004 | Scenario 8 green; interval set unchanged; 400 before querying on bad interval |
| T-006 | NEW | WebSocket handlers `src/api/websocket.rs`. `ControlMsg` (quotes: `{action,coin_id}`; candles: `{action,coin_id,interval}` validated against `SUPPORTED_INTERVALS`), `ErrorFrame{error:"invalid_message",message}`, `apply_control`, `should_deliver`. Two `WebSocketUpgrade` handlers (`stream_coin_quotes`, `stream_coin_candles`) with per-connection `HashSet` subscriptions, `tokio::select!` over socket + broadcast recv. Register `/v1/coins/stream/quotes` + `/v1/coins/stream/candles` **before** `/v1/coins/{coin_id}` routes. | 140,141,143,144,146,147,148 | `src/api/websocket.rs` (new), `src/api/mod.rs` | T-005 | Control-parse + subscription-filter unit tests green; 101 handshake + path-not-`{coin_id}` (Scenario 9a,10); malformed frame keeps conn open |
| T-007 | NEW | Cross-replica delivery plumbing. `AppState` gains `coin_quote_tx`/`coin_candle_tx` (`broadcast::Sender`); new `src/listener.rs` runs `PgListener` on channels `coin_quote_update`/`coin_candle_update` with backoff, deserializing NOTIFY payloads into the broadcasts; spawn both in `main.rs`; wire `.subscribe()` into the T-006 handlers. | 142,145,OR-API2-2 | `src/api/mod.rs` (AppState), `src/listener.rs` (new), `src/main.rs`, `src/api/websocket.rs` | T-006 | Payload deser + backoff unit tests green; AppState constructed with senders in all test fixtures; end-to-end push deferred to Producer-side (see R1) |
| T-008 | NEW | OpenAPI parity. In `api/crypto-collector.yaml`: remove all `/markets*` paths + `Market`/`Derivative` schemas; add `live_poll_interval` to `Coin`/register/update schemas; add coin quote (`getCoinLatestQuote`,`listCoinQuotes`), candle (`listCoinCandles`), and stream (`streamCoinQuotes`,`streamCoinCandles`) paths with `101`/`400` responses; add `CoinQuote`/`CoinCandle`/control/push schemas. | 106,115,153 | `api/crypto-collector.yaml` | T-007 | YAML valid 3.1.0; no `/markets`/`Market`/`Derivative`; new operationIds + schemas present |
| T-009 | NEW | Doc-parity + key-schema test sync and full gate. In `mod.rs`: `openapi_yaml_contains_all_operation_ids` drops market/deriv opIds and adds the new coin opIds; `openapi_yaml_contains_key_schemas` drops `Market`/`DerivativesQuote` and adds `CoinQuote`/`CoinCandle`. Run full gate. | 107,153 | `src/api/mod.rs` | T-008 | `cargo fmt --check` + `clippy -D warnings` + `cargo test` all green; doc-parity passes |

## Producer-side tasks (in-scope — included in SPEC-API-002)

Decision confirmed: T-A and T-B are in scope. Migration 0011 must not be deployed until T-A is complete.

| ID | Δ | Description | REQ | Files | Pred | Done criterion |
|----|---|-------------|-----|-------|------|----------------|
| T-A | MODIFY | Re-base live-poller + collection-queue + upserts to coin-keyed collection: claim due `tracked_coins`, resolve coin→Binance symbol, prefer Binance, write `coin_quotes`/`coin_candles`, emit `pg_notify('coin_quote_update'/'coin_candle_update', payload)` after upsert. Update `tests/db_integration.rs` market scenarios. | 116,123,133,142,145 | `src/collectors/live_poller.rs`, `src/collectors/collection_queue.rs`, `src/db/upserts.rs`, `tests/db_integration.rs` | T-007 | Scenarios 9,11,12 + DB-backed 6,8 pass under `cargo test -- --ignored` |
| T-B | REMOVE | Remove now-orphaned market models/upserts: `TrackedMarket`/`LiveQuote`/`Candle` (`models/quote.rs`), `models/derivatives.rs`, market upserts, and their `model_serde.rs` cases. | 103,105 | `src/models/quote.rs`, `src/models/derivatives.rs`, `src/db/upserts.rs`, `tests/model_serde.rs` | T-A | No orphaned market model/upsert; `cargo test --test model_serde` green |

---

## Completion Status (2026-06-29, commit `be27841`)

**Consumer-side API tasks (complete):**
- T-001 ✓ DONE
- T-002 ✓ DONE
- T-003 ✓ DONE
- T-004 ✓ DONE
- T-005 ✓ DONE
- T-006 ✓ DONE
- T-007 ✓ DONE
- T-008 ✓ DONE
- T-009 ✓ DONE

**Producer-side tasks (deferred to follow-up SPEC):**
- T-A ⏸ Deferred — live-poller re-base and NOTIFY emission will activate WebSocket broadcast in follow-up
- T-B ⏸ Deferred — orphaned model cleanup deferred pending T-A completion
