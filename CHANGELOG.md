# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Changed

- **SPEC-REFACTOR-001** — Phase 7: Batching & structural debt reduction
  (`src/providers/{mod,coinbase,kraken}.rs`, `src/collectors/{lease_worker,collection_queue,backfill,live_poller,cycle_overlay}.rs`,
  `src/db/{upserts,candles}.rs`, `src/api/{mod,quotes,metadata,coins,cursor,candles}.rs`,
  `src/models/interval.rs`, `src/models/query.rs`):
  a behavior-preserving consolidation of the duplication the project's idiomatic-Rust review
  identified as "the dominant idiomatic debt and the proven source of every confirmed drift bug",
  landed in six milestones with **exactly two intended behavior changes**:
  - **M1 — Provider trait capability-derived defaults (F-50)**: every `Provider` fetch method now
    has a capability-derived default body (`Err(NotSupported(..))`, or `Ok(vec![])` for the
    `search_coins`/`fetch_coin_tickers` pair); only `name()`/`supports()` remain mandatory.
    `coinbase.rs`/`kraken.rs` shed ~450 lines of dead `NotSupported` stubs; trait object-safety
    preserved (`Arc<dyn Provider>` unchanged).
  - **M2 — `chain_try` + per-provider pacing (F-53a, F-16 — intended change (a))**: one generic
    `chain_try<T>(chain, capability, registry, f)` replaces the four duplicated non-OHLC fallback
    loops (spot / spot_local / coin_metadata / coin_market), owning `HealthRegistry` bookkeeping.
    **Behavior change**: the pacer slot is now acquired for the provider that actually serves a
    fallback request, not the first capability-supporting member in the chain — fallback traffic
    is now honestly paced/charged to the serving provider. The two OHLC chains
    (`chain_fetch_ohlc{,_range}`) keep their distinct continue-on-empty semantics untouched.
  - **M3 — Shared lease-queue scaffold (F-53b)**: `collection_queue` and `backfill` now share one
    parameterized claim/heartbeat/complete/release scaffold; the heartbeat task is stopped via a
    `tokio::sync::watch` signal instead of `abort()`.
  - **M4 — Batched writes + NOTIFY policy (F-51 — intended change (b), F-52)**: a shared
    UNNEST-based batched candle upsert (generalized from `rollup::batched_upsert_candles`) now
    backs both the live-poll candles dispatch path and the backfill page-write path — live writes
    now commit per page instead of per row. **Behavior change**: the backfill path emits **no**
    `pg_notify` (historical rows no longer flood the WebSocket broadcast); the live-poll path is
    unchanged (one NOTIFY per event). The two UNNEST conflict policies remain distinct: the native
    write path uses an unconditional `DO UPDATE`, while the rollup path retains its
    `WHERE coin_candles.source LIKE 'rollup:%'` native-wins guard — the two are never conflated.
    `recompute_cycle_overlay` now batches its inserts via UNNEST per model group inside the
    existing single-transaction rebuild.
  - **M5 — API deduplication (F-53c, F-55)**: one shared `ensure_coin_exists`, one `concat!`
    -assembled `tracked_coins` column-list const (replacing 5 inlined copies), one generic
    `paginate<T, K: Serialize>` (replacing three near-duplicate paginators), the two dead
    `AppState` fields (`http_client`, `coingecko_base_url`) removed, and a shared
    `#[cfg(test)] AppState::test()` constructor replacing 6+ duplicated test-state builders.
  - **M6 — Domain typing (F-54, F-56)**: `SUPPORTED_INTERVALS` and `interval_to_seconds` are both
    removed and fold into a single `ApiInterval` enum covering the full fixed-duration interval
    vocabulary (API-facing + storage-only), with a **total** `secs()` (no `.expect`, no
    `Option`-panic path) and an `is_api_facing()` boundary predicate — the public API still
    returns 400 for a storage-only interval (e.g. `3m`), identical to the prior behavior. The
    `MarketQuery { market_id: 0 /* dummy */ }` sentinel (4 sites) is replaced by a keyed
    `CoinKeyed { coin_id, symbol } | MarketKeyed { market_id }` enum, making the dummy
    unrepresentable. `F-56`: provider chain type is now `Arc<[Arc<dyn Provider>]>`; dead
    `CgMarketItem.vs_currency` field removed; legacy `coingecko_days_to_interval` helper removed
    (tests migrated).

  **Operator-relevant consequence**: live candle writes now commit per page rather than per row
  and the prior per-candle write-duration histogram observation is superseded by the batched
  write path (no per-row timing sample); backfill writes are silent on the WebSocket channel.

  All 34 acceptance scenarios (AC-REFACTOR-010..083) pass. `cargo fmt --check` exit 0,
  `cargo clippy --all-targets --all-features -- -D warnings` exit 0, `cargo test` exit 0 (748
  non-DB-gated tests pass, 100 DB-gated tests remain `#[ignore]`d pending a live-Postgres
  `--test-threads=1` verification pass — this SPEC is held at `implemented`, not `completed`,
  per the SPEC-PROV-002/003 / SPEC-API-005 / SPEC-OBS-002 close pattern). No new dependency
  added; no f64 introduced for monetary values; config remains env-only.

