---
id: SPEC-API-005
title: "API Contract Fixes, Query Bounds & Schema Truth"
version: "0.1.0"
status: in-progress
created: 2026-07-27
updated: 2026-07-27
author: dholm
priority: High
phase: "api-v1"
module: "src/api"
lifecycle: spec-anchored
tags: "api, quotes, partition-pruning, error-handling, idempotency, schema-truth"
issue_number: 0
tier: M
---

# SPEC-API-005 — API Contract Fixes, Query Bounds & Schema Truth

Phase 5 of the crypto-collector review-driven improvement roadmap
(`research/idiomatic-rust.md` §7, Phase 5). Prior phases: SPEC-SCHED-002, SPEC-CANDLE-002,
SPEC-PROV-003 (all complete). This SPEC closes the **HTTP API boundary** findings — the
contract gaps between the handlers in `src/api/` and `api/crypto-collector.yaml`, the missing
query bounds that violate the codebase's own partition-pruning invariant, and the stale schema
documentation/tests left behind by migration `0011_remove_markets.sql` and
`0020_coin_candles_departition.sql`.

Findings addressed (Categories E & H of `research/idiomatic-rust.md` §6): **F-29, F-30, F-31,
F-32, F-33, F-34, F-35, F-36, F-57, F-58**, plus the per-operation parameter-parity test from
**F-59**. This is a brownfield, behavioral-defect + hygiene SPEC over the existing `/v1` router;
it adds no new endpoint and no migration.

Schema/data-contract base: [SPEC-DB-001](../SPEC-DB-001/spec.md) (`coin_quotes` still
`PARTITION BY RANGE(ts)`; `coin_candles` de-partitioned by `migrations/0020`). Prior API SPECs:
[SPEC-API-001](../SPEC-API-001/spec.md) (REQ-API-0NN, router + `ApiError` + OpenAPI parity),
[SPEC-API-002](../SPEC-API-002/spec.md) (REQ-API-1NN, coin quote/candle reads),
[SPEC-API-003](../SPEC-API-003/spec.md) (REQ-API-2NN, candle aggregation fallback),
[SPEC-API-004](../SPEC-API-004/spec.md) (REQ-API-3NN, all-coin overview — the 48h/24h LATERAL
partition-pruning precedent). This SPEC allocates **REQ-API-4NN** and evolves those endpoints
in place without adding routes; the completed parents stay immutable and are referenced.

## HISTORY

- 2026-07-27 (v0.1.0): Initial draft. Closes the API-boundary findings F-29..F-36 + F-57/F-58 +
  the F-59 parameter-parity test. New `REQ-API-4NN` range (REQ-API-400..417). No new endpoint, no
  migration. Ten LOCKED decisions restated from the roadmap prompt (D1..D10): 48h-bounded quote
  reads with 404-on-stale (F-30), `#[derive(FromRequest)]` extractor wrappers — NOT `WithRejection`
  (F-33), search pacer errors → 503 (F-34), `tokio::sync::Semaphore` ceiling on the `as_of`
  recompute (F-35), `vs_currency` on both quote reads (F-29), aggregation `end`-bound +
  cap-cursor-from-source (F-31), idempotent `ON CONFLICT` registration in one transaction (F-32),
  bidirectional WebSocket read loop (F-36), CoinCandle flat-table anchor + stale-test cleanup +
  migration-presence rename (F-57/F-58), per-operation parameter-parity test (F-59). No new
  dependencies; Decimal-only; keyset cursors stay opaque/decode-compatible.

---

## Goal

The `/v1` HTTP API is truthful to its own OpenAPI contract, its own partition-pruning invariant,
and its own migrated schema. Concretely, after this phase:

- Every documented query parameter in `api/crypto-collector.yaml` is honored by its handler
  (enforced by a new per-operation parameter-parity test — the guard that would have caught F-29).
- No `coin_quotes` read anywhere in `src/api/` lacks a `ts` bound — the "every `coin_quotes`
  read is ts-bounded" invariant declared in `quotes.rs` holds across the whole module with no
  exemption carve-out, so PostgreSQL partition pruning always applies.
- Malformed request bodies, query strings, and path parameters return the documented
  `{code, message}` JSON error body instead of Axum's default `text/plain` rejection.
