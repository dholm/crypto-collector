---
id: SPEC-API-005
type: plan
updated: 2026-07-27
---

# SPEC-API-005 — Implementation Plan

Brownfield behavioral-defect + hygiene changes to the `/v1` HTTP API boundary (`src/api/`,
`src/models/quote.rs`, `tests/`). No new endpoint, no migration, no new dependency. Tier M
(~10-13 files touched, mostly surgical query/error/extractor changes). Methodology per
`quality.yaml` (brownfield TDD/DDD: characterize the existing handler behavior first, then apply
the fix). Commit directly to `main` (no feature branches), Route A (Hybrid Trunk main-direct).
Quality gate after each milestone: `cargo fmt --check`,
`cargo clippy --all-targets --all-features -- -D warnings`, `cargo test`.

Milestones are ordered by **decision-reversibility** — the decisions most likely to change on
review or measurement lead; the mechanical hygiene steps trail. M1–M3 are user-facing contract
changes (48h-stale 404, uniform error bodies, search 503) with the highest review value and the
LOCKED behavioral decisions; M4–M5 are behavioral-correctness fixes; M6–M7 are resource/lifecycle
guards; M8 is mechanical schema-truth cleanup that lands last on top of the corrected behavior.

## Milestones (priority-ordered by decision-reversibility, no time estimates)

### M1 — Quote-read contract & partition-pruning bounds (F-29, F-30) (Priority High)

Highest change-likelihood: a user-facing **behavior change** (a coin whose newest quote is >48h
old now returns 404, and `vs_currency` now filters). Lead so review focuses on the 404-on-stale and
default-window decisions. LOCKED (D1, D5).

- Add `vs_currency: Option<String>` to `ListQuotesParams`; add a `GetLatestQuoteParams
  { vs_currency: Option<String> }` query extractor to `get_latest_quote` (currently `Path`-only).
  Resolve to `usd` via `.unwrap_or("usd")`; no allow-list. (REQ-API-400/401)
- `get_latest_quote`: add `AND vs_currency = $n` and `AND ts >= now() - interval '48 hours'`; the
  existing `None → ApiError::NotFound` branch yields the 404-on-stale semantics. (REQ-API-402)
- `list_quotes`: add `AND vs_currency = $n`; when neither `start` nor `cursor` is supplied, apply a
  default lower bound `now() - interval '48 hours'` (a supplied `start`/`cursor` defines the lower
  bound instead). Single-currency filtering fixes the duplicate-`ts` keyset row loss. (REQ-API-401/403)
- Generalize the ts-bound `@MX:ANCHOR`/`@MX:WARN`/`@MX:REASON` so the "every `coin_quotes` read is
  ts-bounded" invariant covers all three coin_quotes readers, keeping the 41s/30s-timeout incident
  in `@MX:REASON`. (REQ-API-404)
- Open item OR-API5-1: the `end`-only (no `start`/`cursor`) window anchor — default to `now()-48h`
  (LOCKED literal) unless run measurement/consumer input says otherwise.
- Gate: router tests (`axum-test`) for `vs_currency` default + explicit filter and stale→404;
  DB-gated tests for duplicate-`ts`-across-currencies pagination not losing rows and an
  `EXPLAIN`/plan-shape check that `get_latest_quote`/`list_quotes` prune partitions (no parent seq
  scan), mirroring the SPEC-API-004 overview EXPLAIN test.

### M2 — Uniform extractor error bodies (F-33) (Priority High)

New cross-cutting **type interface** (`ApiJson`/`ApiQuery`/`ApiPath`) plus a broad handler
migration — second-highest review value; touches every handler file. LOCKED: `#[derive(FromRequest)]`,
NOT `WithRejection`, no new dependency (D2).

- Add `ApiJson<T>` (via `#[derive(FromRequest)]`, `rejection = ApiError`) for bodies and
  `ApiQuery<T>` / `ApiPath<T>` (via `#[derive(FromRequestParts)]`, `rejection = ApiError`) for
  query/path — in a new `src/api/extract.rs` or inline in `mod.rs` (OR-API5-2). (REQ-API-409)
