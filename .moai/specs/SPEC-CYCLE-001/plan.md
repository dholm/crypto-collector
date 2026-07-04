---
id: SPEC-CYCLE-001
type: plan
updated: 2026-07-04
---

# SPEC-CYCLE-001 — Implementation Plan

A new derived-analytics feature: a materialised Bitcoin halving-cycle overlay recomputed on the
periodic collector tick, plus a new keyset-paginated `/v1` read route. Additive — no existing
collection, provider, candle write path, cursor, or DTO changes. One new migration (next number
`0013_*.sql`). Methodology per `quality.yaml`. Commit directly to `main` (no feature branches).
Quality gate after each phase: `cargo fmt --check`, `cargo clippy --all-targets --all-features --
-D warnings`, `cargo test`.

## Milestones (priority-ordered, no time estimates)

### Phase 1 — Schema + migration for the overlay table (Priority High)

- New migration `migrations/0013_cycle_overlay.sql` creating the overlay table with `NUMERIC`
  columns for `price`, `norm_halving`, `norm_cycle_low`; a `days_since_halving` integer; a
  `cycle_number` and `halving_date`; the candle `ts` (day); and a PK on
  `(coin_id, vs_currency, cycle_number, days_since_halving)`. (REQ-CYCLE-040)
- A `CycleOverlayPoint` model struct (`Decimal` fields, `FromRow`) mirroring the table.
- Gate: `cargo test --test migration_files` (migration file presence/shape test) and `cargo build`.

### Phase 2 — Pure cycle math: partitioning, days-since-halving, normalization (Priority High)

- Compiled-in halving constants (`2012-11-28`, `2016-07-09`, `2020-05-11`, `2024-04-20`), a
  cycle-assignment function over half-open `[halving, next_halving)` windows, and a whole-day
  `days_since_halving` helper (day 0 = halving date). (REQ-CYCLE-010/011)
- Dual-normalization fold in `rust_decimal::Decimal`: `norm_halving = price / anchor`,
  `norm_cycle_low = price / cycle_low`, using the single `close` series for numerator and both
  denominators (D7). Includes the D8 missing-anchor forward-search and the D9 no-interpolation
  rule. (REQ-CYCLE-002/020/021/023/024/032/033)
- Mark the cycle-partitioning helper `@MX:ANCHOR`; mark the normalization fold `@MX:WARN`
  (series-mixing / running-min hazards).
- Gate: `cargo test` — pure unit tests for cycle boundaries (halving day belongs to its cycle,
  day before next halving is the last day), day-0 offset, exact `1.0` at the anchor and at the
  cycle low, sparse (non-contiguous) `days_since_halving` under gaps, and the missing-anchor
  forward-search producing `1.0` at the first available day.

### Phase 3 — In-progress cycle + partial-history handling (Priority High)

- Open-ended current cycle extends to the latest available daily candle; `cycle_low_price` is the
  running minimum over available days; provisional values recompute each tick. (REQ-CYCLE-012/034)
- Absent/partial early cycles produce no points and are not errors. (REQ-CYCLE-030/031)
- Gate: `cargo test` — in-progress running-min updates as a new low arrives; a cycle with zero
  stored candles yields zero points without error; a partial cycle yields only its available days.

### Phase 4 — Recompute driver on the periodic collector tick (Priority High)

- Read the configured coin's daily (`1d`) `coin_candles` for the resolved `vs_currency`, run the
  Phase 2/3 transform, and upsert/replace the overlay table as an idempotent full rebuild.
  (REQ-CYCLE-041/042)
- Wire into the collectors module on the same periodic cadence that refreshes candles — recommend a
  new `collection_queue` `kind` claimed under the existing lease / `SKIP LOCKED` discipline so the
  rebuild is single-owner across replicas (OR-CYCLE-3; SPEC-SCHED-001). Env-var config for coin,
  currency, and cadence via `src/config.rs`. (REQ-CYCLE-043)
- Mark the recompute entry point `@MX:NOTE` (idempotent rebuild; in-progress points provisional).
- Gate: `cargo test` for config defaults (`bitcoin`/`usd`); DB-backed idempotency (two consecutive
  rebuilds converge to the same rows) via `DATABASE_URL=... cargo test -- --ignored`.

### Phase 5 — Read route, DTO, and keyset cursor (Priority High)

- `GET /v1/coins/{coin_id}/cycle-overlay` handler in a new `src/api/cycle_overlay.rs`; register in
  `build_api_router` (`src/api/mod.rs`) — no literal/param ordering conflict (it is under the
  existing `/v1/coins/{coin_id}/…` param family; place alongside `candles`). (REQ-CYCLE-050)
- New keyset key `CycleOverlayKey { cycle_number, days_since_halving }` in `src/api/cursor.rs`,
  ordered `(cycle_number ASC, days_since_halving ASC)`, using the generic `encode_keyset_cursor` /
  `decode_keyset_cursor`; reuse `validate_limit`. (REQ-CYCLE-051/053)
