---
id: SPEC-PROV-003
title: "Provider Data Correctness & Tier Configuration"
version: "0.1.0"
status: implemented
created: 2026-07-24
updated: 2026-07-24
author: manager-spec
priority: High
phase: "v0.4.0"
module: "src/providers"
lifecycle: spec-anchored
tags: "providers, coingecko, binance, tier, interval, decimal, derivatives, data-correctness"
issue_number: null
related_specs: [SPEC-PROV-001, SPEC-PROV-002, SPEC-API-003, SPEC-DB-001]
tier: M
---

# SPEC-PROV-003 — Provider Data Correctness & Tier Configuration

Behavioral-correctness hardening of the provider **data layer**: the values providers
normalise, stamp, and persist, and the tier configuration that governs which CoinGecko host
and header (and capability) each request uses. Nine findings (**F-20 … F-28**, Category D of
`research/idiomatic-rust.md` §6, lines 182–222) are addressed. Two are High-severity
behavioral defects that silently corrupt data (F-20 non-canonical interval stamps, F-21 paid
tiers using the demo host/header); the rest range from a user-visible volume misreport (F-22)
to craft-only clones (F-28).

Findings addressed: **F-20** (High — CoinGecko `/ohlc/range` stamps non-canonical
`"daily"`/`"hourly"` interval strings invisible to `interval_to_seconds`), **F-21** (High —
paid tiers `analyst`/`lite`/`enterprise` get the demo header + demo base URL), **F-22**
(Medium — Binance spot stores a 1-minute kline volume in `volume_24h`), **F-23** (Medium —
`Decimal::from_str` cannot parse scientific notation, one exotic number poisons a whole page),
**F-24** (Medium — derivatives lookup binds the wrong ticker via case-insensitive prefix
match), **F-25** (Low — Binance `fetch_ohlc` limit computed from the raw not the snapped
interval), **F-26** (Low — `chain_fetch_ohlc` reports "empty provider chain" for a non-empty
all-unsupported chain), **F-27** (Low — silent normalization degradations), **F-28** (Low —
`serde_json::Value` array deep-clones in search/tickers/derivatives paths).

This is **Phase 4** of the 7-phase review-driven improvement roadmap
(`research/idiomatic-rust.md` §7, line 383).

## Prerequisites / Sequencing

**Phase 4 depends on Phase 3** ([SPEC-PROV-002](../SPEC-PROV-002/spec.md), `status:
implemented`, feat commit `380150f`; `research/idiomatic-rust.md` §8 line 397). The shared
`transport::paced()` / `transport::get_json()` request helpers and the shared
`transport::build_client()` constructor introduced by Phase 3 are the landing site for every
Phase-4 endpoint change: the Tier enum lands where CoinGecko client construction is
consolidated, and the Binance 24hr-ticker switch (F-22) routes through the shared helpers.
**Any endpoint this SPEC touches MUST route through the Phase-3 shared helpers — no inline
request scaffolding may be reintroduced.**

Schema contact point: [SPEC-DB-001](../SPEC-DB-001/spec.md) `coin_candles` (`interval TEXT
NOT NULL`, PK `(coin_id, vs_currency, interval, ts)` per migration `0020`). Downstream
vocabulary anchor: [SPEC-API-003](../SPEC-API-003/spec.md) `interval_to_seconds`
(`src/api/candles_agg.rs`, already an `@MX:ANCHOR`) — the canonical interval vocabulary this
SPEC's stamps MUST be members of.

## HISTORY

- 2026-07-24 (v0.1.0): Initial draft. Establishes **REQ-PROV-065..081** — the provider
  data-correctness & tier-configuration block, continuing SPEC-PROV-001's REQ-PROV-001..045
  and SPEC-PROV-002's REQ-PROV-050..064 (non-colliding). Seven modules: (M1) a typed `Tier`
  enum parsed once from `COINGECKO_TIER`, fail-fast on unknown, `is_paid()` driving header +
  base-URL + capability in one place (F-21); (M2) canonical interval stamps — the CoinGecko
  range snap returns a `(param, canonical)` tuple, stamping `"1d"`/`"1h"` on candles while
  keeping `"daily"`/`"hourly"` as request params (F-20); (M3) an idempotent guarded cleanup
  migration for any already-written `daily`/`hourly` rows (F-20 migration); (M4) honest
  Binance spot volume via `GET /api/v3/ticker/24hr` + real bid/ask (F-22); (M5)
  scientific-notation Decimal fallback + one-way optional-field degradation strictness
  (F-23); (M6) boundary-aware, venue-preferring, deterministic derivatives matching (F-24);
  (M7) mechanical fixes — snapped-limit division (F-25), a distinct non-empty-all-unsupported
  chain error (F-26), logged degradations (F-27), typed/by-reference DTO iteration (F-28).
  Brownfield — extends SPEC-PROV-001/002. Files: `src/providers/{coingecko,binance,mod}.rs`,
  `src/config.rs`, `migrations/0021_*.sql` (new). No new dependency; env-only config;
  Decimal-only money (REQ-PROV-012) preserved; `Provider` trait public surface preserved.

