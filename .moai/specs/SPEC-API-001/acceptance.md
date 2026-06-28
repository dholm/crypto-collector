# Acceptance Criteria — SPEC-API-001 (REST API & OpenAPI v3.1)

Each scenario maps to EARS requirements in `spec.md`. Handler scenarios use
`axum-test`; data-backed reads may be gated (`#[ignore]`) on a live DB.

## Scenario 1 — Single /v1 surface, no /v2 (REQ-API-001)

- **Given** the assembled router and the OpenAPI document
- **When** routes are enumerated
- **Then** every endpoint is under `/v1`, there is no `/v2` path, and the OpenAPI
  `servers` entry is `/v1`.

## Scenario 2 — Register coin: 201 new, 200 existing, enqueues collection (REQ-API-010/011)

- **Given** an empty registry
- **When** a client POSTs a new coin
- **Then** the response is 201 with the record and initial collection work is enqueued;
  **when** the same coin is POSTed again, the response is 200 with the existing record
  (no duplicate, no error).

## Scenario 3 — Coins CRUD + search (REQ-API-012/013)

- **Given** tracked coins
- **When** the client lists/gets/patches/deletes and searches
- **Then** `GET /v1/coins` returns a keyset-paginated page, `GET /v1/coins/{id}` returns
  one or 404, `PATCH` updates mutable fields, `DELETE` deregisters, and
  `GET /v1/coins/search?q=` returns candidates capped at the documented maximum.

## Scenario 4 — Register market with optional venue; uniqueness (REQ-API-020/021/022)

- **Given** an empty market registry
- **When** the client POSTs `(BTC, USD, null)` then `(BTC, USD, binance)` then a second
  `(BTC, USD, null)`
- **Then** the first two return 201 (and enqueue collection + backfill), and the third
  returns 200 with the existing aggregator record (idempotent); list/filter/get/patch/
  delete behave per spec.

## Scenario 5 — Spot latest + history (REQ-API-030/031)

- **Given** a market with `live_quotes` rows
- **When** the client requests `/quotes/latest` and `/quotes?start&end&cursor&limit`
- **Then** `/latest` returns the newest row (or 404 for unknown market), and the history
  is keyset-paginated within the time range with a `next_cursor`.

## Scenario 6 — Candles require a valid interval; nullable volume (REQ-API-040/041/042)

- **Given** a market with candles at multiple intervals
- **When** the client requests `/candles?interval=1h&...`
- **Then** a keyset-paginated page of 1h OHLCV is returned; **when** `interval` is
  omitted or unknown, the response is 400 (no query); and a CoinGecko-sourced candle's
  `volume` is `null` in the response (not `0`).

## Scenario 7 — Coin metadata latest + as-of (REQ-API-050)

- **Given** a coin with metadata revisions r0 (`first_seen_at = T0`) and r1
  (`first_seen_at = T1 > T0`)
- **When** the client requests `/metadata` and `/metadata?as_of=t` with `T0 <= t < T1`
- **Then** `/metadata` returns r1 (latest) and the as-of request returns r0 (greatest
  `first_seen_at <= t`).

## Scenario 8 — Coin market aggregates latest + history (REQ-API-051/052)

- **Given** a coin with `coin_market_snapshots`
- **When** the client requests `/market/latest?vs_currency=usd` and `/market?...`
- **Then** `/latest` returns the newest market cap / FDV / supply snapshot, and the
  history is keyset-paginated.

## Scenario 9 — Derivatives latest + history (REQ-API-060/061)

- **Given** a derivative market with `derivatives_quotes`
- **When** the client requests `/derivatives/latest` and `/derivatives?...`
- **Then** `/latest` returns the newest funding/OI/mark/index/basis tick and the history
  is keyset-paginated.

## Scenario 10 — Keyset cursor stable + invalid cursor 400 (REQ-API-070/071)

- **Given** a list read returning a `next_cursor`
- **When** the client passes that cursor back
- **Then** the next page continues without skipping or duplicating rows even if new rows
  were appended; and **when** a malformed cursor is supplied, the response is 400.

## Scenario 11 — limit validated and capped (REQ-API-072)

- **Given** any list read
- **When** the client supplies `limit` above the documented maximum or non-numeric
- **Then** the response is 400; a valid `limit` bounds the page size.

## Scenario 12 — Lossless Decimal serialization (REQ-API-073)

- **Given** a quote with price `0.00000000001234` and a coin with supply
  `589000000000000`
- **When** serialised in a response
- **Then** the values round-trip exactly per the documented convention (no `f64`
  truncation).

## Scenario 13 — Uniform errors (REQ-API-074)

- **Given** various bad requests
- **When** the client requests an unknown coin/market id, a malformed body, or a
  semantically invalid registration
- **Then** the responses are 404, 400, and 422 respectively, each with the uniform JSON
  error body.

## Scenario 14 — OpenAPI v3.1 published and in parity (REQ-API-002/003)

- **Given** `api/crypto-collector.yaml`
- **When** the doc-parity test runs
- **Then** the document is OpenAPI 3.1.0 with `servers: /v1`, every implemented endpoint
  operationId and schema name appears in it, and the test fails if a handler is
  added/changed without updating the document.

## Scenario 15 — Search endpoints return 503/429 when the provider pacer is unavailable (REQ-API-080/081)

- **Given** a provider that is in cooldown or has an exhausted monthly credit budget
  (SPEC-PROV-001 REQ-PROV-041/043), or a pacer slot that cannot be acquired within the
  bounded request-scoped wait
- **When** a client calls `GET /v1/coins/search?q=` or `GET /v1/markets/search?q=`
- **Then** the handler responds 503 (or 429 when surfaced as a client-visible rate limit)
  without blocking the request thread and without issuing an unpaced upstream call
  (SPEC-OBS-001 REQ-OBS-051); and when a slot is available, the search call counts against
  the same per-provider credit budget as worker traffic (no separate egress path).

## Quality Gate / Definition of Done

- [ ] Single `/v1`, no `/v2`, no OFFSET pagination (1, 10).
- [ ] Search endpoints pace request-path provider calls; 503/429 when slot/cooldown/credit
      unavailable, no blocking, same credit budget as workers (15).
- [ ] Coins + markets management idempotent (201/200), CRUD + search (2, 3, 4).
- [ ] Reads for all three domains: spot, candles (valid interval, nullable volume),
      metadata (latest + as-of), coin market, derivatives — all keyset-paginated (5–9).
- [ ] Keyset cursor stable under appends; invalid cursor 400; limit validated (10, 11).
- [ ] Lossless `Decimal` serialization (12).
- [ ] Uniform error model 400/404/422/500 (13).
- [ ] OpenAPI 3.1.0 at `api/crypto-collector.yaml` in parity with handlers (14).
- [ ] `cargo sqlx prepare` verified against live Postgres for read queries.
- [ ] Open items OR-API-1..4 resolved or explicitly deferred with user sign-off.