- New `CycleOverlayPointDto` in `src/api/dto.rs` serialising `price`, `norm_halving`,
  `norm_cycle_low` as `DecimalString` and carrying `cycle_number`, `halving_date`,
  `days_since_halving`. (REQ-CYCLE-050/051)
- Optional `vs_currency` (default `usd`, `.unwrap_or("usd")` convention) and optional `cycle`
  filter; non-target coin / no overlay → 200 empty page. (REQ-CYCLE-052)
- Mark the new keyset key `@MX:ANCHOR`.
- Gate: `cargo test` — handler tests for pagination round-trip, both-baselines-present,
  empty-page-for-unknown-coin, 400-on-bad-cursor/limit, and default-`usd`.

### Phase 6 — OpenAPI parity + full suite (Priority Medium)

- Add the endpoint, `CycleOverlayPoint` schema, and operationId (e.g. `listCycleOverlay`) to
  `api/crypto-collector.yaml`; extend the doc-parity test. (REQ-CYCLE-054)
- Full `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo test`; DB-backed overlay scenarios opt-in via `DATABASE_URL=... cargo test -- --ignored`.

## Technical Approach Notes

- **Pure transform, read-only source.** The overlay is a deterministic function of `coin_candles`
  daily rows; only the overlay table is written. The transform (Phase 2/3) is fully unit-testable
  with in-memory fixtures before any DB or handler wiring.
- **Single-series normalization (D7).** Numerator and both denominators are the `1d` `close`. This
  is what makes the anchor day and the cycle-low day equal exactly `1.0`; a `@MX:WARN` guards
  against a future change that swaps the cycle-low denominator to the daily `low`.
- **Missing-anchor forward search (D8).** When the halving-date candle is absent, the anchor is the
  earliest cycle candle with `ts >= halving_date`; `days_since_halving` still measured from the
  true halving date, so the series can start at day N > 0. The cycle is flagged approximate.
- **No interpolation (D9).** Points exist only for days with candles; the `days_since_halving`
  sequence is sparse under gaps. This mirrors SPEC-API-003 REQ-API-211's "never fabricate".
- **In-progress provisional values (REQ-CYCLE-034).** The current cycle's running-min and latest
  day advance over time, so a recompute can alter earlier points of the current cycle. Idempotent
  full replacement is the simplest correct rebuild.
- **Decimal only.** `coin_candles.close` is `Decimal` (`src/models/quote.rs`); the transform stays
  in `Decimal` and serialises via `rust_decimal::serde::str` on the DTO, consistent with
  `CoinCandleDto` (`src/api/dto.rs`). No `f64` at any point (REQ-PROV-012, REQ-CYCLE-024).
- **Pagination over a 2D dataset.** The overlay is naturally cycle × day; a flat total order
  `(cycle_number, days_since_halving)` gives a stable keyset. The composite key reuses the existing
  opaque-cursor helpers unchanged.

## Risk Analysis

- **"= 1.0" invariant regression.** Mixing the price series (close) with a low-based cycle-low
  denominator, or normalizing before the running-min settles, breaks the anchor/low = `1.0`
  property. Covered by exact-`1.0` unit tests and an `@MX:WARN`.
- **Empty / partial early history.** Older cycles (2012/2016) likely have no stored candles; the
  transform must yield zero points for them without error. Explicit tests (REQ-CYCLE-030/031).
- **In-progress churn surprises consumers.** Provisional current-cycle values changing between
  ticks is by design (REQ-CYCLE-034) but must be documented so a consumer does not treat a shifted
  point as corruption; captured in the DTO/OpenAPI description.
- **Recompute cost.** A full rebuild reads the coin's entire daily history each tick; daily
  granularity keeps row counts modest (a decade ≈ 3–4k rows). If cost grows, an incremental rebuild
  of only the in-progress cycle is a later optimisation (out of scope here).
- **Multi-replica double-rebuild.** Two replicas rebuilding concurrently must converge; idempotent
  replacement plus single-owner claiming via the existing lease/`SKIP LOCKED` discipline
  (REQ-CYCLE-042) avoids torn writes.

## Dependencies / Sequencing

- Phase 1 (schema) precedes the recompute (Phase 4) and the read route (Phase 5).
- Phases 2–3 (pure transform) are independently unit-testable and land before wiring.
- Phase 4 (recompute) and Phase 5 (route) both depend on Phase 1's table + model; they are
  otherwise independent and can proceed in parallel after Phase 3.
- Phase 6 (OpenAPI + full suite) closes the loop.
- No dependency on other SPECs beyond the existing `coin_candles` schema (SPEC-DB-001 / SPEC-API-002),
  the periodic tick (SPEC-SCHED-001), and the `/v1` cursor/DTO/error conventions (SPEC-API-001).
  Optional reuse of SPEC-API-003 read-time `1d` aggregation is an Open Item (OR-CYCLE-4), not a
  dependency.