- Make `impl From<JsonRejection> for ApiError` live; add `From<QueryRejection>` and
  `From<PathRejection>` (each → `ApiError::BadRequest(e.to_string())`). (REQ-API-410)
- Migrate handlers in `quotes.rs`, `coins.rs`, `candles.rs`, `cycle_overlay.rs`, `metadata.rs`,
  `coin_market.rs` from bare `Json`/`Query`/`Path` to the wrappers. Multi-file: split into logical
  units per file, analyze cross-file signature dependencies before parallelizing. (REQ-API-410)
- Mark the wrappers `@MX:ANCHOR` (high fan_in) + `@MX:REASON` (uniform `{code, message}` body).
- Gate: router tests asserting a malformed JSON body, a bad query value, and a bad path parameter
  each return `{code, message}` JSON (not `text/plain`) — no DB required.

### M3 — search 503 mapping (F-34) (Priority High)

User-facing **status-code change** — a distinguishable "provider down" vs "no matches". LOCKED (D3).

- In `search_coins`, match the provider error: map `ProviderError::Pacer(_)` (fleet-wide
  cooldown / credit exhaustion) to `ApiError::ServiceUnavailable`; keep the genuine no-match
  (provider returns an empty result) at 200-empty. (REQ-API-411)
- OR-API5-3: confirm the mapping of adjacent variants (`Http { 429 }`, timeout `Network`,
  `Http { 5xx }`, `Parse`) — the OpenAPI description mentions "timeout"; do not silently widen the
  LOCKED pacer/cooldown/credit set.
- `api/crypto-collector.yaml` stays the contract of record (its `searchCoins` 503 is already
  documented — no amendment).
- Gate: router test with a stub provider in the chain returning a `Pacer` error → assert 503;
  a stub returning an empty result → assert 200-empty. No DB required.

### M4 — Aggregation reachability & cap-cursor (F-31) (Priority Medium)

Behavioral correctness in candle-aggregation pagination. LOCKED (D6).

- Source query: add `AND ($n::TIMESTAMPTZ IS NULL OR ts < $n)` with `$n = end + target_secs`
  (one-bucket upper margin, symmetric with the existing `source_start` lower margin), so a far-past
  `[start, end]` window fetches its rows. (REQ-API-405)
- Cap-hit branch: when `source_hit_cap` and aggregation emitted no bucket, derive the continuation
  cursor from the oldest fetched **source** row's bucket start (not `agg.last()`, which is `None`
  when `agg` is empty). (REQ-API-406)
- OR-API5-5: verify the margin + cap-cursor by DB-backed tests.
- Gate: DB-gated test that a far-past `start`/`end` aggregation window returns data, and a cap-hit
  gap-dropped page continues (non-null `next_cursor`) rather than terminating.

### M5 — Idempotent registration + transaction (F-32) (Priority Medium)

Concurrency correctness. LOCKED (D7).

- `register_coin`: open a transaction; `INSERT ... ON CONFLICT (coin_id) DO NOTHING RETURNING ...`;
  on zero returned rows re-select the existing row → 200; on an inserted row run the three
  `ENQUEUE_QUEUE_SQL` enqueues; commit. Return 201 on insert, 200 on conflict. (REQ-API-407/408)
- Gate: DB-gated test that serial repeat registration returns 201 then 200 (extend the existing
  `db_register_coin_returns_201_and_200_on_repeat`); a concurrent-duplicate test asserting both
  requests succeed (one 201, one 200) with no 500.

### M6 — as_of recompute ceiling (F-35) (Priority Medium)

Resource-exhaustion guard. LOCKED (D4): `tokio::sync::Semaphore`, no memoization, no new dep.

- Add a bounded `tokio::sync::Semaphore`; `compute_as_of_page` acquires a permit before
  `load_daily_series` + `compute_overlay(daily.clone())` + projection, releasing on return.
  Placement (AppState field vs module-level `static` via `LazyLock`/`OnceLock`) and permit count:
  OR-API5-4. (REQ-API-412)
- Mark the recompute `@MX:WARN` + `@MX:REASON` (most plausible self-inflicted resource-exhaustion
  vector on the 256 Mi pod).
- Gate: a unit/router test that the `as_of` path still returns correct results under the ceiling
  (functional, not a load test); `cargo clippy` clean on the semaphore lifetime.

