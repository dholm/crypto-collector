# SPEC-CANDLE-001 — Acceptance Criteria

Given/When/Then scenarios, reproduction-first tests, edge cases, quality gates, and Definition of
Done for materializing native `1d`/`1w` OHLCV. Every criterion is observable (test output, row
source marker, page count, or latency).

## Reproduction-First Tests (HARD — write to FAIL first)

These MUST be written and confirmed failing before implementation, then pass after (Rule 4). Pure
tests go in `src/api/candles_agg.rs` / `src/collectors/rollup.rs`; DB-gated ones are `#[ignore]` and
use `db_test_server()` (`src/api/candles.rs:505-844` pattern). Fixtures: `make_candle` /
`make_candle_null_vol` (`src/api/candles_agg.rs:321-361`).

1. **Rollup unit test (pure).** Given known `5m` candles spanning several days (including one day
   with a `NULL`-volume source candle), when the materializer folds them, then it produces the
   expected `1d` (and `1w`) OHLCV rows with: `open`=first-in-bucket, `close`=last-in-bucket,
   `high`=max, `low`=min, `volume`=sum, and `volume=NULL` for the day containing the null-volume
   source. Assert exact open/close ordering, high/low, volume sum, and null propagation. Assert
   `1w` buckets are epoch-Thursday-anchored, not Monday.
2. **Incremental-update test (pure or DB).** Given an existing materialized `1d` bucket for day D,
   when a new `5m` candle is added inside day D and the forward-only recompute runs, then day D's
   `1d` bucket is updated (close/high/low/volume reflect the new candle) and no other day's bucket
   is touched. Assert the recompute window did not rescan full history.
3. **Read-path test (DB, `#[ignore]`).** Given materialized `1d` rows exist for a coin, when
   `GET /v1/coins/{coin}/candles?interval=1d` is called, then the endpoint serves them natively:
   every returned `source` starts with `rollup:` (NOT `aggregated:`), and read-time aggregation is
   not invoked (the native precedence probe short-circuits). Mirrors the existing native-source
   assertion at `src/api/candles.rs:600-608`.
4. **Regression guard.** All existing coverage-aware aggregation tests
   (`src/api/candles_agg.rs:295-1105`, `src/api/candles.rs:505-844`) stay green — the read-time
   fallback is unchanged (REQ-CANDLE-031).

## Given/When/Then Scenarios

### Scenario 1 — Backfill makes daily reads fast and native (REQ-CANDLE-001/010/030)
- **Given** BTC has deep `5m` history spanning ~2017-08-18→today and no native `1d` rows.
- **When** the `rollup` task backfills, **and** a client then calls
  `GET /v1/coins/bitcoin/candles?interval=1d&limit=1000`.
- **Then** the response contains native rows whose `source` is `rollup:5m` (or whichever divisor
  `select_source_interval` picks), up to ~1000 per page; a full walk of ~3,210 daily bars completes in a few fast native
  pages (≤4 at `limit=1000`), well within the client's 3s fetch budget — NOT 19 slow aggregation
  pages (~15s).

### Scenario 2 — Weekly reads are fast and Thursday-anchored (REQ-CANDLE-001/002)
- **Given** BTC has been backfilled for `1w`.
- **When** a client calls `GET /v1/coins/bitcoin/candles?interval=1w&limit=1000`.
- **Then** native `1w` rows are served (`source=rollup:*`), each bucket `ts` aligned to
  epoch-Thursday (`ts.timestamp() % 604800 == 0`), the full weekly series completing in a couple
  fast native pages.

### Scenario 3 — Native precedence is not bypassed on deep pages (REQ-CANDLE-030, Load-Bearing Premise)
- **Given** materialized `1d` rows exist for BTC.
- **When** a client paginates past the first page using `next_cursor` (a deep cursor that yields an
  empty native window would exist for the aggregation path).
- **Then** every page is served from the native branch (`source=rollup:*`), never flipping to
  `aggregated:*` — because the `EXISTS` probe is coin-scoped, not page-scoped.

### Scenario 4 — OHLCV parity with prior aggregated output (REQ-CANDLE-001/004/005)
- **Given** the same BTC `5m` history and a **fixed injected `now`**. Because the read-time
  comparison uses an **unbounded** read (`start` omitted → `window_start = None`), both the
  materializer and the read path receive identical `select_source_interval` inputs and pick the same
  source (for a multi-interval coin, pin `start` to `None` so the read path does not select a coarser
  recent divisor).
