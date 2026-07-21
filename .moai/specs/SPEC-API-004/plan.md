---
id: SPEC-API-004
type: plan
updated: 2026-07-21
---

# SPEC-API-004 — Implementation Plan

Brownfield, read-only addition of one endpoint (`GET /v1/coins/quotes/latest`) to the coin
spot-quote read surface. No migration, no schema change, no writes. Tier S (one endpoint,
`< 5` files). Methodology per `quality.yaml` (brownfield: characterize the existing native
quote read first, then add the overview endpoint). Commit directly to `main` (no feature
branches). Quality gate after each milestone: `cargo fmt --check`,
`cargo clippy --all-targets --all-features -- -D warnings`, `cargo test`.

Milestones are ordered by **decision-reversibility** — the decisions most likely to change on
review or measurement lead; the mechanical steps trail. The bounded query (M1) is the crux and
the highest-change-likelihood decision (it is an explicit PROPOSAL to verify by measurement); the
response DTO/envelope (M2) is a new user-facing type interface with consumer-agreed field names;
handler wiring, routing, and OpenAPI/tests follow.

## Milestones (priority-ordered by decision-reversibility, no time estimates)

### M1 — Bounded overview query (the crux; verify by measurement) (Priority High)

Highest change-likelihood: the SQL shape is a proposal, not gospel. Lead with it so review focuses
on the partition-pruning decision.

- Compose a query over `tracked_coins` (filtered `status='active'`) + `coin_quotes` that yields, per
  active coin, the current spot quote and a 24h-ago baseline, with **every `coin_quotes` access
  bounded by `ts`** (`ts >= now() - interval '48 hours'` for the current price; `ts >= now() -
  interval '24 hours'` for the baseline). Start from the consumer's `CROSS JOIN LATERAL` (current) +
  `LEFT JOIN LATERAL` (baseline) proposal in spec.md § Design Summary. (REQ-API-300/305/306)
- The current-price `LATERAL` (`ORDER BY ts DESC LIMIT 1`) drops any coin with no quote in the 48h
  window (absent-on-stale, D5); the baseline `LATERAL` (`ORDER BY ts ASC LIMIT 1`) yields `NULL` →
  `open_24h: null` when the 24h window is empty (D3). (REQ-API-303/306)
- Placement (inline in the handler vs a new `src/db/` read fn) is open — OR-API4-1; there is no
  dedicated quote-read fn in `src/db/` today (quotes are inline in `src/api/quotes.rs`).