### Fixed

- **SPEC-OBS-002** — Lifecycle, shutdown & observability integrity
  (`src/{metrics/mod,db/upserts,db/pool,db/mod,collectors/mod,collectors/live_poller,alarm/registry,alarm/reconciler,main,health/mod,config,listener,telemetry/mod,api/mod}.rs`,
  `tests/alarm_docs_parity.rs`):
  - **F-38/F-61 metric-name SSOT + operator-visible rename** (REQ-OBS-060/061): the
    persistence-latency histograms were *emitted* as `coin_quote_insert_duration_seconds` /
    `coin_candle_insert_duration_seconds` while being *described* under the canonical
    `quote_insert_duration_seconds` / `candle_insert_duration_seconds` names — a ghost-metric
    describe/emit mismatch. Fixed: both names are now shared `pub const`s
    (`QUOTE_INSERT_DURATION_SECONDS` / `CANDLE_INSERT_DURATION_SECONDS` in `src/metrics/mod.rs`)
    bound by both `describe_all()` and every emitter, structurally closing the drift; a new
    describe/emit parity test pins the contract. **Operator-visible**: external Grafana
    dashboards/alerts keyed on the old `coin_*`-prefixed series MUST be updated to the canonical
    names above.
  - **F-49 `tracked_markets` ghost removal** (REQ-OBS-062/074): a vestigial negative test
    assertion referencing the long-dropped `tracked_markets` table (removed by migration
    `0011_remove_markets.sql`) was removed; the `tracked_coins` gauge is unchanged.
  - **F-39/F-46 generic supervisor + capped backoff + relay supervision** (REQ-OBS-063/064/065):
    four near-duplicate `run_supervised_{live_poller,queue_worker,backfill_worker,reconciler}`
    functions are replaced by one generic `run_supervised(name, registry, shutdown, make_future)`
    with exponential backoff (1s initial, 30s cap, 60s healthy-run reset). The cross-replica
    relay listener's initial-connect failure previously returned permanently; it now retries
    under the same capped-backoff supervisor.
  - **F-41/F-47 bounded shutdown drain + airtight shutdown arms** (REQ-OBS-066/067/068): the
    shutdown drain previously had no timeout — a wedged worker future could block shutdown
    indefinitely. Fixed: `tokio::time::timeout(drain_secs, supervisor)` bounds the drain, with
    `pool.close()`/`telemetry::shutdown()` guaranteed on both the drained and timed-out paths;
    every shutdown-`select!` arm now breaks (rather than hot-spinning) when the shutdown watch
    sender is dropped without sending `true`. The pre-existing 15s endpoint-removal grace sleep
    and broadcast-before-drain ordering are preserved.
  - **F-43/F-44 exact readiness** (REQ-OBS-069/070): readiness previously could flip `ready`
    before the API listener actually bound, and the 2s readiness cache could serve a stale `200`
    for up to 2s into a shutdown. Fixed: `set_ready()` now follows `TcpListener::bind` + relay
    spawn (only `axum::serve` comes after); `check_readiness` consults the shutting-down flag
    before the cache fast-path, so shutdown-time reads are `503` immediately regardless of cache
    freshness.
  - **F-45 config diagnostics** (REQ-OBS-071/072): a present-but-unparseable env var previously
    silently fell back to its default. Fixed: unparseable values now emit a `tracing::warn!`
    naming the variable and fallback used; the pacer-cooldown variable (dangerous to mis-set)
    fails fast instead of silently defaulting.
  - **F-40 credential-safe DB connection** (REQ-OBS-073): the database connection was assembled
    via URL-string interpolation from `DB_HOST`/`DB_PORT`/`DB_NAME`/`DB_USERNAME`/`DB_PASSWORD`,
    corrupting passwords containing `@ / : # %` or spaces. Fixed: the connection is now built
    from `sqlx::postgres::PgConnectOptions` set field-by-field (never re-parsed as a URL); the
    `DATABASE_URL` override path is unchanged.
  - **F-42/F-48 alarm signal quality** (REQ-ALARM-080/081/082): the Critical
    `all_providers_down` alarm previously flipped on a single coin's failure among otherwise
    healthy sweeps. Fixed: it now raises only after the whole provider chain fails continuously
    for a sustained 180s window, and is not suppressed by a single mid-outage success.
    `observe_chain_records`'s doc comment was corrected to match its implementation (code is
    authoritative: it derives only the chain-outcome signal and does not touch the per-provider
    network-failure streak, which counts only `ProviderError::Network` failures per
    REQ-ALARM-020); two new parity tests in `tests/alarm_docs_parity.rs` pin the contract. The
    reconciler ticker now uses `MissedTickBehavior::Skip` (consistent with `live_poller`).
  - **F-49 dead-surface cleanup** (REQ-OBS-074): the duplicate `main.rs` copy of
    `HeaderExtractor` was removed (one canonical copy remains in `src/telemetry/mod.rs`,
    alongside `OtelMakeSpan` which was relocated there); the dead, unreachable
    `start_api_server` function was deleted.

  18 requirements-mapped acceptance criteria (AC-OBS-060..074, AC-ALARM-080/081/082) plus 2
  global gates (G1 quality gates, G2 rename operator-visibility) — all 20 PASS. `cargo test`
  exit 0 (lib 661→681, +20 tests), `cargo clippy --all-targets --all-features -- -D warnings`
  exit 0, `cargo fmt --check` exit 0. No new dependency added, no new endpoint, no new
  migration. Two optional DB-gated variants (kill-the-DB-then-start relay retry, live
  special-character-password connect) remain deferred to a live-Postgres verification pass —
  the non-DB-gated primary paths for both PASS.

