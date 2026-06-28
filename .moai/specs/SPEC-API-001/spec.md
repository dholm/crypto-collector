---
id: API-001
version: 1.1.0
status: planned
created: 2026-06-28
updated: 2026-06-28
author: dholm
priority: high
issue_number: null
---

# SPEC-API-001 — REST API Surface & OpenAPI v3.1 Specification

Foundation SPEC for the externally-consumed REST API. Defines the endpoint surface
(management + read), the request/response shapes, the keyset pagination contract, the
error model, and the requirement to publish an OpenAPI v3.1 document at
`api/crypto-collector.yaml`.

Schema/data contract: [SPEC-DB-001](../SPEC-DB-001/spec.md). Read-data semantics:
[SPEC-PROV-001](../SPEC-PROV-001/spec.md) (degradation), [SPEC-SCHED-001](../SPEC-SCHED-001/spec.md)
(registration enqueues collection). Research:
[../SPEC-PROV-001/research.md](../SPEC-PROV-001/research.md) (§5 versioning, §4.6 keyset).

## HISTORY

- 2026-06-28 (v1.1.0): Specified request-path provider pacing for the search endpoints
  (REQ-API-080/081): the search handler acquires a SPEC-PROV-001 pacer slot with a
  bounded, request-scoped wait, counts against the same per-provider credit budget as
  worker traffic, and returns 503 (or 429) instead of blocking the request thread when
  the slot/cooldown/credit is unavailable — reconciling REQ-API-013/023 with the pacer
  contract (SPEC-PROV-001, SPEC-OBS-001 REQ-OBS-051). (audit M2)
- 2026-06-28 (v1.0.0): Initial greenfield API SPEC. Single coherent `/v1` (no v1/v2
  split — greenfield has no legacy `/v1` to preserve; research §5) with keyset
  pagination from day one. Management endpoints for tracked coins and markets
  (register/list/get/update/delete/search) and read endpoints for the three domains
  (spot quotes + latest, OHLCV candles by interval/time-range, coin metadata, coin
  market aggregates, derivatives). OpenAPI v3.1 published at `api/crypto-collector.yaml`
  as an implementation deliverable.

---

## Goal

Expose a coherent, versioned, keyset-paginated REST API over the collected data:
management endpoints to control which coins/markets are tracked, and read endpoints to
retrieve spot quotes, OHLCV candles, coin metadata, coin market aggregates, and
derivatives — fully described by an OpenAPI v3.1 document that stays in parity with the
handlers.

## Scope

In scope:
- A single `/v1` surface (servers entry `/v1`), keyset-paginated.
- **Management** (write): register/list/get/update/delete tracked coins; register/
  list/get/update/delete tracked markets; search coins and search markets.
- **Read** (the three domains): latest spot quote + recent quote history; OHLCV candles
  by interval and time range; coin metadata (latest + as-of); coin market aggregates
  (latest + time-series); derivatives (latest + time-series).
- Keyset cursor pagination contract (opaque base64url cursor, stable under appends).
- A uniform error model (400/404/409/422/500) and request validation.
- The requirement to publish and keep in parity an OpenAPI v3.1 document at
  `api/crypto-collector.yaml`.

Out of scope: see Exclusions. Health endpoints (SPEC-OBS-001), the collection that
populates the data (SPEC-SCHED-001/PROV-001), the schema (SPEC-DB-001).

## Decisions Restated (authoritative)

- **D1 — Single `/v1`** (no v1/v2 split). Greenfield; keyset pagination from day one.
  (research §5)
- **D2 — Keyset cursor pagination** for all list reads — opaque base64url JSON of the
  ordering-key tuple, O(1)-deep, stable under concurrent appends. (research §4.6;
  ticker `api/v2/cursor.rs`)
- **D3 — Two managed resources:** coins (coin-keyed) and markets (pair-keyed), matching
  the two registries. (SPEC-DB-001 D1)
- **D4 — Read paths reflect storage:** quotes/candles/derivatives keyed by market;
  metadata/market-aggregates keyed by coin. As-of reads for revisioned metadata.
- **D5 — OpenAPI v3.1 is a published deliverable** kept in parity with handlers via a
  doc-parity test (mirrors ticker `openapi_spec_contains_*`).
- **D6 — Idempotent registration:** registering an already-tracked coin/market returns
  the existing record (200) rather than erroring. (ticker register-ticker pattern)

---

## Design Summary (WHAT, not HOW)

### Management endpoints

