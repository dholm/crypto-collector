---
id: SPEC-CANDLE-001
type: spec-compact
updated: 2026-07-05
---

# SPEC-CANDLE-001 (compact) — Materialize native 1d/1w OHLCV (rollup)

Populate native `1d`/`1w` rows in `coin_candles` so `GET /v1/coins/{id}/candles` takes the
endpoint's fast native path. Load-bearing premise: `list_candles` runs a coin-level native `EXISTS`
probe first (`src/api/candles.rs:106-116`) and aggregates only when none exist — so materialized
rows fast the reads with NO read-path change. Reuse `candles_agg.rs` selection+bucketing+folding
unchanged; the only post-processing is relabeling the output `source` field to `rollup:*`.

## Requirements (EARS)

Module 1 — Materialization semantics [NEW]
- REQ-CANDLE-001 (Ubiquitous): materialize `1d`+`1w` per coin/vs_currency; pick source interval via `select_source_interval` (`candles_agg.rs:96-135`, the read path's own selector — coverage-scored, tie-break to LARGER divisor), `window_start=None`+injected `now` = deepest-coverage canonical source. NOT hand-rolled "finest"; matches read-time output for UNBOUNDED reads (read path uses `window_start=params.start`, `candles.rs:187`; a bounded read's coarser-source pick no longer applies once native rows serve one canonical `rollup:*` series — intended consistency gain).
- REQ-CANDLE-002 (Ubiquitous): UTC-epoch-aligned `bucket_start` — `1d`=midnight, `1w`=epoch-Thursday (NOT ISO Monday), OR-API3-6.
- REQ-CANDLE-003 (Ubiquitous): `source = rollup:<source_interval>` via **post-fold relabel** of `aggregate_candles` output (overwrites only `source`; OHLCV/ts/interval untouched); distinct from read-time `aggregated:*` (never persisted).
- REQ-CANDLE-004 (Ubiquitous): OHLC open=first/close=last/high=max/low=min; volume=sum with null-propagation (NULL iff any component NULL); all `Decimal`; reuse `aggregate_candles`/`fold_volume` numeric+bucketing logic UNCHANGED (only the -003 source relabel post-processes output).
- REQ-CANDLE-005 (Complex): while forming bucket (contains now) → emit even if partial; while closed+incomplete → drop (no fabrication) = read-time completeness parity; cross-run set-parity enforced by REQ-CANDLE-022 reconcile.

Module 2 — Full-history backfill (chunked) [NEW]
- REQ-CANDLE-010 (Event): when rollup runs for coin/interval with no rows → full backfill from earliest ts.
- REQ-CANDLE-011 (Ubiquitous): windows are bounded multiples of 604800s (week+day aligned; no bucket straddles); load only window's source.
- REQ-CANDLE-012 (Unwanted): if full history would load entire finer series at once → shall NOT (256Mi OOM, `cycle_overlay.rs:494-498`).
- REQ-CANDLE-013 (Ubiquitous): partition-safe insert; `ensure_candle_partition` per ts across 2017→today (static partitions only 2024-2027).

Module 3 — Incremental maintenance (`kind='rollup'`) [NEW/MODIFY]
- REQ-CANDLE-020 (Event): when `("coin","candles")` completes → enqueue `("coin",id,"rollup")`.
- REQ-CANDLE-021 [MODIFY] (Event): periodic tick enqueues `rollup` per active coin (backstop).
- REQ-CANDLE-022 (State): while processing rollup → recompute only from max-materialized `bucket_start` forward (≤~1wk source) AND reconcile window (upsert emitted + DELETE now-non-emitted buckets in `[recompute_start,now]`); no full rescan.
- REQ-CANDLE-023 (Optional): where duplicate rollup items enqueued → `ON CONFLICT DO NOTHING` dedup absorbs.
- REQ-CANDLE-024 (Ubiquitous): rollup task is network-free (no provider/pacer), mirrors `("coin","cycle_overlay")`.

