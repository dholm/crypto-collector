# SPEC-CANDLE-002 — Acceptance Criteria

Every criterion is testable. DB-gated tests follow the project convention: `#[ignore]` +
`DATABASE_URL`, run with `--test-threads=1` (the DB-gated suite shares a global claim queue, per
CLAUDE.md Integration Tests). Pure tests run under plain `cargo test`.

AC → REQ traceability is stated per scenario. `acceptance.md` is the SSOT for AC count.

---

## Scenario 1 — Mixed-source window: native rows preserved, rollup rows still reconcile

**Covers:** REQ-CANDLE-050, REQ-CANDLE-051, REQ-CANDLE-053 · **AC-CANDLE-050** · DB-gated

- **Given** a coin/`vs_currency` whose `coin_candles` recompute window `[recompute_start, now]`
  contains BOTH native `1d` rows (`source` = a provider name, e.g. `bitstamp`) AND rollup-derived
  `1d` rows (`source = 'rollup:5m'`) at distinct `ts`,
- **And** a source `5m` series sufficient to reconcile the rollup buckets,
- **When** a full `rollup` reconcile cycle runs (`incremental_recompute_target` forward path),
- **Then** every native `1d` row in the window is **byte-identical** afterward (OHLCV, `ts`,
  `source` unchanged — neither deleted nor overwritten),
- **And** the rollup `1d` rows reconcile to **set-parity** with a full rebuild over the same closed,
  complete buckets (REQ-CANDLE-005 semantics preserved for the rollup-owned subset),
- **And** no native `ts` appears in the reconcile's delete set.

## Scenario 2 — Collision: native row wins at a shared timestamp

**Covers:** REQ-CANDLE-052 · **AC-CANDLE-052** · DB-gated

- **Given** an existing native `1d` row at a `ts` that an emitted rollup bucket will also target
  (same `(coin_id, vs_currency, interval, ts)`),
- **When** the rollup materializer upserts the emitted bucket,
- **Then** the native row is **unchanged** — its OHLCV and `source` are preserved (native-wins,
  Decision D1),
- **And** the rollup value does NOT overwrite it.

## Scenario 3 — History repair: source behind the watermark triggers a bounded backward pass

**Covers:** REQ-CANDLE-054, REQ-CANDLE-055, REQ-CANDLE-056, REQ-CANDLE-057 · **AC-CANDLE-055** · DB-gated

- **Given** a coin whose `1d`/`1w` rollup series has already been materialized from source starting
  at some `earliest_materialized` bucket,
- **When** additional source (`5m`) rows are inserted **behind** `earliest_materialized`
  (deep-backfill-after-rollup) and a `rollup` work item is then processed,
- **Then** the query-derived low-watermark comparison detects the earlier source (`MIN(ts)` bucket <
  earliest materialized bucket) and runs a bounded backward pass,
- **And** the end-state `1d`/`1w` materialized series **covers the backfilled history**
  (earliest materialized bucket has moved back to the source low-watermark bucket),
- **And** the backward pass walked week-aligned bounded windows (no full-series load — verified by
  the reuse of the `backfill_target` window loop; no OOM on a 256 Mi-class run),
- **And** re-running the `rollup` item produces **no further change** (idempotent, self-terminating),
- **And** no new migration / state column / table was introduced (REQ-CANDLE-057).

## Scenario 4 — Projection guard: non-positive closes do not panic (pure, no DB)

**Covers:** REQ-CANDLE-058, REQ-CANDLE-059, REQ-CANDLE-060 · **AC-CANDLE-058** · pure test

Two sub-cases exercise the two guard points in their **required order** — `current_price` is derived
from the unfiltered series, so the REQ-CANDLE-059 guard MUST run BEFORE the interior `close <= 0`
filter (see plan.md §4). Sub-case 4b is the direct regression against the ordering defect: it must
stay satisfiable even when positive history precedes the bad last row.

**Sub-case 4a — interior non-positive closes → fit over the positive subset (REQ-CANDLE-058/060):**
- **Given** a projection daily series whose **last (today) close is positive** but which contains an
  **interior** `close = 0` row and an **interior** `close < 0` row among otherwise-positive closes
  spanning enough history to fit,