---

## Goal

Make every value a provider stamps or persists **correct and canonical**, and make the
CoinGecko tier configuration **coherent** (one tier decision, one place, fail-fast). After
this SPEC:

1. `COINGECKO_TIER=analyst` (the deep-backfill configuration) sends `x-cg-pro-api-key`
   against `https://pro-api.coingecko.com` AND enables the range capability — all three
   tier-dependent decisions derive from one parsed `Tier` enum; an unknown tier value fails
   startup rather than silently defaulting.
2. No code path can persist a `coin_candles` row whose `interval` is absent from the
   canonical `candles_agg::interval_to_seconds` vocabulary — the CoinGecko range path stamps
   `"1d"`/`"1h"` (canonical) while still sending `"daily"`/`"hourly"` as the API request
   param, mirroring the already-correct Bitstamp `(step, canonical_name)` split.
3. Binance spot quotes take their price (from the ticker `lastPrice`), real 24-hour volume,
   and real bid/ask from `GET /api/v3/ticker/24hr` — never a 1-minute kline volume in a
   `volume_24h` field, and the price source moves off the 1m-kline close deliberately.
4. A micro-cap price serialized in scientific notation (`1.234e-11`) parses **exactly** to
   `Decimal` (via `Decimal::from_scientific`, never `f64`); an unparseable **optional** field
   degrades to `None` with a `warn!` instead of failing the whole page, while required
   monetary fields still fail loudly.
5. A derivatives lookup for `BTC` never binds `BTCDOM`/`BTCST`/`BTCUP`, prefers the queried
   venue when present, and picks deterministically among remaining candidates.
6. All quality gates green: `cargo test`, `cargo clippy --all-targets --all-features -- -D
   warnings`, `cargo fmt --check`.

## Problem (Why)

- **F-20 — non-canonical interval stamps (High).** `coingecko_range_snap_interval`
  (`coingecko.rs:599-606`) returns the CoinGecko API param values `"daily"`/`"hourly"`, and
  the same string is stamped as the candle's `interval` (via `:283` → `normalise_ohlc_item`
  `:661`; a test asserts `c.interval == "daily"` at `:1435`). But `interval_to_seconds`
  (`candles_agg.rs:28-48`, verified) has **no** `"daily"`/`"hourly"` rows — only canonical
  `"1m"…"1w"`. Range-backfilled rows are therefore invisible to interval resolution, coverage
  selection, and any `interval = '1d'` query; the same series splits across two keys depending
  on which provider served the page. Every other provider stamps canonical taxonomy — Bitstamp
  explicitly separates the API step from the stamped name (`snap_to_bitstamp_step` returns
  `(i64, &'static str)`, `bitstamp.rs:174`). Latent only because the range path is
  Analyst+-tier-gated; upgrading the tier silently writes orphaned rows.
- **F-21 — paid tiers get the demo header + demo base URL (High, Verified verbatim).**
  `supports_ohlc_range` accepts `analyst|lite|enterprise|pro` (`coingecko.rs:59-64`), but
  `key_header_name` returns the pro header only for exactly `"pro"` (`:50-56`), and
  `config::coingecko_base_url` defaults every non-`pro` tier to `https://api.coingecko.com`
  (`config.rs:172-180`). All CoinGecko paid plans are Pro-API plans
  (`pro-api.coingecko.com` + `x-cg-pro-api-key`). `COINGECKO_TIER=analyst` — the exact deep
  backfill configuration — enables the range capability but sends the key in the demo header
  to the demo host. Three tier-dependent decisions live in two files with two different tier
  sets.
- **F-22 — 1-minute volume stored in `volume_24h` (Medium).** `fetch_spot` fetches one 1m
  kline and sets `volume_24h: candle.volume` (`binance.rs:305-314`) — under-reports by ~3
  orders of magnitude in the same normalized field CoinGecko fills genuinely, silently
  corrupting cross-source comparisons whenever the chain falls back. `GET /api/v3/ticker/24hr`
  gives the correct value plus real bid/ask.