Coins (coin-keyed registry):
- `GET /v1/coins` — list tracked coins (keyset-paginated).
- `POST /v1/coins` — register a coin for collection (idempotent: 201 new / 200 existing).
- `GET /v1/coins/{coin_id}` — registration + status for one coin.
- `PATCH /v1/coins/{coin_id}` — update mutable fields (e.g. `status`).
- `DELETE /v1/coins/{coin_id}` — deregister (stops collection; data retention per ops).
- `GET /v1/coins/search?q=` — search candidate coins to add (provider-backed, capped).

Markets (pair-keyed registry):
- `GET /v1/markets` — list tracked markets (keyset-paginated; filter by base/quote/venue).
- `POST /v1/markets` — register a `(base, quote, venue?)` market (idempotent).
- `GET /v1/markets/{id}` — one market's registration + status.
- `PATCH /v1/markets/{id}` — update mutable fields (e.g. `status`, per-market
  `live_poll_interval`).
- `DELETE /v1/markets/{id}` — deregister.
- `GET /v1/markets/search?q=` — search candidate pairs to add.

The two `search` endpoints are the only handlers that make an upstream provider call
inside an HTTP request. They acquire a SPEC-PROV-001 pacer slot with a bounded,
request-scoped wait and return 503/429 rather than blocking the request thread when the
slot, provider cooldown, or monthly credit budget is unavailable; these calls count
against the same per-provider credit budget as worker traffic — there is no separate
request-path egress path (REQ-API-080/081; SPEC-PROV-001 REQ-PROV-040/041/043/045;
SPEC-OBS-001 REQ-OBS-051).

### Read endpoints

Spot (market-keyed):
- `GET /v1/markets/{id}/quotes/latest` — newest `live_quotes` row for the market.
- `GET /v1/markets/{id}/quotes?start=&end=&cursor=&limit=` — quote history,
  keyset-paginated, time-range filtered.

Candles (market-keyed):
- `GET /v1/markets/{id}/candles?interval=&start=&end=&cursor=&limit=` — OHLCV by
  interval and time range, keyset-paginated. `interval` is required and validated
  against the supported set.

Coin metadata (coin-keyed, revisioned):
- `GET /v1/coins/{coin_id}/metadata` — latest metadata revision.
- `GET /v1/coins/{coin_id}/metadata?as_of=` — the revision in effect at `as_of`
  (greatest `first_seen_at <= as_of`).

Coin market aggregates (coin-keyed, time-series):
- `GET /v1/coins/{coin_id}/market/latest?vs_currency=` — latest market cap / FDV /
  supply snapshot.
- `GET /v1/coins/{coin_id}/market?vs_currency=&start=&end=&cursor=&limit=` — snapshot
  history, keyset-paginated.

Derivatives (market-keyed, time-series):
- `GET /v1/markets/{id}/derivatives/latest` — newest funding/OI/mark/index/basis tick.
- `GET /v1/markets/{id}/derivatives?start=&end=&cursor=&limit=` — derivatives history,
  keyset-paginated.

### Pagination contract

- A `limit` query param (validated, capped at a maximum) and an opaque `cursor`.
- The cursor encodes the ordering-key tuple of the last returned row (e.g.
  `(ts, market_id)`) as base64url-no-pad JSON; an invalid cursor → 400.
- The response carries the page items plus a `next_cursor` (null when exhausted).

### Error model

- `400` invalid query/body (bad cursor, bad interval, malformed range).
- `404` unknown coin/market id.
- `409`/`200` idempotent-registration conflict handled as "return existing" (200).
- `422` semantically invalid registration (e.g. unknown base asset).
- `500` internal error (uniform body).
All responses are JSON; numeric values serialise from `Decimal` as JSON strings or
numbers per a documented, lossless convention (decided at run, OR-API-2).

### OpenAPI v3.1

- The handlers' surface is published as `api/crypto-collector.yaml` (OpenAPI 3.1.0,
  `servers: [{ url: /v1 }]`), with schemas for every request/response and the shared
  pagination/error components.

---

## Requirements (EARS)

### Versioning and document

- **REQ-API-001** (Ubiquitous): The system shall expose a single `/v1` API surface and
  shall not introduce a parallel `/v2` surface in the foundation (greenfield — no legacy
  contract to preserve).
- **REQ-API-002** (Ubiquitous): The system shall publish an OpenAPI v3.1 document at
  `api/crypto-collector.yaml` describing every endpoint, request/response schema, the
  pagination components, and the error model.
