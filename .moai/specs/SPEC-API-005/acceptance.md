---
id: SPEC-API-005
type: acceptance
updated: 2026-07-27
---

# SPEC-API-005 — Acceptance Criteria

Given/When/Then acceptance criteria for the API-boundary contract fixes, query bounds, and schema
truth. Each `AC-API-4NN` maps 1:1 to the matching `REQ-API-4NN`. `price`/monetary values are
asserted as JSON strings (DecimalString, no `f64` round-trip). "DB-backed" ACs run via
`DATABASE_URL=... cargo test -- --ignored --test-threads=1` against a fresh migrated database and
hold the SPEC at `implemented` until executed (SPEC-PROV-002/003 precedent). Router-level ACs use
`axum-test`; static-file ACs need no database.

## Global Acceptance Criteria (must all hold)

- **G1** — Every documented query parameter in `api/crypto-collector.yaml` is honored by its
  handler, enforced by the new per-operation parameter-parity test (AC-API-417).
- **G2** — No `coin_quotes` query in `src/api/` lacks a `ts` bound (F-30 leaves no exemption):
  `grep -n "FROM coin_quotes" src/api/*.rs` shows every occurrence carries a `ts >=` / `ts <` bound
  or a `now() - interval` predicate (AC-API-404).
- **G3** — `tests/db_integration.rs` passes in full against a fresh migrated database (AC-API-415).
- **G4** — All quality gates green: `cargo fmt --check`,
  `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test`.

## AC-API-400 — vs_currency on get_latest_quote (REQ-API-400) [DB-backed]

- Given `bitcoin` has recent `coin_quotes` rows in both `vs_currency=usd` and `vs_currency=eur`,
- When the client requests `GET /v1/coins/bitcoin/quotes/latest` with no `vs_currency`,
- Then the returned quote has `vs_currency=="usd"` (the `.unwrap_or("usd")` default),
- And When the client requests `?vs_currency=eur`, Then the returned quote has `vs_currency=="eur"`,
- And an unrecognised `?vs_currency=zzz` matches no rows (→ 404, not 400 — no allow-list).

## AC-API-401 — vs_currency on list_quotes + duplicate-ts keyset row loss fixed (REQ-API-401) [DB-backed]

- Given `bitcoin` has `coin_quotes` rows where a `usd` row and an `eur` row share the same `ts`
  at a page boundary,
- When the client paginates `GET /v1/coins/bitcoin/quotes?vs_currency=usd` across that boundary,
- Then every returned row has `vs_currency=="usd"`,
- And no `usd` row is skipped at the page boundary (the strict `ts <` cursor no longer advances past
  a co-timestamped row of another currency because the query filters `AND vs_currency = $n`).

## AC-API-402 — get_latest_quote 48h bound; stale → 404 (REQ-API-402) [DB-backed]

- Given `litecoin` is registered `active` but its only `coin_quotes` row is older than 48h,
- When the client requests `GET /v1/coins/litecoin/quotes/latest`,
- Then the response is 404 NotFound ("no current quote"),
- And the query carries `ts >= now() - interval '48 hours'` (no unbounded parent scan).

## AC-API-403 — list_quotes default 48h window when no start/cursor (REQ-API-403) [DB-backed]

- Given `bitcoin` has `coin_quotes` rows both inside and older than the last 48h,
- When the client requests `GET /v1/coins/bitcoin/quotes` with neither `start` nor `cursor`,
- Then only rows with `ts >= now() - interval '48 hours'` are returned (default trailing window),
- And When the client supplies an explicit `start` (or a `cursor`), Then that value defines the
  lower bound instead of the 48h default.

## AC-API-404 — every coin_quotes read is ts-bounded; EXPLAIN prunes (REQ-API-404) [DB-backed]

- Given a database with `coin_quotes` seeded across at least two monthly partitions,
- When `EXPLAIN (ANALYZE, BUFFERS)` is run on the `get_latest_quote` and `list_quotes` queries,
- Then each plan prunes `coin_quotes` partitions outside the bounded `ts` window (pruned partitions
  / "Subplans Removed"), and NO sequential scan hits the `coin_quotes` parent table,
- And a source grep confirms no `FROM coin_quotes` in `src/api/*.rs` lacks a `ts` bound (no
  exemption), and the `@MX:ANCHOR`/`@MX:WARN` text states the invariant covers all readers.

## AC-API-405 — aggregation end-bound makes far-past windows reachable (REQ-API-405) [DB-backed]

- Given `bitcoin` has native 1h candles spanning several months and no native 4h candles,
- When the client requests `GET /v1/coins/bitcoin/candles?interval=4h` with a far-past
  `start`/`end` window (e.g. 6 months ago),