- **F-23 — `Decimal::from_str` cannot parse scientific notation (Medium).** With
  `arbitrary_precision`, `Number::to_string()` returns the JSON text verbatim including
  exponent forms; `Decimal::from_str` rejects them (`from_scientific` is a separate
  constructor). `decimal_from_number` (`coingecko.rs:74-77`) uses `from_str`, and
  normalization uses `collect::<Result<…>>`, so one bad field fails the whole page. The
  `precision=full` request param makes extreme representations plausible for micro-caps.
- **F-24 — derivatives lookup binds the wrong ticker (Medium).** `fetch_derivatives`
  (`coingecko.rs:951-972`) pulls the whole `/derivatives/tickers` payload and picks the first
  symbol `to_uppercase().starts_with(base)`. `"BTC"` also matches `BTCDOM`/`BTCST`/`BTCUP`,
  and "first in response order" is an arbitrary venue that can change between polls.
- **F-25 — snapped-limit mismatch (Low).** Binance `fetch_ohlc` requests the snapped kline
  interval but divides by unsnapped seconds to compute `limit` (`binance.rs:324-327`) — wrong
  lookback for between-band inputs. Latent (callers pass canonical seconds).
- **F-26 — "empty provider chain" mislabel (Low).** `chain_fetch_ohlc` seeds `last_err` with
  `Other(anyhow!("empty provider chain"))` (`providers/mod.rs:429`) and returns it unchanged
  when the chain was non-empty but every member was skipped as `Unsupported` (the `continue`
  at `:438` never updates `last_err`).
- **F-27 — silent normalization degradations (Low).** Unparseable `last_updated` silently
  becomes `Utc::now()` (`coingecko.rs:508-512`) — a malformed upstream timestamp masquerades
  as a fresh quote time; `max_supply` parse failures are swallowed via `.ok()`
  (`:720-735`-ish) while sibling fields propagate errors.
- **F-28 — `serde_json::Value` array deep-clones (Low, craft-only).** `body["coins"]...` /
  `body["tickers"]...` deep-clone arrays before read-only iteration; per-element
  `to_uppercase()` in the derivatives scan. Stylistically inconsistent with the typed DTOs
  used elsewhere in the same file. Not hot paths.

## Scope

In scope:
- **Typed tier configuration (F-21)** — a `Tier` enum `{ Demo, Analyst, Lite, Pro,
  Enterprise }` parsed **once** from `COINGECKO_TIER`, **fail-fast on unknown** values
  (startup error, not a silent default — matching the `build_chain` fail-fast philosophy).
  `is_paid()` (true for Analyst/Lite/Pro/Enterprise; false for Demo) drives BOTH the API-key
  header (`x-cg-pro-api-key` for all paid tiers) AND the default base URL
  (`https://pro-api.coingecko.com` for all paid tiers; `COINGECKO_BASE_URL` env still
  overrides). `supports_ohlc_range()` derives from the enum. All three tier decisions live in
  one place. (REQ-PROV-068/069/070/071)