- Concurrent duplicate coin registrations are idempotent (200, never a 500 PK violation), and a
  registration's insert + initial enqueues are atomic.
- The candle aggregation path can reach far-past windows and does not terminate pagination early
  on a cap-hit-but-gap-dropped page.
- `search_coins` distinguishes "provider down" (503) from "no matches" (200 empty).
- The `as_of` recompute path carries a concurrency ceiling.
- The WebSocket handler reacts to client Close frames.
- `CoinCandle`'s `@MX:ANCHOR`, the pre-0011 `tests/db_integration.rs` scenarios, and the stale
  migration-presence test all reflect the current schema, so `tests/db_integration.rs` passes in
  full against a fresh migrated database and no permanently-red test trains people to ignore red.

Externally visible behavior changes are limited to the intended spec-conformance fixes.
`api/crypto-collector.yaml` remains the contract of record: the implementation converges to it
(F-29 `vs_currency`, F-34 503) rather than the reverse.

## Scope

In scope (all within `src/api/`, `src/models/quote.rs`, `tests/`, and `api/crypto-collector.yaml`):

- **Quote-read contract & bounds** (F-29, F-30): `vs_currency` (default `usd`) accepted and
  filtered on both `get_latest_quote` and `list_quotes`; `get_latest_quote` bounded to a 48h
  trailing freshness window (stale → 404); `list_quotes` given a default 48h trailing window when
  neither `start` nor `cursor` is supplied; the module-wide "every `coin_quotes` read is
  ts-bounded" invariant upheld with no exemption; the `@MX:ANCHOR`/`@MX:WARN` text updated to match.
- **Uniform extractor error bodies** (F-33): `ApiJson<T>` / `ApiQuery<T>` / `ApiPath<T>` wrappers
  via `#[derive(FromRequest)]` / `#[derive(FromRequestParts)]` with `rejection = ApiError`; the
  dead `From<JsonRejection>` made live and extended to `QueryRejection` / `PathRejection`;
  handlers migrated to the wrappers.
- **Aggregation reachability** (F-31): source query upper bound `ts < end + one bucket` when `end`
  is present; cap-hit-but-empty continuation cursor derived from the oldest fetched source row's
  bucket start.
- **Idempotent registration** (F-32): `register_coin` uses `INSERT ... ON CONFLICT (coin_id) DO
  NOTHING RETURNING ...` (zero rows → re-select → 200); insert + three enqueues in one transaction.
- **Search 503** (F-34): pacer/cooldown/credit-exhaustion provider errors → 503; genuine no-match
  stays 200-empty.
- **`as_of` ceiling** (F-35): `tokio::sync::Semaphore` bounding the recompute path.
- **WebSocket read loop** (F-36): `select!` over `rx.recv()` and `socket.recv()`, terminate on
  Close/`None`, periodic pings.
- **Schema truth** (F-57, F-58): `CoinCandle` anchor rewritten to the flat-table reality; pre-0011
  `tests/db_integration.rs` scenarios rewritten or deleted; migration-presence test renamed and
  extended to all current migrations.
- **Parameter parity** (F-59): the OpenAPI parity test extended to assert, per operation, that
  every documented query parameter exists on the corresponding param struct.

Out of scope: see Exclusions. This SPEC adds no endpoint, no migration, no new dependency, changes
no money representation (Decimal only), and does not touch the roadmap's other phases (worker
retry/backpressure, provider transport, lifecycle/observability, structural refactors).

## Decisions Restated (authoritative — LOCKED)

The following are LOCKED decisions carried verbatim in intent from the roadmap prompt; they are
not re-litigated at run.

- **D1 — Quote reads are 48h-bounded; stale → 404; no exemption (F-30).** `get_latest_quote` gains
  a **48h trailing freshness bound** (`ts >= now() - interval '48 hours'`); a coin whose newest
  quote is older than 48h returns **404 NotFound** (treated as "no current quote"). `list_quotes`
  with **neither `start` nor `cursor`** supplied gains the **same default 48h trailing window**.
  The literal "every `coin_quotes` read is ts-bounded" invariant is upheld across the whole
  module — **no exemption carve-out**. The `@MX:ANCHOR`/`@MX:WARN` text is updated so code and
  invariant agree. (REQ-API-402/403/404)
