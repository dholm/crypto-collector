---
id: SPEC-API-004
title: "All-Coin Latest-Quote Overview"
version: "0.1.0"
status: in-progress
created: 2026-07-21
updated: 2026-07-21
author: dholm
priority: Medium
phase: "api-v1"
module: "src/api"
lifecycle: spec-anchored
tags: "api, quotes, overview, partition-pruning"
issue_number: 0
tier: S
---

# SPEC-API-004 — All-Coin Latest-Quote Overview (spot price + 24h baseline)

Brownfield evolution of the coin-keyed spot-quote reads defined in
[SPEC-API-002](../SPEC-API-002/spec.md) Module 3 (`GET /v1/coins/{coin_id}/quotes/latest` +
`GET /v1/coins/{coin_id}/quotes`, handlers `get_latest_quote` / `list_quotes` in
`src/api/quotes.rs`). SPEC-API-002 serves one coin per request; a consumer building a
full-portfolio dashboard (Observatory BFF) must currently issue **two fetches per coin** — one
for the current price and one for a 24h-ago baseline. This SPEC adds a single **all-coin
overview** endpoint that returns, for every active tracked coin, the current spot price AND a
24h-ago baseline in one request, replacing the N×2 fan-out.

This follows the [SPEC-API-003](../SPEC-API-003/spec.md) precedent (a new draft SPEC that
evolves a completed parent's endpoint family without modifying it). SPEC-API-002 is `completed`
and stays immutable — it is referenced, not changed.

Schema/data contract base: [SPEC-DB-001](../SPEC-DB-001/spec.md) (`coin_quotes`,
`tracked_coins`, `migrations/0011_remove_markets.sql:36-52`, `migrations/0001_registries.sql:23-31`).
Keyset/DecimalString/Decimal-not-`f64` conventions carry over unchanged from SPEC-API-001/002 and
REQ-PROV-012.

## HISTORY

- 2026-07-21 (v0.1.0): Initial draft. Single endpoint: `GET /v1/coins/quotes/latest` — one
  request returning, for every active tracked coin, the current spot price and a 24h-ago baseline
  (`open_24h`, nullable). Bare `{"quotes":[...]}` envelope (deliberate departure from the standard
  `Page{items,next_cursor}` wrapper). Every read of `coin_quotes` is `ts`-bounded so PostgreSQL
  partition pruning applies — the crux, made an acceptance criterion (EXPLAIN proof deferred to
  run). No migration (reuses `coin_quotes` + `tracked_coins`). New `REQ-API-3NN` range;
  SPEC-API-001 retains `REQ-API-0NN`, SPEC-API-002 `REQ-API-1NN`, SPEC-API-003 `REQ-API-2NN`.

---

## Goal

A dashboard consumer that tracks every registered coin can obtain, in **one** HTTP request, each
active coin's current spot price together with a 24h-ago baseline price for computing 24h change —
without the previous two-fetches-per-coin fan-out. The response is a single unpaginated snapshot
(there is one row per active coin, a small bounded set). Correctness of the change baseline is
explicit: `open_24h` is `null`, never zero, when no 24h-ago quote exists, so the consumer can
render "no change available" rather than a fabricated 0. The endpoint is fast at portfolio scale
because every `coin_quotes` read is bounded by `ts` and therefore prunes partitions — this is a
requirement, not a hope.

## Scope

In scope:
- A new read endpoint **`GET /v1/coins/quotes/latest?vs_currency=`** returning one overview row
  per **active** tracked coin: `coin_id`, `vs_currency`, `ts`, `price` (current spot), `open_24h`
  (24h-ago baseline, nullable), `source`.
- A **bare `{"quotes":[...]}` response envelope** — NOT the standard `Page{items,next_cursor}`
  wrapper — a single unpaginated all-coin snapshot with no cursor (deliberate departure, D1).
- **Partition-pruning discipline**: every read of `coin_quotes` this endpoint performs is bounded
  by `ts` (a lower bound relative to `now()`) so PostgreSQL prunes partitions; an unbounded
  parent-wide scan is forbidden (D7, the crux — REQ-API-305).
- **Active-coin filter + absent-on-stale semantics**: only `tracked_coins.status = 'active'` coins
  are considered; a coin with no quote inside the bounded current-price window is absent from the
  response (no placeholder row).
- **`vs_currency`** optional query parameter defaulting to `usd` (matching the existing
  `get_latest_quote` / coin-market convention).
- **OpenAPI v3.1 parity**: document the new operation under `tags: [quotes]` and extend the
  existing doc-parity test with its operationId.

Out of scope: see Exclusions. This SPEC does not modify SPEC-API-002's per-coin quote endpoints,
does not add a DB migration, does not change collection or the provider chain, and does not touch
candles, cycle overlays, or WebSocket streams.

## Decisions Restated (authoritative)

Encoded here verbatim in intent; the response field names are already agreed with the consumer
and MUST NOT be renamed.

- **D1 — Single unpaginated all-coin snapshot, bare `{"quotes":[...]}` envelope.** This is a
  deliberate departure from the `Page{items,next_cursor}` wrapper used by every other list read in
  this API. There is no cursor and no `limit`: the result set is bounded by the number of active
  tracked coins (small), so pagination adds no value. The envelope is a bare object with a single
  `quotes` array. (REQ-API-301)
- **D2 — Fixed response contract (field names agreed with consumer, do NOT rename).** Each row is
  exactly `{coin_id, vs_currency, ts, price, open_24h, source}`. `ts`/`price`/`source` describe the
  current spot quote; `open_24h` is the baseline price ~24h earlier and carries no timestamp of its
  own. (REQ-API-302)
- **D3 — `open_24h` is nullable and never zero-filled or omitted.** `open_24h` is `null` when the
  trailing 24h window holds no earlier quote for the coin (e.g. a newly-tracked coin). The consumer
  renders a price with no change indicator on `null`. Substituting `0` (a false price) or omitting
  the field is prohibited. (REQ-API-303)
- **D4 — Baseline = earliest quote in the trailing 24h window.** `open_24h` is the `price` of the
  oldest `coin_quotes` row with `ts >= now() - interval '24 hours'` (`ORDER BY ts ASC LIMIT 1`).
  For a continuously-tracked coin this is ~24h old; for a coin tracked less than 24h it is the
  oldest available (younger than 24h). The consumer accepts this approximation; the nuance is
  recorded as OR-API4-2. (REQ-API-303)
- **D5 — 48h outer bound on the current price; absent-on-stale.** The current-price read is bounded
  by `ts >= now() - interval '48 hours'` — the wider 48h bound prunes partitions while tolerating a
  slightly stale coin (one that missed its most recent poll). A coin with **no** quote in the 48h
  window is dropped entirely from the response (the `CROSS JOIN LATERAL` yields no row); the
  consumer shows a dash. This is intended. (REQ-API-305/306)
- **D6 — Active coins only.** Only `tracked_coins.status = 'active'` rows are considered; `paused`
  and `error` coins are excluded. (REQ-API-306)
- **D7 — Every `coin_quotes` read is `ts`-bounded (partition pruning is a hard requirement).**
  `coin_quotes` is `PARTITION BY RANGE(ts)` with 48 monthly partitions (2024-01→2027-12). A
  `SELECT DISTINCT ON (coin_id) ... ORDER BY coin_id, ts DESC` over the **parent** table has no
  `ts` bound and touches **every** partition; the `(coin_id, vs_currency, ts DESC)` btree does not
  rescue it — only partition pruning does. A sibling service shipped exactly that shape and
  produced a **41s** query that blew a 30s client timeout. This SPEC forbids that shape: every
  `coin_quotes` read here carries a `ts` lower bound. (REQ-API-305; proven by an EXPLAIN acceptance
  criterion, deferred to run)
- **D8 — `vs_currency` default `usd`, no allow-list.** Optional `vs_currency` parameter resolves to
  `usd` when omitted (`.unwrap_or("usd")` convention; no `DEFAULT_VS_CURRENCY` constant). Like the
  existing quote reads, `vs_currency` is not validated against an allow-list — an unrecognised value
  simply matches no rows. (REQ-API-307)

---

## Change Surface (brownfield delta markers)

File-level scope only; exact function bodies / query text are deferred to `plan.md` / Run.

[MODIFY]
- `src/api/dto.rs` — add `CoinQuoteOverviewDto` and an envelope type
  `CoinQuoteOverviewPage { quotes: Vec<CoinQuoteOverviewDto> }`, beside `CoinQuoteDto`
  (`src/api/dto.rs:133-158`). `price` uses `#[serde(with = "rust_decimal::serde::str")]`;
  `open_24h: Option<Decimal>` uses `#[serde(with = "rust_decimal::serde::str_option")]`.
- `src/api/quotes.rs` — add a `list_latest_quotes` handler beside `get_latest_quote`
  (`src/api/quotes.rs:34-57`).
- `src/api/mod.rs` — register `/v1/coins/quotes/latest` in `build_api_router`, **before** the
  parameterised `/v1/coins/{coin_id}` routes (`src/api/mod.rs:160-219`), and add the new
  operationId to the `openapi_yaml_contains_all_operation_ids` doc-parity test
  (`src/api/mod.rs:395-425`).
- `api/crypto-collector.yaml` — document the new operation under `tags: [quotes]` with a stable
  operationId (proposed `listLatestCoinQuotes`) and a `CoinQuoteOverview` (+ `CoinQuoteOverviewPage`)
  schema.

[NEW]
- The bounded all-coin overview query (over `tracked_coins` + `coin_quotes`, every `coin_quotes`
  access `ts`-bounded). Placement — inline in `list_latest_quotes` vs a new `src/db/` read function
  — is a Run-phase decision (OR-API4-1); there is no dedicated quote-read function in `src/db/`
  today.

[UNCHANGED]
- `coin_quotes` / `tracked_coins` schema — **no migration** (aggregation-free, read-only over
  existing rows). `CoinQuoteDto`, `get_latest_quote`, `list_quotes`, the `TsKey`/`paginate_ts`
  cursor helpers, and every other route are untouched. SPEC-API-002 is not modified.

---

## Design Summary (WHAT, not HOW)

### Endpoint

`GET /v1/coins/quotes/latest?vs_currency=` — no path parameter, no cursor, no `limit`. The response
body is:

```json
{"quotes":[{"coin_id":"bitcoin","vs_currency":"usd","ts":"...","price":"67123.45","open_24h":"65000.00","source":"binance"}]}
```

`open_24h` may be `null` for any row. `price` is always a non-null JSON string. The `quotes` array
holds one entry per active tracked coin that has a current quote in the bounded window; it is `[]`
when the registry is empty or no active coin has a recent quote.

### Bounded overview query (proposal — verify by measurement, not gospel)

The consumer proposed the following shape, which this SPEC treats as the recommended starting
point. The **binding requirement is partition pruning** (REQ-API-305), not this exact SQL — if
`EXPLAIN (ANALYZE, BUFFERS)` against a real DB points to a better bounded shape, adopt it and record
why in `plan.md` (OR-API4-3).

```sql
SELECT c.coin_id, q.vs_currency, q.ts, q.price, q.source, b.price AS open_24h
FROM tracked_coins c
CROSS JOIN LATERAL (
    SELECT vs_currency, ts, price, source FROM coin_quotes
    WHERE coin_id = c.coin_id AND vs_currency = $1
      AND ts >= now() - interval '48 hours'
    ORDER BY ts DESC LIMIT 1
) q
LEFT JOIN LATERAL (
    SELECT price FROM coin_quotes
    WHERE coin_id = c.coin_id AND vs_currency = $1
      AND ts >= now() - interval '24 hours'
    ORDER BY ts ASC LIMIT 1
) b ON TRUE
WHERE c.status = 'active'
```

Why this shape prunes: each `LATERAL` subquery carries a `ts >= now() - interval '...'` lower
bound, so PostgreSQL can prune the monthly partitions outside that window. Because `now()` is a
`STABLE` (not constant) expression, pruning happens at **execution time** (PG11+), which
`EXPLAIN` reports as pruned partitions / "Subplans Removed" under the `Append` node — NOT a
parent-wide sequential scan across all 48 partitions (OR-API4-4). The current-price `LATERAL` `q`
(`ORDER BY ts DESC LIMIT 1`) is a forward scan of the `(coin_id, vs_currency, ts DESC)` btree; the
baseline `LATERAL` `b` (`ORDER BY ts ASC LIMIT 1`) is a backward scan of the same btree, both
`LIMIT 1` (OR-API4-3). The `CROSS JOIN LATERAL` on `q` drops any coin with no quote in the 48h
window (absent-on-stale, D5); the `LEFT JOIN LATERAL` on `b` yields `NULL` (→ `open_24h: null`)
when the 24h window holds no quote (D3).

### Nullability and serialisation

`price` maps to a non-null `Decimal` → JSON string; `open_24h` maps to `Option<Decimal>` → JSON
string or `null`, via `CoinQuoteOverviewDto`'s `rust_decimal::serde::str` / `str_option`. No `f64`
appears at any point (REQ-PROV-012, REQ-API-152).

### Routing

For `/v1/coins/quotes/latest` the path segments are `v1` (1), `coins` (2), `quotes` (3),
`latest` (4). The collision is at segment 3: this route's literal `quotes` occupies the same
position as the `{coin_id}` parameter in `/v1/coins/{coin_id}` and `/v1/coins/{coin_id}/quotes`
(where `{coin_id}` is segment 3). It MUST therefore be registered **before** the parameterised
`/v1/coins/{coin_id}` routes in `build_api_router` so Axum's literal-first matching lets the
`quotes` literal win over binding `{coin_id} = "quotes"` at that position. This mirrors the
existing `/v1/coins/search` and `/v1/coins/stream/*` placement (REQ-API-148,
`src/api/mod.rs:167-185`).

---

## Requirements (GEARS)

### Endpoint behaviour

- **REQ-API-300** (Event-driven): When a client requests `GET /v1/coins/quotes/latest` with an
  optional `vs_currency` (default `usd`), the system shall return, for every active tracked coin
  that has a current quote in the bounded window, one overview row carrying the coin's current spot
  price and a 24h-ago baseline price, in a single response.

### Response envelope and schema

- **REQ-API-301** (Ubiquitous): The system shall return the overview as a bare `{"quotes": [...]}`
  JSON object — NOT the standard `Page` `{items, next_cursor}` wrapper — with no cursor and no
  pagination; the response is a single unpaginated all-coin snapshot, and an empty result is
  `{"quotes": []}`.
- **REQ-API-302** (Ubiquitous): Each overview row shall carry exactly the fields `coin_id`,
  `vs_currency`, `ts`, `price`, `open_24h`, `source`, where `ts`/`price`/`source` describe the
  current spot quote and `open_24h` is the baseline price approximately 24h earlier.
- **REQ-API-303** (Unwanted): When the trailing 24h window holds no earlier quote for a coin
  (e.g. a newly-tracked coin), the system shall set that row's `open_24h` to JSON `null`; the
  system shall not substitute zero and shall not omit the field.
- **REQ-API-304** (Ubiquitous): The system shall serialise `price` as a non-null JSON string and
  `open_24h` as either a JSON string or `null`, computed and carried in `rust_decimal::Decimal`
  end to end with no `f64` conversion (REQ-PROV-012, REQ-API-073/152).

### Partition-pruning discipline (the crux)

- **REQ-API-305** (Ubiquitous): Every read of `coin_quotes` performed by this endpoint shall be
  bounded by `ts` (a lower bound relative to `now()`) so PostgreSQL partition pruning applies; the
  system shall not issue any `coin_quotes` read that lacks a `ts` bound and therefore scans every
  monthly partition of the parent table.

### Active-coin filter and absent-on-stale semantics

- **REQ-API-306** (State-driven): While a tracked coin's `status` is `active`, the system shall
  include it in the overview if and only if it has at least one `coin_quotes` row within the
  bounded current-price window; a coin with no quote in that window shall be absent from the
  response (no row, no placeholder), and non-active coins (`paused`/`error`) shall be excluded.

### vs_currency default

- **REQ-API-307** (Event-driven): When the request supplies an optional `vs_currency` query
  parameter, the system shall resolve the effective currency to that value and shall default it to
  `usd` when the parameter is omitted (the `.unwrap_or("usd")` codebase convention); `vs_currency`
  shall not be validated against an allow-list — an unrecognised value simply matches no rows and
  yields `{"quotes": []}`.

### Route registration

- **REQ-API-308** (Ubiquitous): The system shall register the literal route
  `/v1/coins/quotes/latest` before the parameterised `/v1/coins/{coin_id}` routes in
  `build_api_router` so Axum's literal-first matching resolves it as an endpoint and not as a
  `{coin_id}` of `quotes` (the REQ-API-148 literal-before-param rule).

### OpenAPI parity

- **REQ-API-309** (Ubiquitous): The system shall document the new operation in
  `api/crypto-collector.yaml` under `tags: [quotes]` with a stable operationId (proposed
  `listLatestCoinQuotes`), and shall add that operationId to the
  `openapi_yaml_contains_all_operation_ids` doc-parity test so the document stays in parity with
  the handler (REQ-API-003/153).

## Exclusions (What NOT to Build)

The following are explicitly **out of scope** for SPEC-API-004. Reports/analysis and other endpoint
families are routed elsewhere; this endpoint stays a single focused read.

### Out of Scope — SPEC-API-002 modification
- SPEC-API-002 is `completed` and immutable. The per-coin `GET /v1/coins/{coin_id}/quotes/latest`
  and `GET /v1/coins/{coin_id}/quotes` endpoints and their handlers are unchanged; this SPEC only
  adds a new sibling endpoint and references the parent.

### Out of Scope — schema and migration
- No DB migration. This endpoint reads existing `coin_quotes` and `tracked_coins` rows; it adds no
  column, table, index, or partition, and writes nothing.

### Out of Scope — pagination and envelope generalisation
- No cursor, no `limit`, no `Page{items,next_cursor}` wrapper for this endpoint. The bare
  `{"quotes":[...]}` envelope is intentional and is not generalised into the shared `Page` type;
  the other list reads keep their `Page` wrapper unchanged.

### Out of Scope — currency conversion and FX
- No currency conversion between `vs_currency` values, no FX, and no new supported currencies. The
  endpoint filters to a single resolved `vs_currency` (default `usd`) and returns only rows stored
  in that currency.

### Out of Scope — baseline enrichment
- No baseline timestamp, no percent-change computation, and no configurable baseline window in the
  response. `open_24h` is a bare price; the consumer computes any change client-side. The 24h/48h
  windows are fixed, not request-parameterised.

### Out of Scope — other endpoint families
- No changes to candle reads (`/candles`), cycle overlays (`/cycle-projection*`), coin metadata,
  coin market aggregates, or WebSocket streams. Only the new `/v1/coins/quotes/latest` route is
  added.

### Out of Scope — collection and provider chain
- No changes to the live poller, collection queue, backfill, pacer, or provider chain
  (SPEC-SCHED-001 / SPEC-PROV-001). This endpoint only reads what those already persist.

### Out of Scope — pre-existing comment drift (note only, do not fix here)
- `src/api/quotes.rs:1` mis-cites `SPEC-API-002 REQ-API-131/132`; Module 3 quote REQs are
  `REQ-API-120..123` (131/132 are candle REQs, themselves mis-labelled `141/142` in
  `src/api/dto.rs:160`). Recorded as OR-API4-5 for a future cleanup SPEC; not corrected here.

## @MX Annotation Targets (high fan_in)

- The bounded overview query (LATERAL joins over `coin_quotes` + `tracked_coins`) — `@MX:ANCHOR`
  (the correctness + performance core; the whole endpoint depends on it) + `@MX:WARN`/`@MX:REASON`:
  every `coin_quotes` access MUST carry a `ts` lower bound so partition pruning applies; an
  unbounded `DISTINCT ON` over the parent touches all 48 monthly partitions — a sibling service
  shipped that shape and produced a 41s query that blew a 30s client timeout (REQ-API-305).
- The `list_latest_quotes` handler's `vs_currency` default + active-coin filter — `@MX:NOTE`:
  default `usd` via `.unwrap_or("usd")`; only `status='active'` coins; absent-on-stale drops coins
  with no quote in the 48h window (REQ-API-306/307).
- The `build_api_router` registration line for `/v1/coins/quotes/latest` — `@MX:NOTE`: literal
  route MUST precede `/v1/coins/{coin_id}` (literal-before-param, REQ-API-148/308).

## Open Items (do not guess)

- **OR-API4-1 — Query placement.** Whether the bounded overview query lives inline in
  `list_latest_quotes` or in a new `src/db/` read function. There is no dedicated quote-read
  function in `src/db/` today (quotes are inline in `src/api/quotes.rs`, mirroring OR-API3-4). A
  Run-phase decision.
- **OR-API4-2 — Baseline definition (earliest-in-window vs closest-to-24h-ago).** The consumer's
  SQL picks the earliest quote in `[now-24h, now]` (`ORDER BY ts ASC LIMIT 1`). For a coin tracked
  less than 24h this baseline is younger than 24h, and the fixed contract carries no baseline `ts`
  so the consumer cannot see its age. Confirm at run that earliest-in-window is the intended
  baseline (vs. the quote nearest to exactly `now() - 24h`).
- **OR-API4-3 — Query shape is a PROPOSAL to verify by measurement.** The `CROSS JOIN LATERAL` +
  `LEFT JOIN LATERAL` is the recommended shape, but the binding requirement is partition pruning,
  not this exact SQL. If `EXPLAIN (ANALYZE, BUFFERS)` on a populated DB points to a faster bounded
  shape, adopt it and record why in `plan.md`. Confirm the baseline `ORDER BY ts ASC LIMIT 1` is a
  backward index scan on `(coin_id, vs_currency, ts DESC)`, not a sort.
- **OR-API4-4 — Runtime vs plan-time pruning with `now()`.** `now()` is `STABLE`, so pruning on
  `ts >= now() - interval '48 hours'` is execution-time pruning (PG11+), reported by `EXPLAIN` as
  pruned partitions / "Subplans Removed" under the `Append` node. The AC (Scenario 10) requires the
  `EXPLAIN` to demonstrate pruning + index scans, not a parent-wide seq scan. Verify against a
  populated DB at run.
- **OR-API4-5 — Pre-existing header-comment drift (minor; do not fix here).** `src/api/quotes.rs:1`
  cites `REQ-API-131/132` but Module 3 quote REQs are `REQ-API-120..123`. Noted for a future
  cleanup SPEC; out of scope for SPEC-API-004.

---

## Post-Implementation Notes (not acceptance criteria)

- Repo convention: commit directly to `main` (no feature branch).
- The endpoint needs `cargo build` + `make deploy` (namespace `finance`) before the Observatory
  BFF can call it. This is a deploy step, recorded here as a note, not an acceptance criterion.