- **Canonical interval stamps (F-20)** — the CoinGecko range snap returns a `(param,
  canonical)` tuple (mirroring Bitstamp's `snap_to_bitstamp_step`): `"daily"`/`"hourly"` stay
  REQUEST params, canonical `"1d"`/`"1h"` are stamped on returned candles. The existing test
  asserting `interval == "daily"` is updated. A cross-module invariant guarantees every
  persisted range candle carries an interval present in `candles_agg::interval_to_seconds`.
  (REQ-PROV-065/066)
- **F-20 cleanup migration (F-20 migration)** — a new idempotent guarded migration that
  rewrites any already-written `daily`/`hourly` rows to `1d`/`1h`; a no-op when zero such rows
  exist (expected, since the range path is tier-gated). (REQ-PROV-067)
- **Honest Binance spot volume + pinned price source (F-22)** — spot switches to `GET
  /api/v3/ticker/24hr` for the real 24-hour volume, the spot price (from the ticker
  `lastPrice`), and real bid/ask from the same payload, routed through the Phase-3
  `paced`/`get_json` helpers. A 1-minute volume is never stored in a 24-hour field, and the
  price source moves from the 1m-kline close to `lastPrice` deliberately (specified, not
  silent). (REQ-PROV-072/073)
- **Scientific-notation Decimal + degradation strictness (F-23)** — `decimal_from_number`
  falls back to `Decimal::from_scientific` when `from_str` fails (MUST go through
  `from_scientific`, NEVER `f64` — REQ-PROV-012). An unparseable **optional** field degrades
  to `None` with a `warn!`; required monetary fields keep failing loudly. (REQ-PROV-074/075)
- **Logged degradations (F-27)** — the `last_updated` → `Utc::now()` and `max_supply` `.ok()`
  degradations keep their behavior but gain `warn!`/`debug!` logs, aligning optional-field
  strictness one way (consistent with the F-23 decision). (REQ-PROV-076)
- **Boundary-aware derivatives matching (F-24)** — a symbol-boundary match (exact, OR prefix
  followed by a non-alphanumeric char) so `BTC` does not match `BTCDOM`; prefer the queried
  venue when `market.venue` is present; deterministic tie-break (e.g. highest open interest)
  among remaining candidates. (REQ-PROV-077/078)
- **Mechanical fixes** — Binance `fetch_ohlc` divides by the snapped seconds (F-25,
  REQ-PROV-079); `chain_fetch_ohlc` returns a distinct error when a non-empty chain is
  entirely unsupported, never claiming "empty provider chain" (F-26, REQ-PROV-080);
  search/tickers/derivatives paths iterate by reference or deserialize into typed DTOs
  instead of deep-cloning arrays (F-28, REQ-PROV-081).
- **Documentation + @MX tags** — document the tier semantics (which tiers are paid; what the
  header / base-URL / capability each derive from) where `COINGECKO_TIER` is described; add
  the canonical-stamp `@MX:ANCHOR` contract on the range path.

Out of scope: see Exclusions.

## Decisions Restated (authoritative — settled in the plan-phase brief; not to be re-litigated)

- **D1 — F-20 stamp split (mirror Bitstamp).** The CoinGecko range snap returns a `(param,
  canonical)` tuple exactly like Bitstamp's `snap_to_bitstamp_step` (`(i64, &'static str)`).
  `"daily"`/`"hourly"` remain the API request param; canonical `"1d"`/`"1h"` are stamped on
  returned candles. The test asserting `interval == "daily"` is updated to assert the
  canonical stamp.
- **D2 — F-20 migration = idempotent, collision-safe cleanup migration.** A new sqlx
  migration rewrites `daily`→`1d` and `hourly`→`1h` in `coin_candles`. Because `interval` is
  part of the PRIMARY KEY `(coin_id, vs_currency, interval, ts)`, a plain `UPDATE` could raise
  a unique-violation (→ failed startup migration → service-down) if a non-canonical row and its
  canonical twin coexist at the same `(coin_id, vs_currency, ts)`. The **shipped default is
  therefore the collision-safe form** — it drops the shadowed non-canonical duplicate on
  collision (the canonical twin already carries the correct data) before rewriting the rest;
  the plain single `UPDATE` is retained only as the simpler-but-collision-unsafe alternative.
  The exact default migration `0021` body is in plan.md § Migration Safety. It is a no-op when
  zero such rows exist (expected, since the range path is tier-gated), so NO live DB check is
  required and it self-heals any rows that slipped in. Migrations are embedded at compile time
  (project memory `sqlx-migrate-embed-rebuild` — a migrations-only change needs the binary
  rebuilt; `build.rs` guards it). The `SELECT DISTINCT interval FROM coin_candles` verification
  is **informational only, not a gate**.
- **D3 — F-21 typed tier + fail-fast.** A `Tier` enum `{ Demo, Analyst, Lite, Pro,
  Enterprise }` parsed once from `COINGECKO_TIER`, **fail-fast on unknown** (startup error,
  matching `build_chain`). `is_paid()` (true for Analyst/Lite/Pro/Enterprise; false for Demo)
  drives BOTH the API-key header (`x-cg-pro-api-key` for all paid tiers) AND the default base
  URL (`pro-api.coingecko.com` for all paid tiers; `COINGECKO_BASE_URL` still overrides).
  `supports_ohlc_range()` derives from the enum. All three decisions live in ONE place.
- **D4 — F-22 = 24hr ticker endpoint (price source pinned).** Binance spot switches to `GET
  /api/v3/ticker/24hr` for the real 24h volume, the spot price (from the ticker's `lastPrice`
  field), AND real bid/ask from the same payload, routed through the Phase-3 `paced`/`get_json`
  helpers. Never store a 1m volume in a 24h field. The spot price source moves from the
  1m-kline close to `lastPrice` — this price-source change is deliberate and explicitly
  specified, not a silent side effect of the endpoint switch.
- **D5 — F-23 = optional→None+warn, required hard-fail.** `decimal_from_number` falls back to
  `Decimal::from_scientific` (NEVER `f64` — REQ-PROV-012). For an unparseable **optional**
  field: degrade to `None` with a `warn!` (item survives, page survives). Required monetary
  fields keep failing loudly. This aligns optional-field strictness one way (ties to F-27).

## Domain Model — affected sites (delta markers)

Delta markers: **[EXISTING]** relied upon unchanged, **[MODIFY]** changed, **[NEW]** net-new.

| Marker | Path | Role |
|--------|------|------|
| [NEW] | `src/config.rs` — `Tier` enum + `Tier::from_env()` (or parse from `coingecko_tier()`) | `{ Demo, Analyst, Lite, Pro, Enterprise }`, parsed once, fail-fast on unknown; `is_paid()` + `supports_ohlc_range()` derive here (REQ-PROV-068/069). |
| [MODIFY] | `src/config.rs:172-180` (`coingecko_base_url`) | Default base URL derives from `Tier::is_paid()` (pro host for all paid tiers); `COINGECKO_BASE_URL` override preserved (REQ-PROV-070). |
| [MODIFY] | `src/providers/coingecko.rs:50-56` (`key_header_name`) | Pro header for all paid tiers via `is_paid()` (REQ-PROV-070). |
| [MODIFY] | `src/providers/coingecko.rs:59-64` (`supports_ohlc_range`) | Derive from the `Tier` enum, single source (REQ-PROV-071). |
| [MODIFY] | `src/providers/coingecko.rs:599-606` (`coingecko_range_snap_interval`) | Return `(param, canonical)` tuple; `"daily"`/`"hourly"` param, `"1d"`/`"1h"` canonical stamp (REQ-PROV-065). |
| [MODIFY] | `src/providers/coingecko.rs:275-295` (range fetch) + `normalise_ohlc_item` (`:659-661`) | Stamp the canonical name on candles; send the param to the API (REQ-PROV-065/066). |
| [MODIFY] | `src/providers/coingecko.rs:1435` (range wiremock test) | Assert canonical `"1d"` stamp instead of `"daily"` (REQ-PROV-065). |
| [NEW] | `migrations/0021_coingecko_range_interval_canonicalise.sql` | Idempotent guarded `daily`→`1d`, `hourly`→`1h` cleanup (REQ-PROV-067). |
| [MODIFY] | `src/providers/binance.rs:305-314` (`fetch_spot`) | Fetch `GET /api/v3/ticker/24hr` via `paced`/`get_json`; `price` from `lastPrice`, real `volume_24h` + bid/ask (REQ-PROV-072/073). |
| [MODIFY] | `src/providers/coingecko.rs:74-77` (`decimal_from_number`) | `from_scientific` fallback when `from_str` fails, never `f64` (REQ-PROV-074). |
| [MODIFY] | `src/providers/coingecko.rs` (optional-field normalization, `:234-237`/`:279-281` collect sites) | Optional field unparseable → `None` + `warn!`; required stays hard-fail (REQ-PROV-075). |
| [MODIFY] | `src/providers/coingecko.rs:508-512` (`last_updated`) + `:720-735` (`max_supply`) | Keep degradation, add `warn!`/`debug!`; align strictness one way (REQ-PROV-076). |
| [MODIFY] | `src/providers/coingecko.rs:951-972` (`fetch_derivatives`) | Boundary-aware match; venue preference; deterministic tie-break (REQ-PROV-077/078). |
| [MODIFY] | `src/providers/binance.rs:324-327` (`fetch_ohlc` limit) | Divide by the SNAPPED seconds; snap returns `(secs, name)` (REQ-PROV-079). |
| [MODIFY] | `src/providers/mod.rs:429` (`chain_fetch_ohlc` `last_err` seed) | Distinct error (e.g. `NoCapableProvider(Capability)`) when a non-empty chain is all-unsupported (REQ-PROV-080). |
| [MODIFY] | `src/providers/coingecko.rs` (search/tickers/derivatives `Value` indexing) | Iterate by reference or typed DTOs; no array deep-clone (REQ-PROV-081). |
| [EXISTING] | `src/api/candles_agg.rs:28-48` (`interval_to_seconds`, `@MX:ANCHOR`) | Canonical interval vocabulary — the membership set REQ-PROV-066 enforces; unchanged. |
| [EXISTING] | `src/providers/transport.rs` (`paced`/`get_json`/`build_client`) | Phase-3 shared helpers — reused, not modified; F-22's new endpoint routes through them. |

---

## Requirements (GEARS)

### Module 1 — Typed tier configuration (F-21) [High] [NEW/MODIFY]

- **REQ-PROV-068** [NEW] (Ubiquitous): The configuration layer **shall** expose a `Tier` enum
  `{ Demo, Analyst, Lite, Pro, Enterprise }` parsed **once** from the `COINGECKO_TIER`
  environment variable, and this parsed value **shall** be the single authority for all three
  tier-dependent decisions (API-key header, default base URL, range capability).
- **REQ-PROV-069** [NEW] (Unwanted / event-detected): When `COINGECKO_TIER` holds a value
  that is not one of the five recognized tiers, the system **shall** fail startup with a clear
  error naming the offending value; it **shall not** silently default to Demo or any other
  tier (matching the `build_chain` fail-fast philosophy).
- **REQ-PROV-070** [MODIFY] (Ubiquitous): The tier's `is_paid()` predicate (true for
  Analyst / Lite / Pro / Enterprise; false for Demo) **shall** drive BOTH the API-key header
  (`x-cg-pro-api-key` for every paid tier, `x-cg-demo-api-key` for Demo) AND the default base
  URL (`https://pro-api.coingecko.com` for every paid tier, `https://api.coingecko.com` for
  Demo). Where `COINGECKO_BASE_URL` is set, the system **shall** use it verbatim as an
  override (unchanged).