- **D2 — Extractor wrappers via `#[derive(FromRequest)]`, NOT `WithRejection` (F-33).** Use
  `#[derive(FromRequest)]` (bodies) / `#[derive(FromRequestParts)]` (query/path) with
  `rejection = ApiError`. `WithRejection` is **rejected** — `axum-extra` is not a dependency and
  **no new dependencies are allowed**. Introduce `ApiJson<T>` / `ApiQuery<T>` / `ApiPath<T>`; the
  previously dead `From<JsonRejection>` becomes live and is extended to `QueryRejection` /
  `PathRejection`. (REQ-API-409/410)
- **D3 — Search failures: pacer/cooldown/credit-exhaustion → 503; no-match → 200 (F-34).**
  Pacer/cooldown/credit-exhaustion provider errors map to **503 ServiceUnavailable**; a genuine
  no-match (provider returns an empty result) keeps **200-empty**. `api/crypto-collector.yaml`
  stays the contract of record (its `searchCoins` 503 is already documented; do not amend it away
  from 503). (REQ-API-411)
- **D4 — `as_of` ceiling via `tokio::sync::Semaphore` (F-35).** Bound the recompute path with a
  small `tokio::sync::Semaphore` — **no memoization, no new dependency**. (REQ-API-412)
- **D5 — `vs_currency` on both quote reads (F-29).** Both quote read endpoints accept
  `vs_currency: Option<String>` defaulting `"usd"` (matching every sibling endpoint) and filter
  `AND vs_currency = $n`. Because the PK is `(coin_id, vs_currency, ts)`, single-currency filtering
  also removes the duplicate-`ts`-across-currencies keyset row-loss hazard at page boundaries.
  (REQ-API-400/401)
- **D6 — Aggregation `end`-bound + cap-cursor from source (F-31).** The candles source query gains
  an upper bound `ts < end + one target-interval bucket` when `end` is present (mirroring the
  existing `source_start` lower-margin trick), making far-past windows reachable; when the row cap
  is hit but aggregation emits nothing, the continuation cursor derives from the oldest fetched
  **source** row's bucket start instead of terminating pagination. (REQ-API-405/406)
- **D7 — Idempotent `ON CONFLICT` registration in one transaction (F-32).** `register_coin` uses
  `INSERT ... ON CONFLICT (coin_id) DO NOTHING RETURNING ...` (zero rows → re-select → 200); the
  insert + three enqueues run in a **single transaction**. (REQ-API-407/408)
- **D8 — Bidirectional WebSocket read loop; subscription filtering deferred (F-36).**
  `handle_stream` `select!`s over `rx.recv()` and `socket.recv()`, terminating on
  `Message::Close`/`None`, and adds periodic pings. Per-coin subscription filtering (also noted in
  F-36) is **explicitly deferred** — not in this SPEC's scope. (REQ-API-413)
- **D9 — Schema truth: anchor + stale tests + migration presence + parity test (F-57/F-58/F-59).**
  Update the `CoinCandle` `@MX:ANCHOR` to the flat-table reality (per `migrations/0020`); the
  `CoinQuote` anchor is unchanged (`coin_quotes` is still partitioned). Rewrite or delete the
  pre-0011 `tests/db_integration.rs` scenarios that assert `tracked_markets`/`live_quotes`. Rename
  and extend the migration-presence test (`all_fourteen_migration_files_exist`) to cover all
  current migrations (0001–0021). Extend the OpenAPI parity test to assert per-operation query
  parameter parity. (REQ-API-414/415/416/417)
- **D10 — No new dependencies; Decimal only; opaque cursors (global).** No new crate is added
  (no `axum-extra`, no memoization crate). No `f64` for any money value (REQ-PROV-012). Keyset
  pagination stays opaque-cursor-based (REQ-API-070); any cursor-format change requires
  decode-compatibility or a documented break — none is introduced here.

---

## Change Surface (brownfield delta markers)

File-level scope only; exact function bodies / query text are deferred to `plan.md` / Run.