- **SPEC-API-005** — API contract fixes, query bounds & schema truth
  (`src/api/{quotes,extract,coins,candles,cycle_overlay,websocket,mod}.rs`,
  `src/models/quote.rs`, `tests/{db_integration,migration_files}.rs`):
  - **F-29** (D1): `get_latest_quote` and `list_quotes` ignored `vs_currency` on read — every
    quote read implicitly assumed `usd`, silently returning wrong-currency rows for any other
    `vs_currency`. Fixed: both handlers now bind `vs_currency` (`.unwrap_or("usd")`, no
    allow-list — an unrecognised currency matches no rows, a 200 empty page, never a 400)
    (REQ-API-400/401).
  - **F-30** (D1/D6): quote reads had no upper-bound-on-staleness contract and no explicit
    window, violating the codebase's own partition-pruning invariant (coin_quotes is
    `PARTITION BY RANGE(ts)`, 48 monthly partitions). Fixed: `get_latest_quote` now 404s when
    the latest quote is older than 48h ("no *current* quote", not "no quote ever"); `list_quotes`
    defaults to a 48h trailing window anchored on `end` when supplied
    (`COALESCE($end, now()) - interval '48 hours'`) else `now()` — resolving D1/OR-API5-1 — and
    a duplicate-`ts`-across-currencies keyset row-loss bug is fixed alongside (REQ-API-402/403).
  - **F-31** (aggregation reachability): the candle-aggregation fallback path had no `end`
    bound, so a far-past window could be unreachable, and a cap-hit-but-empty page silently
    terminated pagination instead of continuing. Fixed: `list_candles` aggregation now carries
    an explicit `end`-bound and a cap-cursor sourced from the underlying bucket, so a
    cap-hit-but-empty page continues rather than dropping (REQ-API-405/406).
  - **F-32** (idempotent registration): concurrent duplicate coin-registration requests could
    race between the existence check and the insert, risking a 500 or a torn insert/enqueue.
    Fixed: registration now uses `ON CONFLICT` inside a single transaction — concurrent
    duplicates yield exactly one 201 + one 200, never a 500, and the insert + collection-queue
    enqueue are atomic (REQ-API-407/408).
  - **F-33/F-34** (uniform error bodies + search 503): rejected/malformed extractor input
    (body/query/path) produced inconsistent, non-JSON error shapes across handlers, and
    upstream pacer/credit exhaustion during search leaked as a generic 500. Fixed: new
    `ApiJson`/`ApiQuery`/`ApiPath` `#[derive(FromRequest)]` wrappers (chosen over
    `axum-extra::WithRejection` — no new dependency, D2) funnel every handler's rejection
    through a uniform JSON `ApiError` body (`From<{Json,Query,Path}Rejection>`); search now maps
    pacer-cooldown and credit-exhaustion upstream errors to 503, leaving a true empty result as
    200 (REQ-API-409/410/411).
  - **F-35** (`as_of` concurrency ceiling): the cycle-overlay `as_of` recompute path had no
    concurrency bound — the most plausible self-inflicted DoS vector, since each request
    re-runs `load_daily_series` + `compute_overlay` + projection over the full series. Fixed: a
    `tokio::sync::Semaphore` ceiling now bounds concurrent recomputes, released on drop
    (REQ-API-412).
  - **F-36** (WebSocket read loop): the WebSocket handler only wrote to clients (broadcast → 
    socket) and never read from the socket, so a client-initiated `Close` frame was never
    observed and the server-side stream task leaked. Fixed: `handle_stream` is now a
    bidirectional `select!` loop — it polls `socket.recv()` (client frames/Close/pong) alongside
    `rx.recv()` (broadcast payload) and a ping interval, terminating the task on `Close` or
    socket error (REQ-API-413).
  - **F-57/F-58** (schema truth): the `CoinCandle` `@MX:ANCHOR` still described the table as
    monthly-`RANGE`-partitioned after migration `0020_coin_candles_departition.sql` flattened it
    to a plain table, and `tests/db_integration.rs` retained stale scenarios asserting the
    removed `live_quotes` table. Fixed: the anchor now documents the flat-table btree+BRIN index
    contract (contrasted against the still-partitioned `CoinQuote`), and `db_integration.rs` was
    rewritten to 16 `#[ignore]` scenarios matching current schema — no more asserts against
    removed tables (REQ-API-414/415).
  - **F-59** (parameter-parity test): no test enforced that every OpenAPI-documented query
    parameter had a matching struct field (or vice versa) per operation, so a handler could
    silently drift from `api/crypto-collector.yaml`. Fixed:
    `openapi_query_params_have_matching_struct_fields` — verified RED once against a
    documented-but-unimplemented param, then GREEN (REQ-API-417); `all_migration_files_exist`
    (`tests/migration_files.rs`) renamed and extended to cover migrations 0001–0021
    (REQ-API-416).

  18 requirements-mapped acceptance criteria (AC-API-400..417) covering REQ-API-400..417, plus
  4 global ACs (G1 parameter-parity green, G2 ts-bound grep PASS, G3 db_integration full-suite
  DEFERRED, G4 fmt/clippy/test PASS). Sandbox-verifiable ACs (AC-API-409/410/411/412/413/414/
  416/417 + G1/G2/G4) are PASS with evidence — `cargo fmt --check` exit 0, `cargo clippy
  --all-targets --all-features -- -D warnings` exit 0, `cargo test` exit 0 (661 lib + 12
  model_serde + 21 migration_files + 8 alarm_docs_parity + 2 backtest_projection passed; 16
  db_integration `#[ignore]`d). 10 DB-gated ACs (AC-API-400/401/402/403/404/405/406/407/408/415
  + G3) **compile but were not executed** in this environment (no live Postgres available);
  deferred to a live-Postgres verification pass (`DATABASE_URL=... cargo test -- --ignored
  --test-threads=1`). No new endpoint, no new migration, no new dependency, no `f64` in any
  monetary path, keyset cursors stay opaque/decode-compatible (D10).