- **REQ-PROV-071** [MODIFY] (Where — capability gate): Where the parsed tier `is_paid()`,
  `supports_ohlc_range()` **shall** return `true`; where the tier is Demo it **shall** return
  `false`. The capability **shall** derive from the `Tier` enum, not from a separately
  maintained string set.

### Module 2 — Canonical interval stamps (F-20) [High] [MODIFY]

- **REQ-PROV-065** [MODIFY] (Ubiquitous): The CoinGecko range snap **shall** return a
  `(param, canonical)` pair (mirroring Bitstamp's `snap_to_bitstamp_step` `(i64, &'static
  str)` shape): the API request **shall** send the param (`"daily"`/`"hourly"`) while returned
  candles **shall** be stamped with the canonical name (`"1d"`/`"1h"`).
- **REQ-PROV-066** [MODIFY] (Unwanted): No provider **shall** persist a `coin_candles` row
  whose `interval` value is absent from the canonical `candles_agg::interval_to_seconds`
  vocabulary table. Every interval a provider stamps **shall** be a member of that table.

### Module 3 — Interval-stamp cleanup migration (F-20 migration) [NEW]

- **REQ-PROV-067** [NEW] (Event-driven): When migrations run at startup, an idempotent,
  collision-safe migration **shall** rewrite any existing `coin_candles` rows with `interval
  IN ('daily','hourly')` to `'1d'`/`'1h'` respectively, and **shall** be a no-op when zero such
  rows exist. Because `interval` is part of the PRIMARY KEY, the migration **shall not** raise
  a unique-violation (which would fail the startup migration and take the service down) when a
  non-canonical row and its canonical twin coexist at the same `(coin_id, vs_currency, ts)` —
  on such a collision it **shall** drop the non-canonical duplicate (the canonical twin already
  carries the correct data) and then rewrite the remaining rows. The migration **shall not**
  require a live pre-check to be safe.

### Module 4 — Honest Binance spot volume (F-22) [Medium] [MODIFY]

- **REQ-PROV-072** [MODIFY] (Event-driven): When Binance spot data is fetched, the system
  **shall** source the spot `price` (from the ticker's `lastPrice` field), `volume_24h`, and
  bid/ask from `GET /api/v3/ticker/24hr`, routed through the Phase-3 `transport::paced()` /
  `transport::get_json()` helpers. The spot price source moves from the 1m-kline close to the
  ticker `lastPrice` — this price-source change is deliberate and specified, not silent.
- **REQ-PROV-073** [MODIFY] (Unwanted): The system **shall not** store a single 1-minute kline
  volume in the `volume_24h` field of a spot quote.

### Module 5 — Scientific-notation Decimal + degradation strictness (F-23) [Medium] [MODIFY]

- **REQ-PROV-074** [MODIFY] (Event-detected): When `Decimal::from_str` fails to parse a
  provider `serde_json::Number`, `decimal_from_number` **shall** fall back to
  `Decimal::from_scientific`; it **shall** parse a scientific-notation value (e.g.
  `1.234e-11`) exactly, and **shall not** route any value through `f64` (REQ-PROV-012).
- **REQ-PROV-075** [MODIFY] (State-driven): While normalising an item, when an **optional**
  field is unparseable the system **shall** degrade that field to `None` with a `warn!` (the
  item and the containing page survive); when a **required** monetary field is unparseable the
  system **shall** continue to fail loudly (the item fails).

### Module 6 — Logged degradations (F-27) [Low] [MODIFY]

- **REQ-PROV-076** [MODIFY] (Ubiquitous): Each optional-field degradation that currently
  occurs silently — an unparseable `last_updated` falling back to `Utc::now()`, a `max_supply`
  parse failure swallowed via `.ok()` — **shall** emit a `warn!`/`debug!` log while keeping its
  existing degradation **target** unchanged: `last_updated` **shall** deliberately retain its
  `Utc::now()` fallback (it is NOT converted to `None`), and `max_supply` **shall** retain its
  `.ok()`→`None` degradation. "Aligned one way" (consistent with REQ-PROV-075) means only that
  every optional-field degradation now **degrades-and-logs** rather than degrading silently —
  it does NOT mean every degradation target becomes `None`.

### Module 7 — Boundary-aware derivatives matching (F-24) [Medium] [MODIFY]

- **REQ-PROV-077** [MODIFY] (Ubiquitous): The derivatives lookup **shall** match a ticker
  symbol against the queried base by symbol boundary — an exact match, OR the base as a prefix
  followed by a non-alphanumeric character — so that querying `BTC` **shall not** match
  `BTCDOM`, `BTCST`, or `BTCUP`.
- **REQ-PROV-078** [MODIFY] (State-driven): While multiple candidate tickers match, when
  `market.venue` is present the system **shall** prefer the ticker from that venue; among
  remaining candidates it **shall** pick deterministically (e.g. highest open interest),
  never relying on upstream response order.

### Module 8 — Mechanical fixes (F-25, F-26, F-28) [Low] [MODIFY]

- **REQ-PROV-079** [MODIFY] (Ubiquitous): Binance `fetch_ohlc` **shall** compute its request
  `limit` by dividing by the **snapped** kline-interval seconds (the snap **shall** return a
  `(secs, name)` pair, Bitstamp pattern), so the lookback matches the interval actually
  requested.
- **REQ-PROV-080** [MODIFY] (Event-driven): When the provider chain is non-empty but every
  member is skipped as `Unsupported` for the requested capability, `chain_fetch_ohlc`
  **shall** return an error that reflects "no capable provider" (e.g.
  `NoCapableProvider(Capability)` or a value synthesized from the attempt records); it
  **shall not** return the `"empty provider chain"` label for a non-empty chain.
- **REQ-PROV-081** [MODIFY] (Ubiquitous): The search, tickers, and derivatives paths **shall**
  iterate `serde_json` arrays by reference or via typed DTOs; they **shall not** deep-clone an
  array before read-only iteration. (Craft-only; not a hot path — no behavior change.)

---

## Edge Cases (summarized; full behavior in acceptance.md)

- **analyst tier end-to-end.** `COINGECKO_TIER=analyst` → `x-cg-pro-api-key` +
  `pro-api.coingecko.com` + `supports_ohlc_range()==true` (wiremock-verified header + base
  URL). (REQ-PROV-068/070/071)
- **unknown tier.** `COINGECKO_TIER=platinum` → startup error naming `platinum`, no silent
  default. (REQ-PROV-069)
- **`COINGECKO_BASE_URL` override.** Set to a custom host on a paid tier → that host is used
  verbatim (override beats the `is_paid()` default). (REQ-PROV-070)
- **range stamp round-trip.** A range-backfilled candle is stamped `"1d"` and that stamp
  resolves through `candles_agg::interval_to_seconds` to `86_400`; the API request still
  carried `daily`. (REQ-PROV-065/066)
- **migration no-op.** With zero `daily`/`hourly` rows, the migration changes nothing;
  `SELECT DISTINCT interval` is unchanged (informational). (REQ-PROV-067)
- **Binance 24hr volume + price.** Spot `price` reflects the ticker `lastPrice` and
  `volume_24h` reflects the `/api/v3/ticker/24hr` `volume` field (not a 1m kline); bid/ask
  populated. (REQ-PROV-072/073)
- **scientific-notation micro-cap.** `1.234e-11` parses exactly to `Decimal`; a plain and a
  high-precision value also parse exactly; no `f64`. (REQ-PROV-074)
- **optional vs required degradation.** An unparseable optional field → `None` + `warn!`, item
  survives; an unparseable required monetary field → item fails. (REQ-PROV-075)
- **derivatives boundary.** `BTC` matches `BTC-PERP`/`BTCUSDT` (boundary) but not `BTCDOM`;
  venue preference + deterministic tie-break resolve multiple candidates. (REQ-PROV-077/078)
- **snapped limit.** A between-band `interval_secs` divides by the snapped seconds, not the
  raw. (REQ-PROV-079)
- **all-unsupported chain.** A non-empty chain with every member unsupported returns a "no
  capable provider" error, not "empty provider chain". (REQ-PROV-080)

## Exclusions (What NOT to Build)

The following are explicitly **out of scope** for SPEC-PROV-003.

### Out of Scope — Provider trait redesign & chain pacing (Phase 7)
- No change to the `Provider` trait's public surface. Per-provider pacing in the chain
  fallback (F-16) and the trait consolidation (F-50) remain **Phase 7**. This SPEC preserves
  the trait exactly; only the data each method normalises/stamps and the tier config change.

### Out of Scope — API contract & HTTP boundary (Phase 5)
- The `vs_currency` filter drift (F-29), unbounded `coin_quotes` reads (F-30), aggregation
  `end`-filter push-down (F-31), and the rest of Category E are **Phase 5**. This SPEC does
  not touch `src/api/*` beyond reading `candles_agg::interval_to_seconds` as the canonical
  vocabulary anchor.

### Out of Scope — New dependency, env var, or schema column
- No new crate dependency (`Cargo.toml` diff empty). No new `coin_candles` column and no new
  table — the F-20 migration is a data `UPDATE`, not a schema change. Config stays env-only.
  The `Tier` enum is an in-code type, not a persisted value.

### Out of Scope — Deep candle backfill / provider selection policy
- Which provider serves a range backfill, the CoinGecko tier's actual credit budget, and the
  backfill worker's cursor loop are unchanged. This SPEC corrects what a range candle is
  *stamped* with and which host/header a request uses — not which provider is chosen or how
  deep it backfills.

### Out of Scope — `f64` in monetary paths
- The Decimal-only monetary invariant (REQ-PROV-012) is untouched; the F-23 scientific
  fallback goes through `Decimal::from_scientific`, and no `f64` is introduced in any provider
  path.

## @MX Annotation Targets (high fan_in / invariant contracts)

- **`@MX:ANCHOR`** on the CoinGecko range stamp path (`coingecko_range_snap_interval` +
  `normalise_ohlc_item` interval stamping) — the **canonical-stamp contract**: every interval
  a provider stamps MUST be a member of `candles_agg::interval_to_seconds`. Pairs with the
  existing `@MX:ANCHOR` on `interval_to_seconds` (`candles_agg.rs:22`). (`@MX:REASON` required
  — F-20 existed because the range path stamped non-canonical strings invisible to interval
  resolution.)
- **`@MX:ANCHOR`** on the `Tier` enum / tier-decision authority — the **single tier-decision
  point**: `is_paid()` drives header + base URL + capability; no tier decision may be made
  outside this authority. (`@MX:REASON` required — F-21 existed because three tier decisions
  lived in two files with two different tier sets.)
- **`@MX:ANCHOR`** (or update) on `decimal_from_number` — the **Decimal-only monetary parse
  core**: every provider `Number` → `Decimal` goes through here (fan_in high across all
  normalisers); `from_scientific` is the fallback, never `f64` (REQ-PROV-012). (`@MX:REASON`
  required.)
- **`@MX:NOTE`** on Binance `fetch_spot` — records that `price` (`lastPrice`), `volume_24h`,
  and bid/ask now come from `/api/v3/ticker/24hr`, never a 1m kline (F-22).
- **`@MX:NOTE`** on `fetch_derivatives` — records the boundary-aware + venue-preference +
  deterministic-tie-break matching contract (F-24).

Full MX placement, tag text, and update/remove policy in plan.md § MX Tag Targets.

## Open Items

**0 unresolved.** All five decision forks are settled as D1 (F-20 stamp split, mirror
Bitstamp), D2 (F-20 idempotent guarded cleanup migration), D3 (F-21 typed tier + fail-fast +
`is_paid()` single authority), D4 (F-22 24hr ticker endpoint), D5 (F-23 optional→None+warn,
required hard-fail). No `[NEEDS CLARIFICATION]` markers remain; residual micro-decisions (exact
`Tier` parse location in `config.rs`, the precise "no capable provider" error variant name,
the derivatives tie-break metric) are plan.md/run-phase concerns, not requirement ambiguities.