[MODIFY]
- `src/api/quotes.rs` — add `vs_currency` to `ListQuotesParams`; add a `vs_currency` query param
  to `get_latest_quote` (currently `Path`-only); add the 48h bound + `AND vs_currency = $n` to
  both quote reads; default 48h window on `list_quotes` when no `start`/`cursor`; generalize the
  ts-bound `@MX:ANCHOR`/`@MX:WARN` to cover all three coin_quotes readers (F-29/F-30).
- `src/api/coins.rs` — `register_coin` → `ON CONFLICT DO NOTHING RETURNING` + re-select + single
  transaction (F-32); `search_coins` → map pacer/cooldown/credit-exhaustion `ProviderError` to
  503, keep no-match at 200-empty (F-34); migrate extractors to the wrappers (F-33).
- `src/api/candles.rs` — source query `end` upper bound + cap-hit-empty cursor from source bucket
  (F-31); migrate extractors (F-33).
- `src/api/cycle_overlay.rs` — bound the `compute_as_of_page` recompute with a
  `tokio::sync::Semaphore` (F-35); migrate extractors (F-33).
- `src/api/websocket.rs` — `handle_stream` bidirectional `select!` + Close/None termination +
  periodic pings; update the existing `@MX:WARN` (F-36).
- `src/api/mod.rs` — make `From<JsonRejection>` live; add `From<QueryRejection>` /
  `From<PathRejection>`; extend `openapi_yaml_contains_all_operation_ids` (or add a sibling test)
  with the per-operation parameter-parity assertion (F-33/F-59).
- `src/api/metadata.rs`, `src/api/coin_market.rs` — migrate extractors to the wrappers (F-33).
- `src/models/quote.rs` — rewrite the `CoinCandle` `@MX:ANCHOR` + doc to the flat-table reality;
  leave the `CoinQuote` anchor unchanged (F-57).
- `tests/db_integration.rs` — rewrite or delete the pre-0011 scenarios asserting
  `tracked_markets`/`live_quotes` so the suite passes against a fresh migrated DB (F-58).
- `tests/migration_files.rs` — rename `all_fourteen_migration_files_exist` and extend to all
  current migrations (0001–0021) (F-58).
- `api/crypto-collector.yaml` — MAY gain a short description note on `listCoinQuotes` documenting
  the default 48h trailing window (contract-of-record clarity); no schema change (D10).

[NEW]
- `src/api/extract.rs` (or inline in `src/api/mod.rs`, OR-API5-2) — `ApiJson<T>` / `ApiQuery<T>` /
  `ApiPath<T>` wrapper types (F-33).
- A per-operation parameter-parity test (in `src/api/mod.rs` tests or a new test module) mapping
  each operationId to its Rust param struct's field names (F-59).
- A concurrency ceiling for the `as_of` recompute (a module-level `Semaphore` or an `AppState`
  field — OR-API5-4) (F-35).

[UNCHANGED]
- `coin_quotes` / `coin_candles` / `tracked_coins` schema — **no migration**. The completed parent
  SPECs (API-001..004) are not modified. The `CoinQuote` `@MX:ANCHOR` stays (still partitioned).
  Money representation stays `rust_decimal::Decimal` (REQ-PROV-012). Keyset cursor format is
  unchanged (opaque, decode-compatible).

---

## Design Summary (WHAT, not HOW)

### Quote reads (F-29, F-30)

Both `get_latest_quote` and `list_quotes` resolve `vs_currency` to `usd` when omitted
(`.unwrap_or("usd")`, no allow-list — the codebase convention) and filter `AND vs_currency = $n`.
`get_latest_quote` adds `AND ts >= now() - interval '48 hours'`; when the resulting query returns
no row, the existing `None → ApiError::NotFound` branch already yields 404 ("no current quote").
`list_quotes`, when neither `start` nor `cursor` is supplied, applies a default lower bound of
`now() - interval '48 hours'`; a supplied `start` or `cursor` defines the lower bound instead.
The module-wide invariant (already declared for the SPEC-API-004 overview query) now covers all
three readers: **no `coin_quotes` read in `src/api/` lacks a `ts` bound**.

### Uniform error bodies (F-33)