- Then the response is 200 with aggregated 4h buckets inside `[start, end]` (not an empty page),
- And the source query carried an upper bound `ts < end + one target-interval bucket`.

## AC-API-406 — cap-hit-but-empty page continues via source-bucket cursor (REQ-API-406) [DB-backed]

- Given an aggregation request whose source read returns the full row cap but every fetched bucket
  is gap-dropped for the page,
- When the handler builds the response,
- Then `next_cursor` is non-null and derived from the oldest fetched source row's bucket start
  (pagination continues), rather than terminating with `next_cursor: null`.

## AC-API-407 — idempotent registration via ON CONFLICT (REQ-API-407) [DB-backed]

- Given `test-coin-api005` is not yet registered,
- When two `POST /v1/coins` requests register it (serial, and concurrent),
- Then the first returns 201 and the second returns 200 (idempotent), and neither returns 500,
- And the implementation uses `INSERT ... ON CONFLICT (coin_id) DO NOTHING RETURNING ...` with a
  re-select on zero returned rows.

## AC-API-408 — registration insert + enqueues are atomic (REQ-API-408) [DB-backed]

- Given a `POST /v1/coins` that inserts a new coin,
- When the handler runs,
- Then the `tracked_coins` insert and the three `collection_queue` enqueues (`metadata`, `market`,
  `candles`) commit inside a single transaction — a coin row never exists without its enqueues, and
  enqueues never exist without the coin.

## AC-API-409 — malformed body/query/path returns the JSON error body (REQ-API-409)

- Given the `ApiJson`/`ApiQuery`/`ApiPath` wrappers are wired,
- When a client sends a malformed JSON body (to `POST /v1/coins`), a malformed query value (e.g.
  `?limit=notanumber`), or a malformed path parameter,
