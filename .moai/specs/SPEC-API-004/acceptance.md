---
id: SPEC-API-004
type: acceptance
updated: 2026-07-21
---

# SPEC-API-004 — Acceptance Criteria

Given/When/Then scenarios for the all-coin latest-quote overview endpoint. Each maps to one or
more `REQ-API-3NN`. `price` is asserted as a JSON string and `open_24h` as a JSON string or `null`
(DecimalString, REQ-API-304). Unless stated, the service runs against a database with `coin_quotes`
populated as described, and every request targets `GET /v1/coins/quotes/latest`.

## Scenario 1 — All-coin overview: current price + baseline per active coin (REQ-API-300, 302)

- Given `bitcoin` and `ethereum` are registered `active` and each has a `coin_quotes` row within
  the last 48h plus an earlier row within the last 24h,
- When the client requests `GET /v1/coins/quotes/latest`,
- Then the response is 200 and `quotes` contains one row per coin, each with `coin_id`,
  `vs_currency`, `ts`, `price`, `open_24h`, `source`,
- And each row's `price` is the coin's newest quote and `open_24h` is the earliest quote in the
  trailing 24h window.

## Scenario 2 — Bare `{"quotes":[...]}` envelope, not the Page wrapper (REQ-API-301)

- Given any populated registry,
- When the client requests `GET /v1/coins/quotes/latest`,
- Then the response body is a bare object with a single `quotes` array key,
- And the body has no `items` key and no `next_cursor` key (it is NOT the `Page{items,next_cursor}`
  wrapper), and no cursor/pagination is present.

## Scenario 3 — Empty registry yields an empty overview, not an error (REQ-API-300, 301)

- Given no coins are registered (or no active coin has a quote in the 48h window),
- When the client requests `GET /v1/coins/quotes/latest`,
- Then the response is HTTP 200 with body `{"quotes": []}` (not 404, not an error).

## Scenario 4 — Coin omitted when it has only stale (>48h) quotes (REQ-API-305, 306)

- Given `litecoin` is `active` but its only `coin_quotes` rows are older than 48h,
- When the client requests `GET /v1/coins/quotes/latest`,
- Then no row for `litecoin` appears in `quotes` (absent-on-stale; the current-price window is
  bounded to the last 48h and the CROSS JOIN LATERAL drops the coin).

## Scenario 5 — open_24h is null when no 24h-back quote exists (REQ-API-303)

- Given `dogecoin` is `active` and was tracked only recently: it has a quote within the last 48h
  (so it appears) but no quote older than the current one within the trailing 24h window,
- When the client requests `GET /v1/coins/quotes/latest`,
- Then `dogecoin`'s row is present with a non-null `price`,
- And its `open_24h` is JSON `null` (not `0`, not omitted).
- Note — this null-baseline condition covers **two distinct sub-cases**, both required to yield
  `null`:
  - (a) **quote only outside the 24h window**: the coin's newest quote sits between 24h and 48h
    old (e.g. a quote at ~30h), so it appears via the 48h current-price window but the trailing
    24h window holds no quote at all → `open_24h: null`.
  - (b) **newly-tracked, only a recent quote**: the coin's *only* quote is recent (e.g. ~1min old)
    and inside the 24h window, so that single quote is simultaneously the current price AND the
    only 24h-window row. The baseline MUST be strictly older than the current quote (`ts < q.ts`);
    with no strictly-earlier quote the baseline is `null`. The current quote MUST NOT be reused as
    its own baseline (that would report a fabricated 0.00% change) → `open_24h: null`, never
    equal to `price`.

## Scenario 6 — price serialises as a string, open_24h as a string or null (REQ-API-304)

- Given a coin with a current price of `67123.45` and a 24h baseline of `65000.00`, and another
  coin whose 24h window holds no earlier quote,
- When the client requests `GET /v1/coins/quotes/latest`,
- Then the first row serialises `"price":"67123.45"` and `"open_24h":"65000.00"` (JSON strings,
  never JSON numbers), with no `f64` round-trip,
- And the second row serialises `"open_24h":null`.

## Scenario 7 — Non-active coins are excluded (REQ-API-306)

- Given `bitcoin` is `active` with a recent quote, and `ethereum` has `status='paused'` (and a
  separate coin has `status='error'`) each with recent quotes,
- When the client requests `GET /v1/coins/quotes/latest`,
- Then only `bitcoin` appears in `quotes`; the `paused` and `error` coins are excluded regardless
  of having recent quotes.

## Scenario 8 — vs_currency default (usd) and explicit filter (REQ-API-307)

- Given `bitcoin` has recent `coin_quotes` rows in both `vs_currency=usd` and `vs_currency=eur`,
- When the client requests `GET /v1/coins/quotes/latest` with no `vs_currency`,
- Then every returned row has `vs_currency=="usd"` (the `.unwrap_or("usd")` default),
- And When the client requests `?vs_currency=eur`, Then every returned row has `vs_currency=="eur"`,
- And When the client requests `?vs_currency=xyz` (unrecognised), Then the response is HTTP 200 with
  `{"quotes": []}` (no allow-list validation; it simply matches no rows), not a 400.

