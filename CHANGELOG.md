# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Fixed

- **SPEC-PROV-002** — Provider transport hardening & pacer compliance
  (`src/providers/{transport(new),coingecko,binance,bitstamp,mod}.rs`, `src/pacer/mod.rs`,
  `src/config.rs`, `src/main.rs`):
  - **F-10** (High): `search_coins`/`fetch_coin_tickers` bypassed the pacer entirely and
    swallowed HTTP 429 without ever signalling cooldown, so a burst against these
    user-facing endpoints could trip the very 429s the pacer exists to prevent while the
    fleet never backed off. Fixed: both routes now go through the same throttle +
    `acquire_slot` prelude as every other provider method, and a 429 calls
    `signal_cooldown` before the result degrades to empty (`Ok(vec![])`, degradation now
    covers all upstream error kinds).
  - **F-11** (High): no provider `reqwest::Client` (CoinGecko, Binance, Bitstamp) had a
    total-request or connect timeout, so a black-holed upstream could hang a worker
    indefinitely after the pacer already charged a credit. Fixed with a single shared
    `transport::build_client()` constructor applying `.timeout()` (default 30 s) and
    `.connect_timeout()` (default 10 s), env-tunable via `PROVIDER_HTTP_TIMEOUT_SECS` /
    `PROVIDER_HTTP_CONNECT_TIMEOUT_SECS` (zero/unparseable guarded back to the default —
    a client can never be built unbounded).
  - **F-12** (Medium): `ProviderError::is_transient` classified every `Http{..}` status —
    including permanent 4xx like 401/404 — as transient, so SPEC-SCHED-001 worker retry
    logic spun on permanent failures. Fixed: transient only for `408 | 425 | 429 |
    500..=599`; `RateLimited`/`Network` unchanged.
  - **F-13** (Medium): `acquire_slot` silently clamped its reserved wait to 60 s, so under
    backlog a request could fire before its own reservation — converting overload into
    exactly the burst the pacer prevents. Fixed: the full computed wait is honoured (no
    truncation); backlog beyond the former 60 s ceiling is now surfaced via `warn!` +
    a new `pacer_backlog_wait_exceeded_total{provider}` counter (observability only, no
    behavior change).
  - **F-14** (Medium, the proven drift source): the request/429/parse scaffolding was
    duplicated across ~10 endpoint methods in `coingecko.rs`/`binance.rs` — the exact drift
    that produced F-10. Extracted into two shared helpers in a new `src/providers/transport.rs`:
    `paced()` (throttle + `acquire_slot` prelude + 429→`signal_cooldown` postlude) and
    `get_json()` (429→`RateLimited`, non-success→`Http{status,body}`, decode→`Parse`). All
    CoinGecko/Binance/Bitstamp endpoints migrated onto these mechanically; behavior
    preserved under existing wiremock coverage.
  - **F-15** (Medium): nothing verified at startup that every provider chain member had an
    `upstream_request_pacer` row — a missing row surfaced only as a runtime error on the
    first fetch. Fixed: a `missing_pacer_rows()` check runs after migrations succeed and
    before readiness flips, failing startup with a message naming the missing member(s)
    (DB-down-at-startup resilience preserved — the check is unreachable until migrations
    report success).
  - **F-17** (Low): `signal_cooldown` set `cooldown_until` unconditionally, so a later,
    shorter signal could truncate an earlier, longer one. Fixed with
    `GREATEST(COALESCE(cooldown_until, 'epoch'), $2)` — monotonic, never shortens.
  - **F-18** (Low): the blocked-path fallback in `acquire_slot` mislabeled a lapsed-block
    race (row present, block already lapsed) as `NotFound`. Fixed with a new
    `AcquireSlotError::Contended` variant, extracted into a pure `classify_blocked` core;
    `NotFound` is now reserved for a genuinely-absent row.
  - **F-19** (Informational): no provider client sent a `User-Agent`. Fixed — the shared
    `build_client()` attaches `crypto-collector/<CARGO_PKG_VERSION>` to all three clients.

  15 requirements (REQ-PROV-050..064) across 6 scenarios (AC-PROV-050/053/055/058/060/063)
  plus the quality gate (AC-PROV-QG). Sandbox-verifiable ACs (pure `sleep_plan` /
  `is_transient` / `classify_blocked` cores, timeout wiremock test, migration onto the
  shared helpers, no new dependency, `fmt`/`clippy -D warnings`) are PASS with evidence.
  4 net-new DB-gated tests (`#[ignore]`, `--test-threads=1`) covering the slot/cooldown
  advance (F-10), monotonic cooldown (F-17), `Contended`/`NotFound` DB half (F-18), and
  startup pacer-row validation (F-15) were **not executed** in this environment (no live
  Postgres available) — deferred to a live-Postgres verification pass. No new migration,
  no new dependency, no `f64` in any monetary path, `Provider` trait public surface
  unchanged.

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