- Mark the query `@MX:ANCHOR` + `@MX:WARN`/`@MX:REASON`: every `coin_quotes` read MUST be
  `ts`-bounded; an unbounded `DISTINCT ON` over the parent touches all 48 partitions (a sibling
  service's 41s / 30s-timeout incident, D7).
- Gate (deferred to M6, named here): `EXPLAIN (ANALYZE, BUFFERS)` against a populated DB shows
  partition pruning (pruned partitions / "Subplans Removed" under `Append`) + index scans, NOT a
  parent-wide seq scan. `now()` is `STABLE` → execution-time pruning (OR-API4-4).

### M2 — Response DTO + envelope type (new type interface; consumer-agreed contract) (Priority High)

New user-facing type interface with fixed field names — second-highest review value.

- Add `CoinQuoteOverviewDto` to `src/api/dto.rs` beside `CoinQuoteDto` (`:133-158`) with fields
  `coin_id`, `vs_currency`, `ts`, `price`, `open_24h`, `source`. `price` uses
  `#[serde(with = "rust_decimal::serde::str")]`; `open_24h: Option<Decimal>` uses
  `#[serde(with = "rust_decimal::serde::str_option")]` (yields `"123.45"` or `null`). Decimal only,
  never `f64`. (REQ-API-302/304)
- Add the envelope `CoinQuoteOverviewPage { quotes: Vec<CoinQuoteOverviewDto> }` (bare `{"quotes":
  [...]}`) — deliberately NOT the shared `Page{items,next_cursor}` type. (REQ-API-301)
- Do NOT rename any field — the names are agreed with the Observatory BFF consumer (D2).
- Gate: `cargo test` — a serde unit test asserting `price` serialises as a JSON string and
  `open_24h` serialises as a JSON string when `Some` and as `null` when `None` (mirroring the
  existing `coin_quote_dto_price_serializes_as_string` /
  `coin_candle_dto_null_volume_serializes_as_null` tests in `src/api/dto.rs`).

### M3 — `list_latest_quotes` handler (Priority High)

- Add `list_latest_quotes(State, Query<{vs_currency: Option<String>}>)` to `src/api/quotes.rs`
  beside `get_latest_quote` (`:34-57`). Resolve `vs_currency` to `usd` via `.unwrap_or("usd")` when
  omitted; `vs_currency` is not validated against an allow-list. (REQ-API-307)
- Execute the M1 query bound to the resolved currency; map each row to `CoinQuoteOverviewDto`; wrap
  in `CoinQuoteOverviewPage`. An empty registry / no recent quote yields `{"quotes": []}`.
  (REQ-API-300/301/306)
- Mark the handler's `vs_currency` default + active-coin filter `@MX:NOTE` (REQ-API-306/307).
- Gate: `cargo test` — a handler-level unit/DB test that an empty registry returns `{"quotes": []}`.

### M4 — Route registration (literal-before-param) (Priority High)

- Register `.route("/v1/coins/quotes/latest", get(quotes::list_latest_quotes))` in
  `build_api_router` (`src/api/mod.rs:160-219`) **before** the parameterised
  `/v1/coins/{coin_id}` route (`:180-185`), alongside the existing literal-first block
  (`/v1/coins/search`, `/v1/coins/stream/*`). (REQ-API-148/308)
- Mark the registration line `@MX:NOTE` (literal-before-param).
- Gate: `cargo test` — extend the `all_routes_are_under_v1` route list and add/extend an ordering
  assertion (mirroring `stream_routes_precede_coin_id_param_route`) that
  `/v1/coins/quotes/latest` precedes `/v1/coins/{coin_id}`.

### M5 — OpenAPI documentation + doc-parity (Priority Medium)

- Document the operation in `api/crypto-collector.yaml` under `tags: [quotes]` with operationId
  `listLatestCoinQuotes`, an optional `vs_currency` query parameter, and a `CoinQuoteOverview` +
  `CoinQuoteOverviewPage` schema (bare `quotes` array; `open_24h` `nullable: true`).
- Add `"listLatestCoinQuotes"` to the `operation_ids` array in the
  `openapi_yaml_contains_all_operation_ids` doc-parity test (`src/api/mod.rs:401-418`). (REQ-API-309)
- Gate: `cargo test` — the doc-parity test passes with the new operationId.

### M6 — Full suite + DB-backed EXPLAIN proof (Priority Medium)

- Full `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo test`.
- DB-backed verification opt-in via `DATABASE_URL=... cargo test -- --ignored`: seed `coin_quotes`
  across ≥2 monthly partitions, then run `EXPLAIN (ANALYZE, BUFFERS)` on the overview query and
  assert the plan prunes partitions + uses the `(coin_id, vs_currency, ts DESC)` btree, and does NOT
  seq-scan the `coin_quotes` parent (REQ-API-305, Scenario 10). Follow the existing
  `#[ignore]` DB-test convention (`db_latest_quote_unknown_coin_returns_404` in `src/api/quotes.rs`).

## Technical Approach Notes

- **Read-only, on demand.** The endpoint computes per request from existing `coin_quotes` /
  `tracked_coins` rows; nothing is written and no result is cached. Pure addition to the read path,
  no migration (Exclusions).
- **Partition pruning is the design driver (D7 / REQ-API-305).** `coin_quotes` is `PARTITION BY
  RANGE(ts)`, 48 monthly partitions (`migrations/0011_remove_markets.sql:36-103`). The
  `(coin_id, vs_currency, ts DESC)` btree indexes each partition but does NOT prevent scanning
  every partition when there is no `ts` predicate — only a `ts` bound prunes. Each `LATERAL` here
  carries `ts >= now() - interval '...'`, so the planner prunes to the recent 1-2 partitions. Since
  `now()` is `STABLE` (not constant), pruning is execution-time (PG11+): `EXPLAIN` shows pruned
  partitions / "Subplans Removed" under `Append` (OR-API4-4).
- **Baseline semantics (D4 / OR-API4-2).** `open_24h` = the price of the oldest `coin_quotes` row in
  `[now-24h, now]` (`ORDER BY ts ASC LIMIT 1`), a backward scan of the `ts DESC` btree. Earliest-in-
  window ≈ 24h old for a continuously-tracked coin; younger for a coin tracked < 24h. The fixed
  contract has no baseline `ts`, so confirm earliest-in-window is intended at run.
- **Absent-on-stale (D5).** The `CROSS JOIN LATERAL` on the current price drops any coin with no
  quote in the 48h window — intended; consumer shows a dash. The wider 48h (vs 24h) tolerates one
  missed poll while still pruning partitions.
- **Envelope departure (D1).** Bare `{"quotes":[...]}` — no cursor, no `limit`. The result set is
  bounded by the active-coin count, so pagination is unnecessary. This is the only list read that
  does not use the `Page` wrapper; it is not generalised.
- **Decimal only.** `CoinQuote.price` is `Decimal` (`src/models/quote.rs`); the DTO serialises via
  `rust_decimal::serde::str` (price) / `str_option` (`open_24h`). No `f64` at any point
  (REQ-PROV-012, REQ-API-152).

## Risk Analysis

- **Unbounded-scan regression (the crux).** The single largest risk is a query that reads
  `coin_quotes` without a `ts` bound (e.g. a naive `DISTINCT ON (coin_id) ... ORDER BY coin_id, ts
  DESC` over the parent), which touches all 48 partitions and can produce a multi-second query (the
  sibling-service 41s / 30s-timeout incident, D7). Mitigated by REQ-API-305, the `@MX:ANCHOR`, and
  the M6 `EXPLAIN` acceptance criterion.
- **Route-ordering regression.** If `/v1/coins/quotes/latest` is registered after
  `/v1/coins/{coin_id}`, Axum could bind `{coin_id} = "quotes"` and the endpoint 404s or mis-routes.
  Mitigated by REQ-API-308 + the ordering test (M4).
- **`open_24h` zero-fill regression.** A `LEFT JOIN` that coalesces the missing baseline to `0`
  (instead of `NULL`) would send a false price. Mitigated by REQ-API-303 + the null-serialisation
  test (M2).
- **Baseline-window nuance (OR-API4-2).** Earliest-in-window may surprise if the consumer expected
  a strict ~24h-old point. Recorded as an open item to confirm at run.
- **Execution-time pruning not firing.** If the query is written so the planner cannot prune (e.g. a
  `ts` predicate the planner cannot push into the `LATERAL`), the endpoint silently regresses to a
  full scan. The M6 `EXPLAIN (ANALYZE, BUFFERS)` proof is the guard.

## Dependencies / Sequencing

- M1 (query) and M2 (DTO/envelope) are independent and can be developed in parallel; both feed M3
  (handler). M4 (routing) depends on M3's handler symbol. M5 (OpenAPI) depends on the final DTO
  field names + operationId. M6 (full suite + EXPLAIN) closes the loop and proves REQ-API-305.
- No dependency on other SPECs beyond the existing `coin_quotes` + `tracked_coins` schema
  (SPEC-DB-001) and the SPEC-API-002 quote read surface this sits beside. SPEC-API-002 is not
  modified.