- **SPEC-PROV-003** — Provider data correctness & tier configuration
  (`src/config.rs`, `src/providers/{coingecko,binance,mod}.rs`,
  `migrations/0021_coingecko_range_interval_canonicalise.sql`,
  `tests/{migration_files,db_integration}.rs`):
  - **F-20** (High): the CoinGecko `/ohlc/range` path stamped the raw API param strings
    `"daily"`/`"hourly"` as the persisted `coin_candles.interval`, invisible to
    `candles_agg::interval_to_seconds`, so range-fetched candles silently dropped out of
    rollup/aggregation. Fixed: `coingecko_range_snap_interval` now returns a
    `(param, canonical)` tuple — the API keeps `"daily"`/`"hourly"`, the persisted stamp is
    always `"1d"`/`"1h"` — mirroring the existing Bitstamp convention. A collision-safe
    migration (`0021_coingecko_range_interval_canonicalise.sql`, DELETE-shadowed-then-UPDATE,
    no-op on already-canonical rows) rewrites historical non-canonical rows in place.
  - **F-21** (High): paid CoinGecko tiers `analyst`/`lite`/`enterprise` sent the *demo*
    `x-cg-demo-api-key` header against the *demo* host while the range capability was
    (incorrectly) enabled for them — three tier decisions lived in two files with two
    divergent tier sets. Fixed with a typed `Tier` enum (`Demo`/`Analyst`/`Lite`/`Pro`/
    `Enterprise`) whose `is_paid()` is the single authority driving the API-key header, the
    default base URL, and the `OhlcRange` capability; unknown `COINGECKO_TIER` values now
    fail fast at startup, naming the offending value.
  - **F-22** (Medium): Binance `fetch_spot` stored a single 1-minute kline's volume in the
    `volume_24h` field (~3 orders of magnitude undercount) and derived price from the kline
    close. Fixed: price (`lastPrice`), `volume_24h`, and bid/ask now all come from
    `GET /api/v3/ticker/24hr`, routed through the shared `transport::paced()`/`get_json()`
    frame — never a kline.
  - **F-23** (Medium): `Decimal::from_str` cannot parse scientific notation (e.g.
    `"1.23e-5"`), so one exotic upstream number poisoned an entire page. Fixed:
    `decimal_from_number` — the single Decimal-only monetary parse core every provider
    `serde_json::Number` conversion routes through — falls back to
    `Decimal::from_scientific` (never `f64`, REQ-PROV-012). Optional fields that still fail
    to parse degrade to `None` with a `warn!` log; required fields hard-fail.
  - **F-24** (Medium): derivatives-ticker lookup matched by case-insensitive prefix, so a
    query for `"BTC"` could bind `"BTCDOM"`, `"BTCUP"`, or `"BTCST"` instead of the intended
    contract. Fixed with `select_deriv_ticker` — boundary-aware symbol matching (exact,
    separator, or quote-currency boundary), preference for the queried venue, and a
    deterministic tie-break on highest open interest (never upstream response order).
  - **F-25** (Low): Binance `fetch_ohlc`'s `limit` was computed from the raw requested
    interval rather than the snapped one, so a between-band interval could request the wrong
    candle count. Fixed: `kline_limit` now divides by the snapped interval.
  - **F-26** (Low): `chain_fetch_ohlc` reported the generic "empty provider chain" message
    for a non-empty chain where every member lacked the capability. Fixed with a new
    `ProviderError::NoCapableProvider(Capability)` variant naming the missing capability;
    the true empty-chain case is unchanged.
  - **F-27** (Low): normalization degradations (unparseable `max_supply`, missing
    `last_updated`) were silently swallowed. Fixed: both now degrade gracefully
    (`max_supply` → `None`, `last_updated` → `Utc::now()`) with a `warn!` log recording the
    original value — behavior-preserving, observability-only.
  - **F-28** (Low): search/tickers/derivatives normalization paths deep-cloned
    `serde_json::Value` arrays. Fixed: iteration is now by-reference throughout
    `coingecko.rs` (verified via `grep -c '.as_array().cloned()'` == 0).

  11 requirements-mapped acceptance criteria (AC-PROV-065/067/068/072/074/076/077/079/080/081
  + the quality gate AC-PROV-QG), covering REQ-PROV-065..081. Sandbox-verifiable ACs (pure
  `coingecko_range_snap_interval`/`decimal_from_number`/`select_deriv_ticker`/`kline_limit`
  cores, tier matrix + fail-fast + wiremock header checks, migration-file shape test, `fmt`/
  `clippy -D warnings`, 652 tests passed) are PASS with evidence. 4 net-new DB-gated tests
  (`#[ignore]`, `--test-threads=1`) — three exercising the shipped `0021` migration body
  (no-op on canonical rows, idempotent daily→1d rewrite, shadowed-duplicate drop without PK
  violation) and one exercising the Binance 24hr-ticker spot path end-to-end through the
  pacer — were **not executed** in this environment (no live Postgres available); deferred to
  a live-Postgres verification pass. No new migration beyond `0021`, no new dependency, no
  `f64` in any monetary path, `candles_agg::interval_to_seconds` (the canonical-stamp
  vocabulary anchor) unchanged.

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
