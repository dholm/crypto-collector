---
id: SPEC-CYCLE-001
type: plan
updated: 2026-07-14
---

# SPEC-CYCLE-001 — Implementation Plan

A new derived-analytics feature: a materialised Bitcoin halving-cycle overlay recomputed on the
periodic collector tick, plus a new keyset-paginated `/v1` read route. Phases 1–7 are additive — no
existing collection, provider, candle write path, cursor, or DTO changes. **Phase 8 (v0.6.0) is the
one exception: a deliberately breaking HTTP-surface refactor** that folds the two cycle endpoints
into `GET /v1/coins/{coin_id}/cycle-projection/{model}` plus a base-path discovery endpoint, still
with no migration, schema, or projection-math change. One new migration for the base feature (number
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

### Phase 7 — Point-in-time (`as_of`) reads on both endpoints (v0.5.0, Priority High)

Additive amendment: **no migration, no schema change, no cache, no new stored rows**. All new work
is request-time wiring around the three already-unit-tested pure functions. The methodology is
DDD/TDD-friendly because the projection math is untouched — only the request-time source-series
truncation and in-memory paginate/filter are new.

- **Param.** Add `as_of: Option<DateTime<Utc>>` to `ListCycleOverlayParams` in
  `src/api/cycle_overlay.rs`, deserialised exactly like `GetMetadataParams::as_of` in
  `src/api/metadata.rs` (RFC3339; an unparseable value is a `Query` rejection → HTTP 400 for free,
  REQ-CYCLE-079). Both `list_cycle_overlay` and `list_cycle_projection` already share
  `list_overlay_for_model`, so the branch is added once there. (REQ-CYCLE-070)
- **Branch in `list_overlay_for_model`.** When `params.as_of` is `None`, keep the existing SELECT
  against `cycle_overlay_points` unchanged (REQ-CYCLE-074). When `Some(as_of)`, take the on-the-fly
  path below instead of the table read. (REQ-CYCLE-072/074)
- **Request-time daily-series loader.** A new async helper mirroring `recompute_cycle_overlay`'s
  source logic (`src/collectors/cycle_overlay.rs`): read native `1d` `coin_candles` filtered by
  `ts <= as_of`; when none exist, fall back to the finer-interval aggregation — the SAME
  `DISTINCT ON ((ts AT TIME ZONE 'UTC')::date) … ORDER BY day, ts DESC` shape as
  `aggregate_daily_from_finer`, but with an added `AND ts <= $as_of` predicate. This MUST stay
  SQL-side one-row-per-day — never `fetch_all` the finer series (256Mi-pod OOM invariant,
  REQ-CYCLE-080). Recommend factoring the existing `recompute_cycle_overlay` source-building block
  into a shared `load_daily_series(pool, coin, vs, as_of: Option<..>)` so the tick path and the
  as-of path cannot drift. (REQ-CYCLE-071/080)
- **Compute.** Run `compute_overlay(daily)` for the real points, then `project_cycle_repeat(&daily,
  &points)` for `cycle-overlay` or `project_composite(&daily, &points, use_btc_anchors)` for
  `cycle-projection`, with `use_btc_anchors = (coin_id == "bitcoin" && vs_currency == "usd")`.
  These functions already anchor `today` at the latest series date and already emit zero projected
  points under `< CYCLE_DAYS` history, so the "as-of view only", at-or-after-latest, and
  insufficient-history behaviours (REQ-CYCLE-073/075/077) fall out for free — no new math.
  (REQ-CYCLE-072/077/081/082)
- **In-memory paginate + filter.** Apply the optional `cycle` filter, the
  `(cycle_number ASC, days_since_halving ASC)` ordering, the keyset cursor
  (`decode_keyset_cursor::<CycleOverlayKey>` for the start bound + `paginate_cycle_overlay` for the
  page/`next_cursor`), and the `limit` over the computed `Vec<OverlayPoint>` — the same contract as
  the SQL path, just applied to the in-memory vector. `vs_currency` still defaults to `usd`; an
  unknown/non-target coin computes an empty vec → 200 empty page (no `ensure_coin_exists`).
  (REQ-CYCLE-078)
- **OpenAPI.** Add the `as_of` query parameter (`type: string`, `format: date-time`) to both the
  `listCycleOverlay` and `listCycleProjection` operations in `api/crypto-collector.yaml`; extend the
  doc-parity test. (REQ-CYCLE-084)
- **@MX.** Mark the request-time as-of daily-series loader `@MX:WARN`/`@MX:REASON` (keep SQL-side
  aggregation; `ts <= as_of` is the point-in-time truncation).
- Gate: `cargo test` — pure/handler tests for the as-of branch (mid-history truncation and
  re-anchoring, before-all-data empty page, at-or-after-latest equals no-`as_of`,
  `< CYCLE_DAYS` → empty projection, pagination round-trip under fixed `as_of`, `cycle`+`as_of`
  compose, invalid `as_of` → 400); DB-backed as-of scenarios opt-in via
  `DATABASE_URL=... cargo test -- --ignored`.

### Phase 8 — Fold the two cycle endpoints into one `{model}` data route + discovery (v0.6.0, Priority High)

Breaking HTTP-surface refactor: **no migration, no schema change, no projection-math change, no
`cycle_overlay_points` content change**. All work is route/handler wiring around the already-shared
`list_overlay_for_model` and the already-existing `project_as_of_for_model` dispatch. The new risk is
the `{model}` validation boundary and the parity-test rewrite.

- **`ProjectionModel` enum (single source of truth, OR-CYCLE-9).** Add a small enum with variants
  `Replay`/`Composite`, a `TryFrom<&str>`/`FromStr` that maps unknown strings (including `"real"`) to
  `ApiError::BadRequest` (REQ-CYCLE-094/093), and an `as_projection_model_str()` returning
  `"replay"`/`"composite"` for the SQL bind. Reuse the same enum to build the discovery list so the
  set of valid models is declared once. Mark this enum + the validation step so a bad `{model}`
  becomes a 400 **before** dispatch — the existing `unreachable!()` in `project_as_of_for_model`
  (`src/api/cycle_overlay.rs:221`) then stays genuinely unreachable via the path.
- **Data handler.** Replace `list_cycle_overlay`/`list_cycle_projection` with one handler taking
  `Path((coin_id, model)): Path<(String, String)>` (or `Path<(String, ProjectionModel)>`), validating
  `model`, then calling the unchanged `list_overlay_for_model(state, coin_id, params, model.as_str())`.
  `ListCycleOverlayParams` (`src/api/cycle_overlay.rs:35`) — including `as_of` — carries over
  unchanged. (REQ-CYCLE-090/091/092)
- **Discovery handler.** New handler on the base path returning
  `Json(CycleProjectionModelsDto { models: vec![replay_meta, composite_meta] })`, where each entry is
  `{ id, description, has_confidence_bands }` (`replay` → `false`, `composite` → `true`). New DTO
  `CycleProjectionModelsDto` + `CycleProjectionModelDto` in `src/api/dto.rs`. (REQ-CYCLE-095/096/097)
- **Routes (`src/api/mod.rs:205-212`).** Remove the `/v1/coins/{coin_id}/cycle-overlay` route
  entirely (→ 404, REQ-CYCLE-098). Repoint `/v1/coins/{coin_id}/cycle-projection` (base) to the
  discovery handler, and add `/v1/coins/{coin_id}/cycle-projection/{model}` for the data handler. The
  base and `{model}` paths differ by a segment, so there is no axum route-ordering conflict; both sit
  under the existing `/v1/coins/{coin_id}/…` param family alongside `candles`.
- **OpenAPI (`api/crypto-collector.yaml`).** Delete the `/coins/{coin_id}/cycle-overlay` path (~L431)
  and the old data body of `/coins/{coin_id}/cycle-projection` (~L484). Add
  `/coins/{coin_id}/cycle-projection/{model}` with a `model` path parameter (`enum: [replay,
  composite]`), the carried-over `vs_currency`/`cycle`/`cursor`/`limit`/`cycle_as_of` parameters, and
  the `CycleOverlayPointPage` response; add the base `/coins/{coin_id}/cycle-projection` discovery
  operation with a new `CycleProjectionModels` schema. (REQ-CYCLE-099)
- **Doc-parity tests (`src/api/mod.rs`).** Update `openapi_yaml_contains_all_operation_ids` (`:389`)
  to drop `listCycleOverlay` and add the discovery operationId (recommended
  `listCycleProjectionModels`, keeping `listCycleProjection` for the data op — OR-CYCLE-7). Rewrite
  `openapi_yaml_documents_as_of_on_both_cycle_endpoints` (`:424`): `as_of` now lives on the single
  `{model}` data path (not two endpoints) and MUST NOT be on the discovery path — assert it on
  `/coins/{coin_id}/cycle-projection/{model}` and assert its absence on the bare discovery path.
  `openapi_yaml_contains_key_schemas` (`:456`) gains the discovery schema name.
- **@MX.** `@MX:ANCHOR` on `list_overlay_for_model` (single fan-in for the `{model}` dispatch);
  `@MX:WARN`/`@MX:REASON` on the `{model}` validation → dispatch boundary (unvalidated `{model}` →
  `unreachable!()` panic → 500 instead of the required 400).
- Gate: `cargo test` — handler tests for `replay`/`composite` returning the same page shape as the
  old endpoints, unknown `{model}` and `.../real` → 400, discovery two-entry payload with correct
  `has_confidence_bands` and no `real`, old `.../cycle-overlay` → 404, base `.../cycle-projection`
  returns discovery (not data), `as_of` still works per-model, and the updated OpenAPI parity tests.
  DB-backed data scenarios opt-in via `DATABASE_URL=... cargo test -- --ignored`.

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
- **As-of reuses the precompute source logic, not the precompute table (v0.5.0).** The precomputed
  `cycle_overlay_points` table only ever holds the projection from the *current* latest data, so an
  arbitrary past cutoff cannot be served from it — the as-of path re-runs the pure functions over a
  daily series truncated to `ts <= as_of`. The only genuinely new code is (a) one extra SQL
  predicate on the daily-series query and (b) an in-memory version of the paginate/filter that the
  SQL path does in the database; the projection math and the cursor/DTO contract are unchanged. The
  same-series truncation means `today` inside the pure functions becomes "latest candle `<= as_of`",
  which is precisely the point-in-time anchor REQ-CYCLE-073 requires, with no special-casing.
- **Endpoint consolidation is a pure surface reshape (v0.6.0).** The two handlers already funnel into
  one `list_overlay_for_model(..., projected_model)`; v0.6.0 only moves the `projected_model` string
  from a hardcoded handler argument to a validated `{model}` path segment. Because the shared impl,
  the `real`-baseline SQL filter, the DTO, the cursor, and the `as_of` branch are all untouched, the
  data endpoint's behaviour is identical to the pre-refactor endpoints for the same model — the change
  is entirely in routing, path validation, and the new discovery handler. The `ProjectionModel` enum
  is the one place the two valid model strings live, shared by the data-path validation and the
  discovery list so they cannot drift.

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
- **As-of OOM regression (v0.5.0).** The greatest as-of risk is loading the finer-interval history
  into the app to truncate it in Rust — a deep 5m backfill is ~1M rows and OOM-kills the 256Mi pod.
  The `ts <= as_of` filter MUST live inside the `DISTINCT ON` daily aggregation SQL (one row per
  day), never applied post-`fetch_all`. Guarded by an `@MX:WARN` on the shared loader and mirrored
  from the existing `aggregate_daily_from_finer` invariant (REQ-CYCLE-080).
- **As-of compute cost per request.** Unlike the no-`as_of` path (a single indexed SELECT), an
  as-of request recomputes the overlay/projection on the fly. Daily granularity keeps this modest
  (a decade ≈ 3–4k rows; the pure functions are O(n)), and there is no cache by design
  (REQ-CYCLE-072). If as-of traffic ever dominates, caching is a later optimisation, explicitly out
  of scope here.
- **As-of / no-as-of divergence.** The tick-time precompute and the as-of path must build the daily
  series identically (native `1d` first, else widest-coverage finer aggregation) or an
  `as_of >= latest` request would not equal the no-`as_of` result (REQ-CYCLE-075). Mitigation:
  factor the source-building into one shared `load_daily_series(...)` used by both, and cover the
  equality with Scenario 21.
- **`{model}` reaching `unreachable!()` (v0.6.0).** The single greatest v0.6.0 risk: if `{model}`
  from the path is passed to `list_overlay_for_model`/`project_as_of_for_model` without being
  validated, an unknown value (or `"real"`) hits the `match` fallthrough `unreachable!()` and panics
  → HTTP 500 instead of the required HTTP 400. Mitigation: validate via the `ProjectionModel` enum in
  the handler *before* dispatch (REQ-CYCLE-094), an `@MX:WARN` on the dispatch boundary, and explicit
  400 tests for unknown `{model}` and `.../real` (Scenario 30).
- **Breaking change lands without a client shim (v0.6.0).** Removing `.../cycle-overlay` and
  reshaping the base `.../cycle-projection` is intentional (D12) but silently breaks any live client.
  This is accepted per the confirmed decision; mitigation is limited to documenting the migration in
  the OpenAPI descriptions and the CHANGELOG at sync — no alias is added (REQ-CYCLE-098, Exclusions).
- **Doc-parity test drift (v0.6.0).** The `openapi_yaml_documents_as_of_on_both_cycle_endpoints` test
  hardcodes the two old path strings and the "both endpoints" assumption; if it is not rewritten it
  will pass against a stale document or fail spuriously. It must be updated to assert `as_of` on the
  single `{model}` data path and its absence on the discovery path (REQ-CYCLE-099).

## Dependencies / Sequencing

- Phase 1 (schema) precedes the recompute (Phase 4) and the read route (Phase 5).
- Phases 2–3 (pure transform) are independently unit-testable and land before wiring.
- Phase 4 (recompute) and Phase 5 (route) both depend on Phase 1's table + model; they are
  otherwise independent and can proceed in parallel after Phase 3.
- Phase 6 (OpenAPI + full suite) closes the loop.
- Phase 7 (v0.5.0 as-of reads) depends only on the Phase 2/3 pure functions and the Phase 5 route +
  cursor/DTO already being in place; it adds no migration and reuses the Phase 4 recompute's
  source-building logic (native `1d` → widest-coverage finer aggregation, now with a `ts <= as_of`
  predicate). It is otherwise independent and lands after the base feature is complete.
- Phase 8 (v0.6.0 endpoint fold) depends only on the Phase 5 route + the Phase 7 `as_of` branch
  already existing (both `replay` and `composite` already route through `list_overlay_for_model` and
  `project_as_of_for_model`). It adds no migration, no schema, and no projection math — only route
  registration, the `{model}` validation enum, the discovery handler/DTO, and the OpenAPI + parity
  test updates. It lands last and is the only phase that changes external HTTP behaviour in a
  breaking way.
- No dependency on other SPECs beyond the existing `coin_candles` schema (SPEC-DB-001 / SPEC-API-002),
  the periodic tick (SPEC-SCHED-001), and the `/v1` cursor/DTO/error conventions (SPEC-API-001).
  Optional reuse of SPEC-API-003 read-time `1d` aggregation is an Open Item (OR-CYCLE-4), not a
  dependency. The as-of daily loader (Phase 7) reuses the same SPEC-API-003 aggregation path.