### M7 — WebSocket read loop + pings (F-36) (Priority Low)

Connection-lifecycle correctness. LOCKED (D8); subscription filtering deferred.

- `handle_stream`: `tokio::select!` over `rx.recv()` (payload → send) and `socket.recv()` (client
  frame); terminate on `Message::Close` or `None`/error; add a periodic ping timer arm. (REQ-API-413)
- Update the existing `@MX:WARN` to describe the bidirectional loop.
- Gate: an `axum-test` WebSocket test that a client Close frame terminates the stream task.

### M8 — Schema truth & test hygiene (F-57, F-58, F-59) (Priority Low)

Mechanical hygiene — lowest change-likelihood; lands last on the corrected behavior.

- `src/models/quote.rs`: rewrite the `CoinCandle` `@MX:ANCHOR` + doc to the flat de-partitioned
  reality (plain table, PK `(coin_id, vs_currency, interval, ts)`, btree + BRIN retained, NOT
  RANGE-partitioned, per `migrations/0020`). Leave the `CoinQuote` anchor unchanged. (REQ-API-414)
- `tests/db_integration.rs`: rewrite or delete the pre-0011 scenarios asserting
  `tracked_markets`/`live_quotes` (scenarios 01, 02, 04 [live_quotes], 12 [live_quotes], 13, 15,
  and the table-list assertions) against the current `tracked_coins`/`coin_quotes`/`coin_candles`
  schema, so the suite passes in full against a fresh migrated DB. Per-scenario rewrite-vs-delete:
  OR-API5-6. (REQ-API-415)
- `tests/migration_files.rs`: rename `all_fourteen_migration_files_exist` (e.g.
  `all_migration_files_exist`) and extend the expected list to 0001–0021. (REQ-API-416)
- Add the per-operation parameter-parity test: map each operationId → its Rust param struct's
  field names, and assert every documented query parameter name in `api/crypto-collector.yaml` has
  a matching struct field (the F-29 guard). (REQ-API-417)
- Gate: `tests/migration_files.rs` (no DB) passes; the parity test passes; the full
  `tests/db_integration.rs` passes against a fresh migrated DB (DB-gated).

## Technical Approach Notes

- **Partition pruning is the F-30 driver (REQ-API-404).** `coin_quotes` is `PARTITION BY
  RANGE(ts)` (~48 monthly partitions). The `(coin_id, vs_currency, ts DESC)` btree does NOT prevent
  scanning every partition when there is no `ts` predicate — only a `ts` lower bound prunes. The
  48h window mirrors the SPEC-API-004 overview endpoint's proven shape; `now()` is `STABLE` →
  execution-time pruning (`EXPLAIN` shows pruned partitions / "Subplans Removed").
- **F-33 extractor mechanics.** `Json` is `FromRequest`; `Query`/`Path` are `FromRequestParts`. So
  `ApiJson` derives `FromRequest`, `ApiQuery`/`ApiPath` derive `FromRequestParts`, all with
  `rejection = ApiError`. The derive requires `ApiError: From<{Json,Query,Path}Rejection>` +
  `IntoResponse` (already present). No `axum-extra` / `WithRejection` (D2, D10).
- **F-34 error taxonomy.** `ProviderError::Pacer(AcquireSlotError::{Cooldown, CreditExhausted})` is
  the LOCKED 503 set. `search_coins` currently degrades every provider error to `vec![]`; the fix
  matches the error before degrading. Genuine no-match is the provider returning `Ok(vec![])` —
  that path is unchanged (200-empty).
- **F-32 transaction.** `pool.begin()` → `ON CONFLICT DO NOTHING RETURNING` → branch on
  `Option`/rows → enqueues → `tx.commit()`. `ENQUEUE_QUEUE_SQL` already exists
  (`collectors::collection_queue`). Note: `upsert_coin_metadata` elsewhere is also read-then-insert
  without a transaction (benign with a single claimant) — out of scope here.
- **F-35 semaphore.** `tokio::sync::Semaphore` is already available (tokio is a dep). A module-level
  `static` (via `std::sync::LazyLock`) avoids adding an `AppState` field (which the `AppState`
  `@MX:REASON` warns taxes every test constructor) — OR-API5-4.