- **When** the materialized `1d`/`1w` rows are compared against read-time `aggregate_candles` output
  for an **unbounded pinned** request (interval, `start` omitted, `vs_currency`, same frozen `now`),
  restricted to **closed, complete** buckets.
- **Then** `ts`, `interval`, open/high/low/close, and volume (including `NULL` where any source volume
  was `NULL`) are **equal**, and the emitted-bucket set matches (incomplete closed buckets dropped,
  forming bucket present). The `source` column is **excluded** from the comparison — it is
  intentionally `rollup:*` vs `aggregated:*`.

### Scenario 5 — Incremental refresh, no full rescan (REQ-CANDLE-020/022)
- **Given** BTC `1d`/`1w` are already materialized up to yesterday.
- **When** a new batch of `5m` candles for today is persisted and the `rollup` task runs.
- **Then** today's forming `1d` and `1w` buckets are re-upserted within one refresh cycle, prior days
  are untouched, and the recompute reloaded at most ~one week of source (no full-history scan).

### Scenario 6 — Fallback preserved for un-materialized coins (REQ-CANDLE-031)
- **Given** a coin/interval with no materialized rows (e.g. a newly tracked coin before its first
  rollup, or an interval with no divisible source).
- **When** its candles are requested.
- **Then** the endpoint still returns correct data via read-time coverage-aware aggregation
  (`source=aggregated:*`), unchanged by this SPEC.

## Edge Cases

- **Null-volume source in a bucket** → that bucket's `volume` is `NULL` (never 0).
- **Incomplete closed bucket** (missing source candles) → dropped, producing an intentional gap that
  matches read-time output; not an error.
- **Forming bucket later closes incomplete** → REQ-CANDLE-022's bounded window-reconcile deletes the
  now-non-emitted bucket on the next recompute. Acceptance: after a rollup recompute, the materialized
  set for the window `[recompute_start, now]` equals `aggregate_candles` output for that window (no
  lingering partial row).
- **No divisible source interval** (e.g. only `1M`) → zero materialized rows; read path falls back.
- **Historical `ts` before 2024** → insert succeeds because `ensure_candle_partition` creates/uses
  the covering monthly partition (partitions for 2017-2023 already exist from the `5m` backfill).
- **Duplicate `rollup` enqueues** → dedup-absorbed; at most one pending item per coin.

## Quality Gate Criteria

- `cargo fmt --check` — clean.
- `cargo clippy --all-targets --all-features -- -D warnings` — zero warnings.
- `cargo test` — all green, including the 4 reproduction-first tests and all prior coverage-aware
  aggregation tests.
- No `f64` in any OHLCV path (REQ-PROV-012 / REQ-CANDLE-041).
- Memory: full BTC backfill completes without OOM in the 256Mi pod (bounded per-window source load).

## Definition of Done

- [ ] Migration widens `collection_queue_kind_check` to admit `rollup`; enqueue no longer violates it.
- [ ] `("coin","rollup")` dispatch arm exists, network-free (no provider/pacer).
- [ ] Full-history backfill materializes BTC `1d`/`1w` spanning ~2017-08-18→today with correct OHLC.
- [ ] Materialized rows carry `source=rollup:<source_interval>`; never `aggregated:`.
- [ ] `GET /v1/coins/bitcoin/candles?interval=1d&limit=1000` served natively; full walk ≤ a couple
      seconds (a few native pages), within the client's 3s budget. Same for `1w`.
- [ ] Incremental recompute updates the forming day/week within one refresh cycle, no full rescan.
- [ ] Periodic refresh tick enqueues a `rollup` item per active coin as a backstop (REQ-CANDLE-021),
      dedup-absorbed alongside the post-candles enqueue.
- [ ] Batched historical insert path (if introduced, REQ-CANDLE-043) preserves the
      `(coin_id, vs_currency, interval, ts)` conflict target and calls `ensure_candle_partition` per
      distinct month — verified idempotent (re-run inserts no duplicates).
- [ ] Parity check passes: for a pinned (interval, start, vs_currency, fixed `now`) over closed
      complete buckets, materialized rows equal read-time output on ts/interval/OHLC + volume-null +
      emitted-bucket-set (`source` column excluded).
- [ ] Read-time coverage-aware aggregation fallback still works for un-materialized coins/intervals.
- [ ] No change to HTTP schema, `TsKey` cursor format, or client contract.
- [ ] Quality gate green (`fmt --check`, `clippy -D warnings`, `test`).
- [ ] `@MX` tags added on new rollup entry point + batched insert (WARN/REASON) per `plan.md`.