- **REQ-API-003** (Ubiquitous): The published OpenAPI document shall remain in parity
  with the implemented handlers, verified by a doc-parity test that fails when an
  endpoint or schema is added/changed without updating the document.

### Management — coins

- **REQ-API-010** (Event-Driven): When a client POSTs a new coin registration, the
  system shall create the `tracked_coins` record, enqueue its initial collection
  (SPEC-SCHED-001), and respond 201 with the record.
- **REQ-API-011** (Event-Driven): When a client POSTs a coin that is already tracked,
  the system shall return the existing record with 200 (idempotent), not an error.
- **REQ-API-012** (Ubiquitous): The system shall provide `GET /v1/coins` (keyset-
  paginated list), `GET /v1/coins/{coin_id}`, `PATCH /v1/coins/{coin_id}` (mutable
  fields), and `DELETE /v1/coins/{coin_id}`.
- **REQ-API-013** (Event-Driven): When a client requests `GET /v1/coins/search?q=`, the
  system shall return candidate coins matching the query, capped at a documented maximum
  (provider-backed; subject to request-path pacing per REQ-API-080/081).

### Management — markets

- **REQ-API-020** (Event-Driven): When a client POSTs a new market `(base, quote,
  venue?)`, the system shall create the `tracked_markets` record (respecting the
  `(base,quote,COALESCE(venue,''))` uniqueness), enqueue its initial collection and
  backfill (SPEC-SCHED-001), and respond 201 with the record.
- **REQ-API-021** (Event-Driven): When a client POSTs a market that is already tracked,
  the system shall return the existing record with 200 (idempotent).
- **REQ-API-022** (Ubiquitous): The system shall provide `GET /v1/markets` (keyset-
  paginated, filterable by base/quote/venue), `GET /v1/markets/{id}`,
  `PATCH /v1/markets/{id}`, and `DELETE /v1/markets/{id}`.
- **REQ-API-023** (Event-Driven): When a client requests `GET /v1/markets/search?q=`,
  the system shall return candidate pairs matching the query, capped at a documented
  maximum (provider-backed; subject to request-path pacing per REQ-API-080/081).

### Read — spot

- **REQ-API-030** (Event-Driven): When a client requests `GET /v1/markets/{id}/quotes/
  latest`, the system shall return the newest `live_quotes` row for that market, or 404
  if the market is unknown.
- **REQ-API-031** (Event-Driven): When a client requests `GET /v1/markets/{id}/quotes`
  with optional `start`/`end`/`cursor`/`limit`, the system shall return a keyset-
  paginated, time-range-filtered page of quote history with a `next_cursor`.

### Read — candles

- **REQ-API-040** (Event-Driven): When a client requests `GET /v1/markets/{id}/candles`
  with `interval` (required) and optional `start`/`end`/`cursor`/`limit`, the system
  shall return a keyset-paginated page of OHLCV candles for that interval and range.
- **REQ-API-041** (If/Unwanted): If `interval` is absent or not in the supported set,
  then the system shall respond 400 without querying.
- **REQ-API-042** (Ubiquitous): The candle response shall represent `volume` as
  nullable, so candles sourced from a volume-less provider (CoinGecko OHLC) are
  distinguishable from zero-volume candles (SPEC-PROV-001 REQ-PROV-031).

### Read — coin metadata and market aggregates

- **REQ-API-050** (Event-Driven): When a client requests `GET /v1/coins/{coin_id}/
  metadata` without `as_of`, the system shall return the latest metadata revision; with
  `as_of`, it shall return the revision with the greatest `first_seen_at <= as_of`.
- **REQ-API-051** (Event-Driven): When a client requests `GET /v1/coins/{coin_id}/
  market/latest` (with `vs_currency`), the system shall return the newest
  `coin_market_snapshots` row (market cap, FDV, circulating/total supply, price).
- **REQ-API-052** (Event-Driven): When a client requests `GET /v1/coins/{coin_id}/
  market` with a range, the system shall return a keyset-paginated page of market
  snapshots.

### Read — derivatives

- **REQ-API-060** (Event-Driven): When a client requests `GET /v1/markets/{id}/
  derivatives/latest`, the system shall return the newest `derivatives_quotes` tick
  (funding rate, open interest, mark/index, basis).
- **REQ-API-061** (Event-Driven): When a client requests `GET /v1/markets/{id}/
  derivatives` with a range, the system shall return a keyset-paginated page of
  derivatives history.

### Pagination, validation, errors, precision

