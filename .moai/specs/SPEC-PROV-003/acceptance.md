# SPEC-PROV-003 — Acceptance Criteria

Every criterion is testable. Development mode is **TDD** (brownfield): each RED-first test
MUST fail against current `main` before the fix. Pure and wiremock tests run under plain
`cargo test`. DB-gated tests follow the project convention (`#[ignore]` + `DATABASE_URL`, run
with `--test-threads=1` — the DB-gated suite shares a global claim queue, per CLAUDE.md
Integration Tests). AC → REQ traceability is stated per scenario; **`acceptance.md` is the
SSOT for AC count**.

---

## Top-line Acceptance Criteria (must-pass)

1. **(AC-PROV-068)** With `COINGECKO_TIER=analyst`, requests carry `x-cg-pro-api-key` against
   `pro-api.coingecko.com` and the range capability is enabled (wiremock-verified header +
   base URL).
2. **(AC-PROV-065)** No code path can persist a `coin_candles` row whose `interval` is absent
   from the canonical `candles_agg::interval_to_seconds` vocabulary table.
3. **(AC-PROV-QG)** All quality gates green: `cargo test`, `cargo clippy --all-targets
   --all-features -- -D warnings`, `cargo fmt --check`.

---

## Scenario 1 — Canonical interval stamps + cross-module vocabulary (F-20)

**Covers:** REQ-PROV-065, REQ-PROV-066 · **AC-PROV-065** · wiremock + pure (cross-module)

- **Given** a CoinGecko range fetch whose desired interval snaps to daily or hourly,
- **When** `/ohlc/range` is requested and candles are normalised,
- **Then** the outbound request's `interval` query param is `"daily"` (or `"hourly"`) — the
  API param is unchanged (REQ-PROV-065),
- **And** every returned candle is stamped with the canonical `"1d"` (or `"1h"`), NOT
  `"daily"`/`"hourly"` (REQ-PROV-065; the updated wiremock test at `coingecko.rs:1435`
  asserts `c.interval == "1d"`),
- **And** (cross-module) the stamped interval `"1d"`/`"1h"` resolves through
  `candles_agg::interval_to_seconds` to `Some(86_400)`/`Some(3_600)` — the range-path stamp
  is a member of the canonical vocabulary (REQ-PROV-066),
- **And** no provider stamp path can produce an interval absent from `interval_to_seconds`
  (the cross-module test ties the producer and consumer together).

## Scenario 2 — Interval-stamp cleanup migration (F-20 migration)

**Covers:** REQ-PROV-067 · **AC-PROV-067** · migration-file test (no DB) + DB-gated (informational)

- **Given** the new `migrations/0021_coingecko_range_interval_canonicalise.sql`,
- **When** the migration file is inspected (no-DB `tests/migration_files` group),
- **Then** it contains the idempotent, **collision-safe** canonicalisation (the
  DELETE-shadowed-duplicate-then-UPDATE form: it rewrites `daily`→`1d` / `hourly`→`1h`, and on
  a PK collision drops the shadowed non-canonical duplicate rather than raising a
  unique-violation) — NOT the bare single `UPDATE` (REQ-PROV-067),
- **And** (DB-gated, informational) applied against a DB with zero `daily`/`hourly` rows it
  changes nothing (no-op), and against a DB seeded with a `daily` row it rewrites that row to
  `1d` (idempotent — a second run changes nothing),
- **And** (DB-gated, informational) applied against a DB seeded with BOTH a `daily` row and its
  canonical `1d` twin at the same `(coin_id, vs_currency, ts)` it drops the `daily` duplicate
  and completes WITHOUT a unique-violation (never startup-fatal),
- **And** the `SELECT DISTINCT interval FROM coin_candles` verification is treated as
  informational only, never a gate.

## Scenario 3 — Typed tier: analyst end-to-end + fail-fast on unknown (F-21)

**Covers:** REQ-PROV-068, REQ-PROV-069, REQ-PROV-070, REQ-PROV-071 · **AC-PROV-068** · pure matrix + wiremock

- **Given** the `Tier` enum parsed from `COINGECKO_TIER`,
- **When** the tier matrix is exercised for each tier,
- **Then** analyst / lite / enterprise / pro each yield header `x-cg-pro-api-key` + default
  base URL `https://pro-api.coingecko.com` + `supports_ohlc_range() == true`
  (REQ-PROV-070/071),