`ApiJson<T>` wraps `Json<T>` via `#[derive(FromRequest)]` with `rejection = ApiError`; `ApiQuery<T>`
and `ApiPath<T>` wrap `Query<T>`/`Path<T>` via `#[derive(FromRequestParts)]` with the same
rejection. `ApiError` gains `From<QueryRejection>` and `From<PathRejection>` alongside the now-live
`From<JsonRejection>`, each producing `ApiError::BadRequest(e.to_string())` → the documented
`{code, message}` JSON body (REQ-API-074). Handlers swap their bare `Json`/`Query`/`Path`
extractors for the wrappers.

### Search 503 (F-34)

`search_coins` matches the provider error: `ProviderError::Pacer(_)` (fleet-wide cooldown / credit
exhaustion) maps to `ApiError::ServiceUnavailable`; a genuine no-match — the provider returns an
empty result — keeps the 200-empty degrade. The exact treatment of adjacent variants (HTTP 429,
timeout) is OR-API5-3.

### Aggregation reachability (F-31)

The source query gains `AND ($n::TIMESTAMPTZ IS NULL OR ts < $n)` where `$n = end + target_secs`
(one-bucket upper margin, symmetric with the existing `source_start` lower margin), so a far-past
`[start, end]` window fetches its rows instead of always fetching newest-first and post-filtering
to nothing. In the cap-hit branch, when the source read returned the full cap **and** aggregation
emitted no bucket for the page, the continuation cursor is derived from the oldest fetched source
row's bucket start (not `agg.last()`, which is `None` when `agg` is empty).

### Idempotent registration (F-32)

`register_coin` opens a transaction, runs `INSERT INTO tracked_coins (...) ... ON CONFLICT
(coin_id) DO NOTHING RETURNING ...`; on zero returned rows it re-selects the existing row and
returns 200; on an inserted row it runs the three `ENQUEUE_QUEUE_SQL` enqueues and commits, then
returns 201. The whole unit (insert + enqueues) commits atomically.

### as_of ceiling (F-35)

`compute_as_of_page` acquires a permit from a bounded `tokio::sync::Semaphore` before the
`load_daily_series` + `compute_overlay(daily.clone())` + projection recompute, releasing it on
return. No result is cached; the ceiling only caps concurrent per-request CPU on the 256 Mi pod.

### WebSocket read loop (F-36)

`handle_stream` runs a `tokio::select!` over `rx.recv()` (broadcast payload → send to client) and
`socket.recv()` (client frame). It terminates on `Message::Close` or a `None`/error from
`socket.recv()`, and a periodic ping timer arm sends `Message::Ping` to detect dead peers.

### Schema truth (F-57, F-58, F-59)

The `CoinCandle` `@MX:ANCHOR` + doc comment describe the flat de-partitioned `coin_candles` (plain
table, PK `(coin_id, vs_currency, interval, ts)`, btree + BRIN retained, NOT monthly RANGE). The
pre-0011 `tests/db_integration.rs` scenarios asserting `tracked_markets`/`live_quotes` are
rewritten against the current schema or deleted. `all_fourteen_migration_files_exist` is renamed
and extended to assert 0001–0021. A per-operation parameter-parity test maps each documented
operation's query parameters to its Rust param struct and asserts every documented parameter has a
matching field.

---

## Requirements (GEARS)

### Quote-read contract: vs_currency (F-29)

- **REQ-API-400** (Event-driven): When a client requests `GET /v1/coins/{coin_id}/quotes/latest`
  with an optional `vs_currency` (default `usd`), the handler shall filter the read
  `AND vs_currency = $n` and return the newest quote for the resolved currency; `vs_currency` is
  not allow-list-validated (an unrecognised value simply matches no rows).
- **REQ-API-401** (Event-driven): When a client requests `GET /v1/coins/{coin_id}/quotes` with an
  optional `vs_currency` (default `usd`), the handler shall filter the history read
  `AND vs_currency = $n`; because the primary key is `(coin_id, vs_currency, ts)`, single-currency
  filtering shall eliminate the duplicate-`ts`-across-currencies keyset row loss at page
  boundaries (the strict `ts <` cursor no longer skips a co-timestamped row of another currency).

### Quote-read contract: ts bounds & partition pruning (F-30)

- **REQ-API-402** (State-driven): While no current quote exists within the trailing 48h window,
  `get_latest_quote` shall return 404 NotFound; the handler shall bound its read by
  `ts >= now() - interval '48 hours'` and shall not issue an unbounded parent scan.