- **REQ-API-070** (Ubiquitous): Every list read shall use an opaque base64url-no-pad
  cursor encoding the ordering-key tuple of the last returned row, and shall return a
  `next_cursor` that is null when the result is exhausted.
- **REQ-API-071** (If/Unwanted): If a supplied `cursor` cannot be decoded into the
  endpoint's keyset key, then the system shall respond 400.
- **REQ-API-072** (Ubiquitous): Every list read shall accept a `limit` validated
  against a documented maximum, rejecting out-of-range values with 400.
- **REQ-API-073** (Ubiquitous): The system shall serialise every monetary/quantity
  value losslessly from `Decimal` (no `f64` round-trip) per a documented JSON
  convention.
- **REQ-API-074** (Event-Driven): When a requested coin/market id does not exist, the
  system shall respond 404 with the uniform error body; when a request is malformed, it
  shall respond 400; when a registration is semantically invalid, it shall respond 422.

### Search endpoints — request-path provider pacing

- **REQ-API-080** (Event-Driven): When a `GET /v1/coins/search` or
  `GET /v1/markets/search` request requires an upstream provider call, the handler shall
  acquire a SPEC-PROV-001 pacer slot for that provider with a bounded, request-scoped
  wait, and that call shall count against the same per-provider credit budget as worker
  traffic (SPEC-PROV-001 REQ-PROV-040/045) — there is no separate request-path egress
  path.
- **REQ-API-081** (If/Unwanted): If the pacer slot cannot be acquired within the bounded
  wait, or the provider is in cooldown (SPEC-PROV-001 REQ-PROV-041), or the provider has
  exhausted its monthly credit budget (SPEC-PROV-001 REQ-PROV-043), then the search
  handler shall respond 503 (Service Unavailable) — or 429 when the limit is surfaced as
  a client-visible rate limit — and shall not block the request thread and shall not
  issue an unpaced upstream call (SPEC-OBS-001 REQ-OBS-051).

## Exclusions (What NOT to Build)

- **No `/v2` surface** and **no OFFSET pagination** — single `/v1`, keyset only
  (REQ-API-001/070; research §5).
- **No health endpoints here** — `/healthz/live` and `/healthz/ready` are SPEC-OBS-001
  (served on a separate port).
- **No WebSocket / streaming / SSE** endpoints in the foundation.
- **No authentication/authorization layer** in the foundation (internal service; matches
  ticker-collector's unauthenticated read/management surface). Auth is future work.
- **No bulk mutation, no admin "force collect now"** endpoints in the foundation
  (collection is worker-driven).
- **No separate request-path egress path** — the search endpoints' provider calls go
  through the same SPEC-PROV-001 pacer and per-provider credit budget as workers, with a
  bounded wait and 503/429 on unavailability; no handler issues an unpaced or unbounded
  blocking upstream call (REQ-API-080/081).
- **No `f64` serialization** of monetary values (REQ-API-073).
- **No on-chain/sentiment/DEX-depth read endpoints** — out of product scope.

## @MX Annotation Targets (high fan_in)

- The keyset cursor encode/decode helpers — `@MX:ANCHOR` (every list endpoint depends
  on the opaque, stable-under-appends contract) + `@MX:WARN`/`@MX:REASON`: keyset, not
  OFFSET, for stability over partitioned append-heavy tables (REQ-API-070/071).
- The shared error-response mapper — `@MX:ANCHOR` (uniform 400/404/422/500 body).
- The `Decimal`→JSON serialization convention — `@MX:NOTE` documenting the lossless
  rule (REQ-API-073).
- The OpenAPI doc-parity test — `@MX:NOTE` that handler changes require document updates
  (REQ-API-003).

## Open Items (do not guess)

- **OR-API-1:** the supported candle `interval` set surfaced by the API (e.g.
  `1m,5m,15m,1h,4h,1d,1w`), bounded by what providers/tier can supply (research §2.2).
  The validation rule is normative (REQ-API-041); the exact set is confirmed at run.
- **OR-API-2:** `Decimal`→JSON representation — JSON number vs JSON string. Recommend
  string for guaranteed losslessness across clients; confirm at run (REQ-API-073).
- **OR-API-3:** default and maximum `limit` per endpoint. Rule normative
  (REQ-API-072); numbers at run.
- **OR-API-4:** `DELETE` semantics — soft-deregister (stop collection, keep data) vs
  cascade data delete. Recommend soft-deregister; data retention is OR-DEPLOY-1.