- **When** the projection is computed,
- **Then** the computation does **not panic** (`log10`/`ln` is never reached with a non-positive
  argument),
- **And** a fit is produced over the **positive subset** — the interior zero/negative rows are
  dropped from the fit and logged at `warn!`,
- **And** the REQ-CANDLE-059 `current_price` guard is NOT triggered (today's positive close anchors
  continuity).

**Sub-case 4b — `current_price <= 0` → graceful skip, guard reachable (REQ-CANDLE-059):**
- **Given** a series whose derived `current_price` (last/today close) is `0` (or negative), **with
  positive history preceding it**,
- **When** the projection is computed,
- **Then** it returns a **graceful empty result, not a panic**, logged at `warn!` — the guard fires
  BEFORE the interior filter, so it is reachable despite the positive history preceding the bad last
  row (proving the filter did not shift `today`/`current_price` to a positive day),
- **And** `tests/backtest_projection.rs` passes **unchanged** (constants not tuned).

## Edge Cases

- **No source history (`MIN(ts)` NULL).** No watermark, no backward pass; first-run full-backfill
  (REQ-CANDLE-010) behavior unchanged. (REQ-CANDLE-054)
- **Watermark equal to / ahead of earliest materialized.** Pure `backward_repair_window` returns
  `None` — no backward pass. (REQ-CANDLE-055)
- **Collision inside the backward-repair pass.** The pass upserts through the same native-wins path,
  so a native row encountered during backward repair is also preserved. (REQ-CANDLE-052)
- **All closes non-positive.** Series filters to empty → `fit_model` returns `None` →
  projection returns `vec![]` gracefully, no panic. (REQ-CANDLE-058/060)
- **`reconcile_window` purity.** The pure core is unchanged and remains DB-free; the source filter
  lives at the SQL SELECT that feeds it. (REQ-CANDLE-050)

## Quality Gate — AC-CANDLE-QG

**Covers:** all REQs · non-negotiable

- `cargo test` — all suites pass (new pure + DB-gated tests green; DB-gated run with
  `--test-threads=1`).
- `cargo clippy --all-targets --all-features -- -D warnings` — zero warnings.
- `cargo fmt --check` — clean.
- `tests/backtest_projection.rs` — passes unchanged (backtest-locked constants intact).
- No new migration, no new dependency (`Cargo.toml` diff empty), no `f64` in any monetary path
  (Decimal-only, REQ-PROV-012).

## Definition of Done

- [ ] REQ-CANDLE-050 — `previously_materialized` SELECT carries `AND source LIKE 'rollup:%'`.
- [ ] REQ-CANDLE-051 — per-`ts` DELETE carries `AND source LIKE 'rollup:%'`; native rows never
      deleted/counted (Scenario 1).
- [ ] REQ-CANDLE-052 — native-wins collision guard in the upsert; native row unchanged on collision
      (Scenario 2).
- [ ] REQ-CANDLE-053 — rollup-owned subset still reconciles to set-parity (Scenario 1).
- [ ] REQ-CANDLE-054 — recompute no longer strictly forward-only; watermark comparison added.
- [ ] REQ-CANDLE-055 — bounded backward pass materializes history behind the watermark (Scenario 3).
- [ ] REQ-CANDLE-056 — backward pass reuses week-aligned bounded windows; no full-series load.
- [ ] REQ-CANDLE-057 — watermark query-derived; no new migration/column/table.
- [ ] REQ-CANDLE-058 — projection series filters `close <= 0`, logs at `warn!` (Scenario 4).
- [ ] REQ-CANDLE-059 — `current_price <= 0` graceful skip, no panic (Scenario 4).
- [ ] REQ-CANDLE-060 — no `log10`/`ln` panic path; constants unchanged; backtest green.
- [ ] `backward_repair_window` + projection guard extracted as pure, DB-free unit-tested cores.
- [ ] rollup module docs + `@MX:ANCHOR` (source-filter, native-wins) + `@MX:WARN` (memory-bounded
      backward pass) added per plan.md § MX Tag Targets.
- [ ] AC-CANDLE-QG passes (fmt, clippy -D warnings, test, backtest unchanged, no migration/dependency).