- Then the response body is the documented `{code, message}` JSON with `content-type:
  application/json` (not Axum's default `text/plain`),
- And the status is 400 (BadRequest).

## AC-API-410 — handlers migrated; From<*Rejection> live (REQ-API-410)

- Given the extractor migration,
- When the `/v1` handlers are compiled,
- Then no handler uses a bare `Json`/`Query`/`Path` extractor for request data (they use
  `ApiJson`/`ApiQuery`/`ApiPath`),
- And `impl From<JsonRejection> for ApiError` is invoked (no longer dead code) and
  `From<QueryRejection>` + `From<PathRejection>` exist.

## AC-API-411 — search: pacer error → 503, no-match → 200 (REQ-API-411)

- Given a stub search provider in the chain,
- When the provider call fails with a pacer/cooldown/credit-exhaustion error
  (`ProviderError::Pacer(...)`), Then `GET /v1/coins/search?q=x` returns 503 ServiceUnavailable
  with the `{code, message}` JSON body,
- And When the provider returns a genuine empty result, Then the response is 200 with an empty
  result set (not 503).

## AC-API-412 — as_of recompute is concurrency-bounded (REQ-API-412)

- Given the `as_of` cycle-projection path,
- When an `as_of` request triggers a recompute,
- Then the recompute acquires a permit from a bounded `tokio::sync::Semaphore` before
  `load_daily_series` + `compute_overlay` + projection and releases it on return,
- And the endpoint still returns correct results under the ceiling (functional AC; no memoization,
  no new dependency).

## AC-API-413 — WebSocket client Close terminates the stream (REQ-API-413)

- Given a connected WebSocket client on `/v1/coins/stream/quotes`,
- When the client sends a `Close` frame,
- Then the server-side `handle_stream` task terminates promptly (it `select!`s over `rx.recv()` and
  `socket.recv()` and breaks on `Message::Close`/`None`),
- And the handler sends periodic pings while the connection is open.

## AC-API-414 — CoinCandle anchor reflects the flat table (REQ-API-414)

- Given `migrations/0020_coin_candles_departition.sql` flattened `coin_candles`,
- When `src/models/quote.rs` is read,
- Then the `CoinCandle` `@MX:ANCHOR` + doc describe a plain (non-partitioned) table with PK
  `(coin_id, vs_currency, interval, ts)` and retained btree + BRIN indexes, and do NOT claim
  monthly RANGE partitioning,
- And the `CoinQuote` `@MX:ANCHOR` is unchanged (still describes RANGE partitioning — `coin_quotes`
  is still partitioned).

## AC-API-415 — db_integration passes against a fresh migrated DB (REQ-API-415) [DB-backed]

- Given a fresh database migrated to the current schema (through 0021),
- When `tests/db_integration.rs` runs (`--ignored --test-threads=1`),
- Then all scenarios pass — the pre-0011 scenarios asserting `tracked_markets`/`live_quotes` have
  been rewritten against the current schema or deleted, and no scenario is permanently red.

## AC-API-416 — migration-presence test renamed and complete (REQ-API-416)

- Given `tests/migration_files.rs`,
- When `cargo test --test migration_files` runs,
- Then the (renamed) migration-presence test asserts the presence of every current migration file
  0001–0021 (not the stale 14-file list), and passes.

## AC-API-417 — per-operation parameter-parity test (REQ-API-417)

- Given `api/crypto-collector.yaml` documents query parameters per operation,
- When the parameter-parity test runs,
- Then for each operation, every documented query parameter name has a matching field on the
  corresponding Rust param struct (the guard that would have caught F-29),
- And the test is verified once against a deliberately-removed parameter (it fails), then kept
  green with all parameters present.

## Edge Cases

- Unrecognised `vs_currency` on a quote read ⇒ matches no rows (404 for `get_latest_quote`, empty
  page for `list_quotes`), never a 400 (no allow-list) — AC-API-400/401.
- A coin registered `active` with only >48h-old quotes ⇒ `get_latest_quote` 404, and absent from a
  default-window `list_quotes` — AC-API-402/403.
- Malformed cursor / oversized limit still return 400 with the `{code, message}` JSON body via the
  wrappers (existing behavior preserved through the migration) — AC-API-409.
- A far-past aggregation window that legitimately has no source data ⇒ 200 empty page (distinct from
  AC-API-405's has-data case) — the fix makes has-data reachable without breaking the true-empty case.
- Concurrent duplicate registration ⇒ exactly one 201 + one 200, no 500 — AC-API-407.
- WebSocket peer that goes silent without a Close frame ⇒ detected by the periodic ping (dead-peer
  detection) — AC-API-413.
- No `f64` appears in any changed money path (`grep -rn 'f64' src/api/`) — G4 / REQ-PROV-012.

## Definition of Done

- [ ] `get_latest_quote` + `list_quotes` accept `vs_currency` (default `usd`) and filter
      `AND vs_currency = $n`; duplicate-`ts`-across-currencies keyset row loss is fixed —
      REQ-API-400/401.
- [ ] `get_latest_quote` is 48h-bounded (stale → 404); `list_quotes` applies a default 48h window
      when no `start`/`cursor` — REQ-API-402/403.
- [ ] No `coin_quotes` read in `src/api/` lacks a `ts` bound (no exemption); EXPLAIN prunes,
      no parent seq scan; `@MX` invariant text updated — REQ-API-404 / G2.
- [ ] Candle aggregation source query bounds `ts < end + one bucket`; cap-hit-empty page continues
      via source-bucket cursor — REQ-API-405/406.
- [ ] `register_coin` uses `ON CONFLICT DO NOTHING RETURNING` (200 on conflict, 201 on insert) with
      insert + enqueues in one transaction — REQ-API-407/408.
- [ ] `ApiJson`/`ApiQuery`/`ApiPath` wrappers via `#[derive(FromRequest)]`/`FromRequestParts`
      (no new dep); handlers migrated; `From<{Json,Query,Path}Rejection>` live; malformed
      body/query/path → `{code, message}` JSON — REQ-API-409/410.
- [ ] `search_coins` maps pacer/cooldown/credit-exhaustion → 503; genuine no-match → 200-empty —
      REQ-API-411.
- [ ] The `as_of` recompute path is bounded by a `tokio::sync::Semaphore` (no memoization, no new
      dep) — REQ-API-412.
- [ ] WebSocket `handle_stream` `select!`s over `rx.recv()`/`socket.recv()`, terminates on
      Close/None, sends periodic pings — REQ-API-413.
- [ ] `CoinCandle` `@MX:ANCHOR` reflects the flat table; `CoinQuote` anchor unchanged — REQ-API-414.
- [ ] Pre-0011 `tracked_markets`/`live_quotes` scenarios rewritten/deleted; `tests/db_integration.rs`
      passes in full against a fresh migrated DB — REQ-API-415 / G3.
- [ ] Migration-presence test renamed and extended to 0001–0021 — REQ-API-416.
- [ ] Per-operation parameter-parity test added and green (verified once against a removed param) —
      REQ-API-417 / G1.
- [ ] No new dependency added; no `f64` for any money value — D10 / REQ-PROV-012.
- [ ] Quality gate green: `cargo fmt --check`,
      `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test` — G4.
- [ ] DB-backed ACs (400, 401, 402, 403, 404, 405, 406, 407, 408, 415) verified via
      `DATABASE_URL=... cargo test -- --ignored --test-threads=1` (held at `implemented` until run).