- **REQ-API-403** (State-driven): While a `GET /v1/coins/{coin_id}/quotes` request supplies neither
  `start` nor `cursor`, `list_quotes` shall apply a default 48h trailing lower bound; when `start`
  or `cursor` is supplied, that value shall define the lower bound.
- **REQ-API-404** (Ubiquitous): Every read of `coin_quotes` performed by any handler in `src/api/`
  shall carry a `ts` lower bound so PostgreSQL partition pruning applies; the system shall not
  issue any `coin_quotes` read that lacks a `ts` bound — there is no exemption carve-out — and the
  `@MX:ANCHOR`/`@MX:WARN` invariant text shall be updated so code and invariant agree.

### Aggregation reachability (F-31)

- **REQ-API-405** (State-driven): While a `GET /v1/coins/{coin_id}/candles` aggregation request
  supplies `end`, the source query shall carry an upper bound `ts < end + one target-interval
  bucket` (mirroring the existing `source_start` lower margin), so a far-past `[start, end]` window
  is reachable rather than yielding an empty page indistinguishable from "no data".
- **REQ-API-406** (Event-driven): When the source row cap is hit but aggregation emits no bucket
  for the page, the handler shall derive the continuation cursor from the oldest fetched source
  row's bucket start rather than terminating pagination with a null `next_cursor`.

### Idempotent registration (F-32)

- **REQ-API-407** (Event-driven): When two concurrent `POST /v1/coins` requests register the same
  `coin_id`, `register_coin` shall use `INSERT ... ON CONFLICT (coin_id) DO NOTHING RETURNING ...`
  and, on zero returned rows, re-select the existing row and respond 200 (idempotent), never 500
  on a primary-key violation.
- **REQ-API-408** (Ubiquitous): `register_coin` shall execute the insert and the three initial
  collection enqueues inside a single database transaction, so no coin row is committed without its
  enqueues and no enqueues are committed without the coin.

### Uniform extractor error bodies (F-33)

- **REQ-API-409** (Ubiquitous): The API shall provide `ApiJson<T>` / `ApiQuery<T>` / `ApiPath<T>`
  extractor wrappers implemented via `#[derive(FromRequest)]` / `#[derive(FromRequestParts)]` with
  `rejection = ApiError` (not `WithRejection`, adding no new dependency), so a malformed body,
  query string, or path parameter shall produce the documented `{code, message}` JSON error body
  (REQ-API-074) rather than Axum's default `text/plain` rejection.
- **REQ-API-410** (Ubiquitous): The `/v1` handlers shall be migrated from the bare
  `Json`/`Query`/`Path` extractors to the `ApiJson`/`ApiQuery`/`ApiPath` wrappers; the previously
  dead `impl From<JsonRejection> for ApiError` shall become live and shall be extended with
  `From<QueryRejection>` and `From<PathRejection>`.

### Search 503 (F-34)

- **REQ-API-411** (Event-driven): When the `search_coins` provider call fails with a
  pacer/cooldown/credit-exhaustion error, the handler shall respond 503 ServiceUnavailable (per
  the `searchCoins` OpenAPI contract of record); when the provider returns a genuine empty result,
  the handler shall respond 200 with an empty result — the handler shall not degrade
  pacer/cooldown/credit-exhaustion failures to 200-empty.

### as_of recompute ceiling (F-35)

- **REQ-API-412** (State-driven): While `as_of` cycle-projection requests are in flight, the
  recompute path shall bound concurrent recomputes with a `tokio::sync::Semaphore` (no
  memoization, no new dependency), so the per-request full-history recompute cannot be issued
  without a concurrency ceiling.

### WebSocket read loop (F-36)

- **REQ-API-413** (Event-driven): When a WebSocket client sends a `Close` frame or the socket
  returns `None`, `handle_stream` shall terminate the stream task; the handler shall `select!` over
  both `rx.recv()` and `socket.recv()` and shall send periodic pings.

### Schema truth (F-57, F-58)