## Scenario 9 — Route registration order: literal resolves as the endpoint (REQ-API-308)

- Given the router is assembled by `build_api_router`,
- When a request hits `GET /v1/coins/quotes/latest`,
- Then it resolves to `list_latest_quotes` (not to `/v1/coins/{coin_id}` with `coin_id="quotes"`),
- And the source registration order places `/v1/coins/quotes/latest` before the parameterised
  `/v1/coins/{coin_id}` route (verified by the ordering assertion mirroring
  `stream_routes_precede_coin_id_param_route`).

## Scenario 10 — EXPLAIN shows partition pruning + index scans, not a parent seq scan (REQ-API-305) [DB-backed]

- Given a database with `coin_quotes` seeded across at least two monthly partitions for several
  active coins,
- When `EXPLAIN (ANALYZE, BUFFERS)` is run on the overview query (resolved `vs_currency`),
- Then the plan prunes `coin_quotes` partitions outside the bounded `ts` window (pruned partitions
  / "Subplans Removed" under the `Append` node — `now()` is `STABLE`, so pruning is execution-time),
- And each `LATERAL` uses an index scan on `(coin_id, vs_currency, ts DESC)` (the current-price
  `ORDER BY ts DESC LIMIT 1` forward, the baseline `ORDER BY ts ASC LIMIT 1` backward),
- And the plan contains NO sequential scan over the `coin_quotes` parent table.
- Note: the per-`LATERAL` "index scan" sub-assertion is cost-planner-dependent — on a lightly
  seeded test DB the planner may legitimately prefer a per-partition seq scan over the index
  because the tables are tiny. At run, either seed enough rows that the index is the cheaper plan,
  OR set `enable_seqscan=off` for that specific assertion. The load-bearing guards — partition
  pruning applies AND no sequential scan hits the `coin_quotes` **parent** table — remain the
  primary, planner-independent checks (REQ-API-305).
- (This proof is deferred to the run phase and exercised via `DATABASE_URL=... cargo test --
  --ignored`.)

## Scenario 11 — OpenAPI doc-parity includes the new operationId (REQ-API-309)

- Given `api/crypto-collector.yaml` documents the new operation under `tags: [quotes]`,
- When `openapi_yaml_contains_all_operation_ids` runs,
- Then the YAML contains the operationId `listLatestCoinQuotes` and the test passes.

## Edge Cases

- Empty registry ⇒ `{"quotes": []}`, HTTP 200 (Scenario 3).
- A coin present in `tracked_coins` (active) but with zero `coin_quotes` rows in the 48h window ⇒
  absent from the response (Scenario 4).
- A coin with a current quote but no earlier quote in the trailing 24h window ⇒ present with
  `open_24h: null` (Scenario 5), never `0`, never field-omitted.
- An unrecognised `vs_currency` ⇒ HTTP 200 `{"quotes": []}` (no allow-list validation), unlike an
  invalid `interval` elsewhere which is a 400 (Scenario 8).
- All `price` and `open_24h` values serialise as JSON strings / `null`, never as JSON numbers, with
  no `f64` round-trip (REQ-API-304).
- No cursor / `limit` parameters are honoured; supplying them has no paginating effect (the response
  is always the full active-coin snapshot) (REQ-API-301).

## Definition of Done

- [ ] `GET /v1/coins/quotes/latest` returns one overview row per active tracked coin with a current
      quote, each carrying `coin_id`, `vs_currency`, `ts`, `price`, `open_24h`, `source` —
      REQ-API-300/302.
- [ ] Response is the bare `{"quotes":[...]}` envelope (no `items`/`next_cursor`, no cursor); empty
      result is `{"quotes":[]}` — REQ-API-301.
- [ ] `open_24h` is `null` (never `0`, never omitted) when the trailing 24h window holds no earlier
      quote — REQ-API-303.
- [ ] `price` serialises as a non-null JSON string and `open_24h` as a JSON string or `null`;
      Decimal end to end, no `f64` — REQ-API-304 (REQ-PROV-012).
- [ ] Every `coin_quotes` read is `ts`-bounded so partition pruning applies; `EXPLAIN (ANALYZE,
      BUFFERS)` on a populated DB shows pruned partitions + index scans and NO parent seq scan —
      REQ-API-305.
- [ ] Only `status='active'` coins are considered; a coin with no quote in the 48h window is absent
      — REQ-API-306.
- [ ] `vs_currency` defaults to `usd` and is not allow-list-validated (unrecognised ⇒ 200 empty) —
      REQ-API-307.
- [ ] `/v1/coins/quotes/latest` is registered before `/v1/coins/{coin_id}` and resolves as an
      endpoint, not `{coin_id}="quotes"` — REQ-API-308.
- [ ] The operation is documented in `api/crypto-collector.yaml` under `tags: [quotes]` and its
      operationId (`listLatestCoinQuotes`) is in the doc-parity test — REQ-API-309.
- [ ] Quality gate green: `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D
      warnings`, `cargo test`.
- [ ] No `f64` used for any price value (REQ-PROV-012 / REQ-API-304).
- [ ] DB-backed scenarios (4, 5, 8, 10) verified via `DATABASE_URL=... cargo test -- --ignored`.
