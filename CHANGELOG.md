# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Fixed

- **SPEC-CANDLE-002** — Materializer & projection data integrity hardening
  (`src/collectors/rollup.rs`, `src/collectors/cycle_projection.rs`):
  - **F-07** (High): the rollup reconcile path (`incremental_recompute_target`) could delete or
    overwrite native provider `coin_candles` rows it did not own. Fixed with a three-layer
    defense: the `previously_materialized` SELECT and the per-`ts` DELETE now carry
    `AND source LIKE 'rollup:%'`, and `batched_upsert_candles` gates its `ON CONFLICT DO UPDATE`
    on the existing row being a rollup row (native-wins collision policy) so a colliding native
    row is never overwritten.
  - **F-08** (Medium): the incremental recompute was forward-only and never repaired rollup
    history behind a deep source backfill. Fixed with a query-derived low-watermark comparison
    (`MIN(ts)` on source vs. earliest materialized rollup bucket, no new migration/column) that
    triggers a bounded, week-aligned backward-repair pass reusing the existing memory-bounded
    window walk; the trigger is target-interval-aware (day bucket for `1d`, week bucket for
    `1w`) so the pass is idempotent and self-terminating.
  - **F-09** (Low): the cycle-projection compute path (`project_composite`) panicked calling
    `log10` on a non-positive stored close or `current_price`. Fixed with a guard on
    `current_price <= 0` (evaluated before the fit, since `current_price` is derived from the
    unfiltered series) plus an interior `close <= 0` filter feeding `fit_model`, both degrading
    gracefully to an empty result instead of panicking. Backtest-locked projection constants
    (`tests/backtest_projection.rs`) are unchanged.

  4 acceptance criteria (AC-CANDLE-050, AC-CANDLE-052, AC-CANDLE-055, AC-CANDLE-058) plus the
  quality gate (AC-CANDLE-QG) verified: offline `cargo test` / `clippy -D warnings` / `fmt --check`
  green, 4 DB-gated integration tests green against live PostgreSQL 16
  (`--test-threads=1`), `tests/backtest_projection.rs` passes unchanged. No new migration, no new
  dependency.