- **No f64, opaque cursors (D10).** All money stays `rust_decimal::Decimal`. The F-31 cap-cursor
  reuses the existing `TsKey` `encode_keyset_cursor` — decode-compatible, no format break.

## Risk Analysis

- **F-30 behavior change (the crux).** 404-on-stale for `get_latest_quote` and the default 48h
  window on `list_quotes` change externally visible behavior for coins with no recent quote. LOCKED
  (D1) and made an AC; the overview endpoint's 48h window is the precedent. Mitigated by router +
  DB-gated tests and the EXPLAIN plan-shape guard (no parent seq scan).
- **F-33 broad migration.** Migrating every handler's extractors is mechanical but touches 6 files;
  a missed handler leaves a `text/plain` rejection surface. Mitigated by the multi-file
  decomposition (per-file units) and the malformed-body/query/path router tests.
- **F-34 over-widening.** Mapping too many `ProviderError` variants to 503 could mask genuine
  provider bugs as "down". Mitigated by the LOCKED pacer/cooldown/credit set (D3) + OR-API5-3
  keeping the residual-variant decision explicit.
- **F-31 pruning regression.** A poorly-placed `end` upper bound could defeat pruning or fetch too
  many rows. Mitigated by the one-bucket margin (symmetric with `source_start`) + DB-backed
  far-past-window test (OR-API5-5).
- **F-32 transaction scope.** Holding a transaction across the insert + three enqueues lengthens the
  connection hold slightly; benign on the small pool and correct-by-design. Mitigated by the
  concurrent-duplicate DB test.
- **F-58 fresh-DB parity.** The rewrite-vs-delete split (OR-API5-6) risks either deleting coverage
  or asserting stale schema. Binding AC: `tests/db_integration.rs` passes in full against a fresh
  migrated DB — the objective, planner-independent guard.

## Dependencies / Sequencing

- M1–M3 (quote contract, error bodies, search 503) are independent user-facing changes and can be
  developed in any order; M2's extractor wrappers are used by M1/M3/M4/M5 handlers, so landing M2
  early reduces churn (but M1/M3 can also land first on the bare extractors and migrate in M2).
- M4 (candles) and M5 (registration) are independent of each other and of M1–M3 (aside from the M2
  extractor migration touching the same files).
- M6 (as_of) and M7 (websocket) are independent guards touching their own files.
- M8 (schema truth) is pure hygiene and lands last on top of the corrected behavior; the parity
  test (F-59) validates M1's F-29 fix, so M8 naturally follows M1.
- No dependency on other SPECs beyond the existing schema (SPEC-DB-001) and the API surface the
  completed API-001..004 SPECs defined. None of those SPECs is modified.

## DB-gated ACs (hold at `implemented` until live Postgres)

Consistent with the SPEC-PROV-002/003 precedent, the following ACs require a live PostgreSQL and
hold the SPEC at `implemented` until run against live Postgres
(`DATABASE_URL=... cargo test -- --ignored --test-threads=1`; DB-gated tests share a global claim
queue and MUST run serially):

- Duplicate-`ts`-across-currencies pagination no longer loses rows (M1, REQ-API-401).
- 48h-stale `get_latest_quote` → 404 and default-window `list_quotes` (M1, REQ-API-402/403).
- `coin_quotes` reads prune partitions / no parent seq scan — EXPLAIN plan-shape (M1, REQ-API-404).
- Far-past `start`/`end` aggregation window returns data; cap-hit page continues (M4, REQ-API-405/406).
- Serial + concurrent `register_coin` ON CONFLICT returns success, no 500 (M5, REQ-API-407/408).
- `tests/db_integration.rs` passes in full against a fresh migrated DB (M8, REQ-API-415).

Non-DB ACs (router-level via `axum-test`, unit tests, static file checks) reach `implemented`
without live Postgres: malformed body/query/path → JSON error (M2), search 503 mapping (M3),
WebSocket Close terminates (M7), CoinCandle anchor text (M8, REQ-API-414), migration-presence test
(M8, REQ-API-416), parameter-parity test (M8, REQ-API-417).