- **And** demo yields header `x-cg-demo-api-key` + default base URL
  `https://api.coingecko.com` + `supports_ohlc_range() == false` (REQ-PROV-070/071),
- **And** an unknown tier value (e.g. `platinum`) fails startup / parse with a clear error
  naming the value — no silent default (REQ-PROV-069),
- **And** where `COINGECKO_BASE_URL` is set it overrides the `is_paid()` default verbatim
  (REQ-PROV-070),
- **And** (wiremock, top-line #1) with `COINGECKO_TIER=analyst` an outbound request carries
  the `x-cg-pro-api-key` header against the pro host (REQ-PROV-068/070).

## Scenario 4 — Honest Binance spot volume from the 24hr ticker (F-22)

**Covers:** REQ-PROV-072, REQ-PROV-073 · **AC-PROV-072** · wiremock

- **Given** a stubbed Binance `GET /api/v3/ticker/24hr` returning a known `lastPrice`, a known
  24-hour `volume`, plus bid/ask fields,
- **When** `fetch_spot` is invoked,
- **Then** the returned quote's `price` equals the ticker's `lastPrice` field — the spot price
  source is the 24hr ticker's `lastPrice`, NOT the 1m-kline close; the price semantics are
  pinned, not silently changed (REQ-PROV-072),
- **And** the returned quote's `volume_24h` equals the ticker's `volume` field (NOT a 1m
  kline volume), routed through `transport::paced()`/`get_json()` (REQ-PROV-072),
- **And** the quote's bid and ask are populated from the same payload (REQ-PROV-072),
- **And** no code path sets `volume_24h` from a single 1-minute kline (REQ-PROV-073).

## Scenario 5 — Scientific-notation Decimal + optional-field degradation (F-23)

**Covers:** REQ-PROV-074, REQ-PROV-075 · **AC-PROV-074** · pure

- **Given** `decimal_from_number` and the item normalizer,
- **When** a plain (`123.45`), a high-precision (`0.00000000001234`), and a
  scientific-notation (`1.234e-11`) `serde_json::Number` are parsed,
- **Then** each parses **exactly** to `Decimal` (the scientific value via
  `Decimal::from_scientific`), with no `f64` on any path (REQ-PROV-074),
- **And** an unparseable **optional** field degrades to `None` with a `warn!` — the item and
  its containing page survive (REQ-PROV-075),
- **And** an unparseable **required** monetary field still fails the item loudly
  (REQ-PROV-075).

## Scenario 6 — Logged degradations align optional-field strictness (F-27)

**Covers:** REQ-PROV-076 · **AC-PROV-076** · pure / behavior-preserving

- **Given** the `last_updated` → `Utc::now()` fallback and the `max_supply` `.ok()` swallow,
- **When** a malformed `last_updated` or `max_supply` is normalised,
- **Then** the degradation is preserved (the item survives) AND a `warn!`/`debug!` is emitted
  (REQ-PROV-076),
- **And** the degradation **target** is unchanged: `last_updated` deliberately retains its
  `Utc::now()` fallback (it is NOT converted to `None`) and `max_supply` keeps its
  `.ok()`→`None` — "aligned one way" means every optional degradation now degrades-and-logs,
  NOT that every target becomes `None` (REQ-PROV-076),
- **And** optional-field strictness is aligned one way, consistent with Scenario 5
  (unparseable optional → degrade-and-log, not silent).

## Scenario 7 — Boundary-aware, venue-preferring, deterministic derivatives match (F-24)

**Covers:** REQ-PROV-077, REQ-PROV-078 · **AC-PROV-077** · pure

- **Given** a `/derivatives/tickers` payload containing `BTCDOM`, `BTCUP`, `BTC-PERP` on two
  venues, and `BTCUSDT`,
- **When** `fetch_derivatives` matches base `BTC`,
- **Then** it matches `BTC-PERP`/`BTCUSDT` (boundary) but does NOT match `BTCDOM`/`BTCUP`
  (REQ-PROV-077),
- **And** when `market.venue` is present the ticker from that venue is preferred
  (REQ-PROV-078),
- **And** among remaining candidates the pick is deterministic (e.g. highest open interest),
  never dependent on upstream response order (REQ-PROV-078).

## Scenario 8 — Binance snapped-limit lookback (F-25)

**Covers:** REQ-PROV-079 · **AC-PROV-079** · pure

- **Given** a between-band `interval_secs` that snaps to a different kline interval,
- **When** `fetch_ohlc` computes its request `limit`,
- **Then** the `limit` divides by the **snapped** interval seconds (the snap returns
  `(secs, name)`), not the raw `interval_secs` (REQ-PROV-079).

## Scenario 9 — Non-empty all-unsupported chain error label (F-26)

**Covers:** REQ-PROV-080 · **AC-PROV-080** · pure

- **Given** a **non-empty** provider chain whose every member returns `Unsupported` for the
  `Ohlc` capability,
- **When** `chain_fetch_ohlc` returns its error,
- **Then** the error reflects "no capable provider" (e.g. `NoCapableProvider(Capability)` or a
  value synthesized from the attempt records) and does NOT carry the `"empty provider chain"`
  label (REQ-PROV-080),
- **And** the empty-chain case still reports genuinely empty (existing behavior preserved).

## Scenario 10 — Typed / by-reference DTO iteration (F-28)

**Covers:** REQ-PROV-081 · **AC-PROV-081** · grep + wiremock (behavior-preservation)

- **Given** the search, tickers, and derivatives paths,
- **When** the source is inspected,
- **Then** `grep -n '.as_array().cloned()' src/providers/coingecko.rs` finds no deep-clone of
  an array before read-only iteration — iteration is by reference or via typed DTOs
  (REQ-PROV-081),
- **And** the existing search/tickers/derivatives wiremock tests still pass unchanged
  (behavior preserved — craft-only refactor).

---

## Quality Gate (AC-PROV-QG) — must-pass

| Gate | Command | Pass condition |
|------|---------|----------------|
| Format | `cargo fmt --check` | exit 0 |
| Lint | `cargo clippy --all-targets --all-features -- -D warnings` | exit 0 |
| Test | `cargo test` | all pass; 0 failed |
| No new dependency | `git diff --stat Cargo.toml Cargo.lock` | empty |
| No new f64 in monetary path | `git diff \| grep '^+' \| grep -w f64` | no new `f64` |
| Provider trait surface unchanged | `git diff src/providers/mod.rs` (trait signatures) | no trait signature change |
| Vocabulary anchor untouched | `git diff src/api/candles_agg.rs` | `interval_to_seconds` unchanged |

---

## Definition of Done

- [ ] REQ-PROV-065/066 — range candles stamped canonical `"1d"`/`"1h"`; cross-module test ties
  the stamp to `interval_to_seconds`; the `daily` wiremock assertion updated (AC-PROV-065).
- [ ] REQ-PROV-067 — `migrations/0021_*.sql` present, idempotent guarded cleanup; binary
  rebuilt (AC-PROV-067).
- [ ] REQ-PROV-068/069/070/071 — `Tier` enum, fail-fast on unknown, `is_paid()` drives
  header + base URL + capability from one place; analyst→pro header+host+range wiremock
  (AC-PROV-068, top-line #1).
- [ ] REQ-PROV-072/073 — Binance spot volume + bid/ask from `/api/v3/ticker/24hr` via the
  shared helpers; no 1m volume in a 24h field (AC-PROV-072).
- [ ] REQ-PROV-074/075 — scientific-notation Decimal via `from_scientific` (no `f64`);
  optional→None+warn, required hard-fail (AC-PROV-074).
- [ ] REQ-PROV-076 — logged degradations for `last_updated`/`max_supply`; strictness aligned
  (AC-PROV-076).
- [ ] REQ-PROV-077/078 — boundary-aware + venue-preferring + deterministic derivatives match
  (AC-PROV-077).
- [ ] REQ-PROV-079 — snapped-limit division (AC-PROV-079).
- [ ] REQ-PROV-080 — distinct non-empty all-unsupported chain error (AC-PROV-080).
- [ ] REQ-PROV-081 — typed/by-reference DTO iteration, behavior preserved (AC-PROV-081).
- [ ] @MX tags placed: `@MX:ANCHOR` on the range stamp path, `Tier` authority, and
  `decimal_from_number`; `@MX:NOTE` on `fetch_spot` and `fetch_derivatives`.
- [ ] AC-PROV-QG — all quality gates green.

**AC count: 11** (AC-PROV-065, 067, 068, 072, 074, 076, 077, 079, 080, 081 + AC-PROV-QG).