Module 4 — Read-path integration & fallback [EXISTING/NEW]
- REQ-CANDLE-030 [EXISTING] (Event): when native rows exist → endpoint serves native path (no change).
- REQ-CANDLE-031 [EXISTING] (Unwanted): if no native rows → fall back to read-time aggregation; shall NOT be removed.
- REQ-CANDLE-032 (Ubiquitous): shall NOT alter HTTP schema, `TsKey` cursor (`cursor.rs:39-43`), or client contract.

Module 5 — Integrity, idempotency, migration [EXISTING/NEW/MODIFY]
- REQ-CANDLE-040 [EXISTING] (Ubiquitous): idempotent via `(coin_id,vs_currency,interval,ts)` upsert; re-run updates forming bucket in place.
- REQ-CANDLE-041 [EXISTING] (Ubiquitous): all OHLCV `Decimal`, never `f64` (REQ-PROV-012).
- REQ-CANDLE-042 [MODIFY] (Unwanted): if kind not permitted → enqueue fails; migration widens `collection_queue_kind_check` to admit `rollup` (template 0014); re-added constraint enumerates full set: spot,candles,metadata,market,derivatives,cycle_overlay,rollup.
- REQ-CANDLE-043 (Optional): where batched insert added to avoid per-row tx+NOTIFY → same conflict semantics + partition-ensure.

## Files

[EXISTING] `src/api/candles.rs:69-293` (native probe/fallback, unchanged), `src/api/candles_agg.rs`
(bucketing reused), `src/db/upserts.rs:105-153` (`upsert_coin_candle` templated), `src/db/partitions.rs:39-75`
(`ensure_candle_partition`), `src/models/quote.rs:33-46` (`CoinCandle`).

[MODIFY] `src/collectors/collection_queue.rs:329-590` (add `("coin","rollup")` arm; enqueue after candles),
`src/main.rs:426-490` (periodic rollup enqueue backstop).

[NEW] `src/collectors/rollup.rs` (materializer: chunked backfill + forward-only incremental + batched insert),
`migrations/00NN_collection_queue_rollup_kind.sql`, rollup tests in candles_agg.rs / rollup.rs / candles.rs.

## Scenarios (Given/When/Then, condensed)

1. Backfill BTC → `GET /candles?interval=1d&limit=1000` native (`rollup:*`), ~3210 bars in ≤4 fast pages, <3s (not 19 pages ~15s). (001,010,030)
2. `interval=1w&limit=1000` → native `rollup:*`, `ts%604800==0` (Thursday), couple fast pages. (001,002)
3. Deep cursor pagination → every page native (coin-scoped EXISTS), never flips to `aggregated:*`. (030)
4. OHLCV parity: for a pinned UNBOUNDED read (interval, start omitted→window_start=None, vs_currency, fixed now) over closed complete buckets, materialized == read-time output on ts/interval/OHLC + volume-null + emitted-bucket-set; `source` column excluded (rollup:* vs aggregated:*). (001,004,005)
5. New today `5m` → forming 1d/1w re-upserted in one cycle, prior days untouched, ≤~1wk reload. (020,022)
6. Un-materialized coin/interval → read-time aggregation fallback still correct (`aggregated:*`). (031)

Repro-first (write to FAIL first): (1) rollup unit — exact OHLC/volume-sum/null-prop, Thursday weeks;
(2) incremental — add 5m to day D updates only D's bucket, no full rescan; (3) read-path #[ignore] —
materialized rows served natively (`rollup:*` not `aggregated:*`); (4) all prior tests stay green.

Edge: null-vol→NULL not 0; incomplete-closed→dropped (parity); forming-then-incomplete→deleted by REQ-CANDLE-022 window-reconcile (no lingering);
no divisor→0 rows+fallback; pre-2024 ts→`ensure_candle_partition`; dup enqueues→dedup.

## Exclusions (What NOT to Build)

- No HTTP schema / `TsKey` cursor / client-contract change — data-population only.
- Do NOT remove coverage-aware read-time aggregation fallback.
- No parallel scheduler — reuse `collection_queue` worker + periodic tick.
- No downstream client change (fast large-paged reads fit the existing 3s budget).
- No new provider, no pacer, no network calls in the rollup path.
- No forked bucketing math — `candles_agg.rs` is the single source of truth.