- **REQ-API-414** (Ubiquitous): The `CoinCandle` `@MX:ANCHOR` and doc comment in
  `src/models/quote.rs` shall describe the flat (de-partitioned) `coin_candles` table per
  `migrations/0020_coin_candles_departition.sql` — a plain table, PK
  `(coin_id, vs_currency, interval, ts)`, btree + BRIN indexes retained, NOT monthly
  RANGE-partitioned — and the `CoinQuote` anchor shall remain unchanged (`coin_quotes` is still
  partitioned).
- **REQ-API-415** (Ubiquitous): The pre-0011 `tests/db_integration.rs` scenarios that assert the
  removed `tracked_markets` / `live_quotes` schema shall be rewritten against the current schema or
  deleted, so `tests/db_integration.rs` passes in full against a fresh migrated database.
- **REQ-API-416** (Ubiquitous): The migration-presence test `all_fourteen_migration_files_exist` in
  `tests/migration_files.rs` shall be renamed and extended to assert the presence of every current
  migration file (0001–0021).

### Parameter parity (F-59)

- **REQ-API-417** (Ubiquitous): The OpenAPI parity test shall be extended to assert, per operation,
  that every documented query parameter name appears as a field on the corresponding Rust param
  struct — the guard that would have caught F-29.

## Exclusions (What NOT to Build)

The following are explicitly **out of scope** for SPEC-API-005. Roadmap items outside the API
boundary are routed to their own phases; analysis/report content is routed elsewhere per the
SPEC-vs-report classification.

### Out of Scope — other roadmap phases
- No worker retry/backpressure (Phase 1), materializer/projection integrity (Phase 2), provider
  transport/pacer (Phase 3/4), lifecycle/shutdown/observability (Phase 6: F-38..F-49), or
  structural/batching debt (Phase 7: F-50..F-56). Those are separate SPECs.

### Out of Scope — new dependencies
- No new crate. `axum-extra` / `WithRejection` are rejected for F-33 (D2); no memoization crate for
  F-35 (D4). The fixes use only what is already in `Cargo.toml`.

### Out of Scope — OpenAPI contract rewrites
- `api/crypto-collector.yaml` is the contract of record and is not rewritten away from its
  documented `vs_currency` defaults or the `searchCoins` 503. The implementation converges to the
  contract; the only permitted yaml touch is an optional clarifying description note.

### Out of Scope — money representation
- No `f64` is introduced for any price/monetary value; `rust_decimal::Decimal` end-to-end is
  unchanged (REQ-PROV-012). No currency conversion / FX / new supported currencies.

### Out of Scope — cursor format changes
- Keyset pagination stays opaque-cursor-based (REQ-API-070). No cursor-format break is introduced;
  the F-31 cap-cursor change reuses the existing `TsKey` encoding (decode-compatible).

### Out of Scope — WebSocket subscription filtering
- F-36's per-coin subscription filtering (every client currently gets every coin) is deferred. This
  SPEC only fixes the read loop (Close handling + pings), not per-client topic filtering.

### Out of Scope — F-37 minor API notes
- The `delete_coin` UPDATE-then-probe TOCTOU (benign 404-vs-204 ambiguity), the three near-identical
  paginators (F-53), and the `Page<T: Serialize>` bound placement are not addressed here.

### Out of Scope — pre-existing comment/doc drift (note only)
- `src/api/quotes.rs:1` mis-cites `SPEC-API-002 REQ-API-131/132` (Module 3 quote REQs are
  REQ-API-120..123). Recorded for a future cleanup SPEC; not corrected here except incidentally if
  the header comment is already being edited for F-29/F-30.

## @MX Annotation Targets (high fan_in)

- `src/api/quotes.rs` — generalize the ts-bound `@MX:ANCHOR` + `@MX:WARN`/`@MX:REASON` so the
  "every `coin_quotes` read is ts-bounded" invariant covers `get_latest_quote` and `list_quotes`
  (the F-30 violators), not only `list_latest_quotes`. Keep the 41s/30s-client-timeout incident in
  `@MX:REASON` (REQ-API-404, D7 of SPEC-API-004).
- `src/models/quote.rs` — rewrite the `CoinCandle` `@MX:ANCHOR`/`@MX:REASON` to the flat-table
  reality (per `migrations/0020`); the `CoinQuote` anchor stays (still partitioned) (REQ-API-414).
- `src/api/extract.rs` (or `src/api/mod.rs`) — the `ApiJson`/`ApiQuery`/`ApiPath` wrappers +
  extended `From<*Rejection>` impls — `@MX:ANCHOR` (high fan_in: every handler routes rejections
  through here) + `@MX:REASON`: all extractor rejections must produce the uniform `{code, message}`
  body (REQ-API-074/409/410).
- `src/api/cycle_overlay.rs` — the `as_of` recompute semaphore — `@MX:WARN` + `@MX:REASON`:
  unbounded per-request full-history recompute is the most plausible resource-exhaustion vector on
  the 256 Mi pod (REQ-API-412).
- `src/api/websocket.rs` — update the existing `@MX:WARN` to reflect the bidirectional `select!`
  read loop + Close/None termination + pings (REQ-API-413).

## Open Items (do not guess)

- **OR-API5-1 — `list_quotes` default window vs an explicit `end`.** D1 mandates a default 48h
  trailing window when neither `start` nor `cursor` is supplied. When `end` alone is supplied (no
  `start`/`cursor`), confirm at run whether the 48h window anchors on `now()` (LOCKED literal,
  possibly making a far-past `end`-only query empty) or on `end` (making far-past history reachable,
  mirroring the F-31 candles fix). Default to the LOCKED literal (`now() - 48h`) unless
  consumer/measurement input says otherwise.
- **OR-API5-2 — Extractor wrapper placement.** Whether `ApiJson`/`ApiQuery`/`ApiPath` live in a new
  `src/api/extract.rs` module or inline in `src/api/mod.rs`, and whether every `Path` extractor
  needs migration (most are `Path<String>` which rarely rejects; `Path<(String, String)>` in
  `cycle_overlay.rs` is the realistic reject site). Run-phase decision.
- **OR-API5-3 — Search error-variant mapping beyond the LOCKED set.** D3 locks
  pacer/cooldown/credit-exhaustion → 503 and genuine no-match → 200. Confirm at run how adjacent
  `ProviderError` variants map — `Http { status: 429 }` (rate limit), a timeout `Network` error
  (the OpenAPI description says "timeout" too), `Http { 5xx }`, `Parse` — whether they also map to
  503 or stay degraded-to-empty. Do not silently widen the LOCKED set.
- **OR-API5-4 — as_of semaphore placement + permit count.** Whether the `Semaphore` is an
  `AppState` field (taxes every test constructor per the `AppState` `@MX:REASON`) or a module-level
  `static` (via `LazyLock`/`OnceLock`, avoiding the test-constructor churn). The permit count is a
  tuning parameter; pick a conservative default and record it at the `@MX:WARN`.
- **OR-API5-5 — F-31 exact margin + cap-cursor derivation.** Confirm the one-bucket upper margin
  (`ts < end + target_secs`) and the cap-hit-empty cursor derivation (oldest fetched source row's
  bucket start) by DB-backed tests: a far-past `[start, end]` window returns data, and a cap-hit
  gap-dropped page continues rather than terminating.
- **OR-API5-6 — F-58 rewrite-vs-delete per scenario.** Which pre-0011 `tests/db_integration.rs`
  scenarios to rewrite against the current schema versus delete (some assert schema that `0011`
  legitimately removed — `tracked_markets`, `live_quotes`, the market-keyed live-poller columns).
  The binding AC is "passes in full against a fresh migrated DB"; the rewrite/delete split per
  scenario is a run-phase judgment.

---

## Post-Implementation Notes (not acceptance criteria)

- Repo convention: commit directly to `main` (no feature branch); Tier M, Route A (Hybrid Trunk
  main-direct), no PR unless the user passes `--pr`.
- Several ACs require a live PostgreSQL to reach `completed` (duplicate-ts pagination, 48h-stale
  404, far-past aggregation window, ON CONFLICT registration, full `tests/db_integration.rs`,
  EXPLAIN plan-shape). Consistent with the SPEC-PROV-002/003 precedent, the SPEC holds at
  `implemented` until the DB-gated suite is run against live Postgres (`DATABASE_URL=... cargo test
  -- --ignored --test-threads=1`).
- The endpoint changes need `cargo build` + `make deploy` (namespace `finance`) before consumers
  see the new behavior. This is a deploy step, recorded as a note, not an acceptance criterion.
