# SPEC-PROV-003 — Implementation Plan

Phase 4 of the 7-phase idiomatic-Rust improvement roadmap. Tier **M**. Development mode:
**TDD** (brownfield) — every REQ has a RED-first test that MUST fail against current `main`
before the fix. Route **A** (Hybrid Trunk main-direct, Tier M default; commit-direct-to-main
per CLAUDE.local.md — no feature branch, no per-phase PR).

Milestones are ordered by **decision-reversibility**: the highest-change-likelihood decisions
(the `Tier` enum data-model change and the canonical-stamp contract) come first so review
focuses there; mechanical small fixes (F-25/F-26/F-27/F-28) land last on corrected behavior.

## §A Context

- Project root: `/home/dholm/Projects/crypto-collector`. Rust microservice, `finance` k8s
  namespace, `aarch64`. Build/test: `cargo test`, `cargo clippy --all-targets --all-features
  -- -D warnings`, `cargo fmt --check`.
- Prerequisite: **SPEC-PROV-002 `implemented`** (feat `380150f`). The shared
  `transport::paced()` / `transport::get_json()` / `transport::build_client()` helpers are on
  the working tree — every touched endpoint routes through them; no inline request scaffolding.
- Source of findings: `research/idiomatic-rust.md` §6 Category D (F-20…F-28, lines 182–222),
  §7 line 383 (Phase 4), §8 lines 397–398 (Phase 4 depends on Phase 3; F-20 migration note).
- PRESERVE targets (do NOT touch beyond these): `src/providers/{coingecko,binance,mod}.rs`,
  `src/config.rs`, `migrations/0021_*.sql` (new). Do NOT touch `src/api/*` (Phase 5),
  `src/providers/{bitstamp,coinbase,kraken}.rs` (bitstamp is the reference pattern, read-only),
  `Cargo.toml`/`Cargo.lock` (no new dependency), `src/pacer/*`, `src/main.rs`.

## §B Known Issues (auto-injected, filtered to Rust relevance)

- **B4 — Frontmatter canonical schema**: `created:`/`updated:`/`tags:` (snake_case aliases
  prohibited); the 12 canonical fields are present in spec.md.
- **B6 — spec-lint Out of Scope heading**: spec.md carries `### Out of Scope — <topic>` H3
  sub-headings with `-` bullets (not a bare `## Out of Scope` H2).
- **B9 — Commit direct to main (Hybrid Trunk, Tier M)**: manager-develop commits + pushes
  directly to `main`; Conventional Commits (`feat(SPEC-PROV-003): M{N} …`); never
  `--no-verify`.
- **B10 — Scope discipline**: touch only the PRESERVE set. `bitstamp.rs` is the read-only
  reference for the `(step, canonical)` snap pattern — do not modify it.
- **Cross-SPEC pre-scan**: `interval_to_seconds` is owned by SPEC-API-003 (`@MX:ANCHOR`).
  REQ-PROV-066 enforces membership in that table but MUST NOT modify the table (adding a new
  stored interval string would require an entry there first — not this SPEC's job; the range
  stamps `"1d"`/`"1h"` are already members).
- **Migration embedding (project memory `sqlx-migrate-embed-rebuild`)**: migrations are
  embedded at compile time via `sqlx::migrate!()`; a migrations-only change needs the binary
  rebuilt (`build.rs` guards it). The deploy silently ships stale otherwise.

## §C Pre-flight (run before any code change)

```bash
git branch --show-current                 # expect: main
git rev-parse HEAD
cargo build                               # baseline compiles
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -5   # lint baseline
grep -rn '"daily"\|"hourly"' src/providers/coingecko.rs                   # F-20 sites
grep -n "supports_ohlc_range\|key_header_name" src/providers/coingecko.rs # F-21 sites
grep -n "fn snap_to_bitstamp_step" src/providers/bitstamp.rs              # (step,name) reference
ls migrations/ | sort | tail -3           # confirm 0020 is highest → new file is 0021
```

## §D Constraints (carry-through; DO NOT VIOLATE)

- **Decimal-only money** (REQ-PROV-012): the F-23 scientific fallback goes through
  `Decimal::from_scientific`, NEVER `f64`. No `f64` in any provider path.
- **Chain semantics preserved**: preserve the provider chain fallback ORDER (= declaration
  order) and the range-path continue-on-empty / error-must-surface distinction — regression
  tests exist; keep them green. The F-26 fix changes only the *error label* for a non-empty
  all-unsupported chain, not the control flow.
- **Phase-3 shared helpers**: every touched endpoint (F-22's new `/api/v3/ticker/24hr`) routes
  through `transport::paced()` / `transport::get_json()`. No inline request/429/parse
  scaffolding may be reintroduced.
- **No new dependency; env-only config**: `Cargo.toml`/`Cargo.lock` diff empty. The `Tier`
  enum is an in-code type; `COINGECKO_TIER` stays the sole tier env var.
- **Downstream vocabulary anchor**: `interval_to_seconds` (`src/api/candles_agg.rs`) is the
  canonical interval vocabulary; every stamp this SPEC produces MUST be a member of that
  table. Do NOT edit the table.
- **No schema change**: the F-20 migration is a data `UPDATE` only — no new column, no new
  table.

### Migration Safety (F-20, REQ-PROV-067) — migration `0021` default body

`coin_candles` PRIMARY KEY is `(coin_id, vs_currency, interval, ts)` (migration `0020` line
39) — `interval` is part of the PK. A plain `UPDATE … SET interval = CASE …` could therefore
raise a unique-violation IF a `daily`/`hourly` row and its canonical `1d`/`1h` twin coexist at
the same `(coin_id, vs_currency, ts)`; a migration that raises inside `sqlx::migrate!()` fails
startup and takes the service down. The collision is expected impossible in practice (the range
path is tier-gated and no `daily`/`hourly` rows are expected at all), but a startup-fatal
failure mode is not acceptable as the shipped default. **The shipped default `0021` body is
therefore the collision-safe DELETE-shadowed-then-UPDATE form** below — a set-based, idempotent
migration that is a no-op on zero rows, requires no live pre-check, and can never raise a PK
unique-violation:

```sql
-- migrations/0021_coingecko_range_interval_canonicalise.sql
-- F-20 (SPEC-PROV-003): canonicalise CoinGecko range-path interval stamps
-- ('daily'->'1d', 'hourly'->'1h'). Idempotent + collision-safe: no-op when zero such rows
-- exist; never raises a PK unique-violation. coin_candles PK is
-- (coin_id, vs_currency, interval, ts) — a bare UPDATE would collide when a non-canonical row
-- and its canonical twin share (coin_id, vs_currency, ts).

-- Step 1: drop any non-canonical row that is shadowed by an existing canonical twin
-- (the canonical twin already carries the correct data).
DELETE FROM coin_candles c
WHERE c.interval IN ('daily', 'hourly')
  AND EXISTS (
      SELECT 1 FROM coin_candles t
      WHERE t.coin_id = c.coin_id
        AND t.vs_currency = c.vs_currency
        AND t.ts = c.ts
        AND t.interval = CASE c.interval WHEN 'daily' THEN '1d' WHEN 'hourly' THEN '1h' END
  );

-- Step 2: canonicalise the remaining non-canonical rows (now collision-free).
UPDATE coin_candles
SET interval = CASE interval WHEN 'daily' THEN '1d' WHEN 'hourly' THEN '1h' END
WHERE interval IN ('daily', 'hourly');
```

Properties preserved: **no-op on zero rows** (both statements match nothing), **no live-DB
gate** (self-contained, safe to embed), **compile-time embedded** (`sqlx::migrate!()` — rebuild
the binary after adding it, per project memory `sqlx-migrate-embed-rebuild`), and
**idempotent** (a second run finds no `daily`/`hourly` rows and changes nothing).

Simpler-but-collision-unsafe alternative (NOT the default — recorded only for context): the
bare single `UPDATE coin_candles SET interval = CASE interval WHEN 'daily' THEN '1d' WHEN
'hourly' THEN '1h' END WHERE interval IN ('daily','hourly');`. It is correct ONLY when no
canonical twin can shadow a non-canonical row; because a shadow would make it startup-fatal, it
is not shipped. The `SELECT DISTINCT interval FROM coin_candles` check is informational only,
never a gate.

## §E Self-Verification (plan-phase audit-ready — see progress.md §E.1)

Plan-phase deliverables: spec.md (17 REQs, GEARS), plan.md (this), acceptance.md (11 ACs),
progress.md (§E skeleton). Frontmatter 12-field schema validated; SPEC ID
`SPEC-PROV-003` regex-checked PASS; `### Out of Scope` H3 sub-headings present; no
implementation code in spec.md.

## §F Milestones (ordered by decision-reversibility — highest-change-likelihood first)

### M1 — Typed tier configuration (F-21) [High — data-model change: new type + startup behavior]

- Add a `Tier` enum `{ Demo, Analyst, Lite, Pro, Enterprise }` in `src/config.rs`, parsed once
  from `COINGECKO_TIER`, **fail-fast on unknown** (startup error naming the value — match the
  `build_chain` fail-fast style). Add `is_paid()` (true for the four paid tiers) and
  `supports_ohlc_range()` (derives from the enum).
- Route `coingecko_base_url()` default and `key_header_name()` through `is_paid()`; keep the
  `COINGECKO_BASE_URL` override. Move `supports_ohlc_range` (coingecko.rs) to derive from the
  enum. All three tier decisions now share one authority.
- Tests (RED-first): pure tier matrix — for each tier assert `(header_name, default_base_url,
  supports_ohlc_range)`; unknown tier → parse/startup error. REQ-PROV-068/069/070/071.
- @MX: `@MX:ANCHOR` on the `Tier` decision authority (`@MX:REASON`).

### M2 — Canonical interval stamps + cross-module vocabulary contract (F-20) [High — persisted-value/behavioral]

- Change `coingecko_range_snap_interval` to return a `(param, canonical)` tuple (mirror
  Bitstamp's `snap_to_bitstamp_step` `(i64, &'static str)`). Send the param
  (`"daily"`/`"hourly"`) to `/ohlc/range`; stamp the canonical name (`"1d"`/`"1h"`) on
  returned candles via the range fetch + `normalise_ohlc_item` path.
- Update the wiremock test at `coingecko.rs:1435` to assert the canonical `"1d"` stamp while
  still asserting the request `interval` param is `"daily"`.
- Tests (RED-first): **cross-module vocabulary test** — a range-path candle is stamped
  `"1d"`/`"1h"` AND that stamp resolves through `candles_agg::interval_to_seconds` (ties the
  two modules together, REQ-PROV-066). REQ-PROV-065/066.
- @MX: `@MX:ANCHOR` on the range stamp path — canonical-stamp contract, pairs with the
  `interval_to_seconds` anchor (`@MX:REASON`).

### M3 — Interval-stamp cleanup migration (F-20 migration) [data cleanup]

- Add `migrations/0021_coingecko_range_interval_canonicalise.sql` with the **collision-safe
  DELETE-shadowed-then-UPDATE default body** (verbatim in § Migration Safety — the plain
  `UPDATE` is the collision-unsafe alternative, NOT shipped). No-op when zero rows exist.
- Rebuild the binary after adding the migration (embedded at compile time). REQ-PROV-067.
- Test: migration-file presence + shape test (project has a `tests/migration_files` group, no
  DB required); the row-rewrite behavior is DB-gated (`#[ignore]`, run with
  `--test-threads=1`) and informational.

### M4 — Honest Binance spot volume + pinned price source (F-22) [Medium — user-visible data change]

- Switch `fetch_spot` to `GET /api/v3/ticker/24hr`, routed through `transport::paced()` /
  `transport::get_json()`; populate `price` (from the ticker `lastPrice`), `volume_24h`, bid,
  and ask from that payload. Remove the 1m-kline `volume_24h` approximation and move the spot
  price source off the 1m-kline close to `lastPrice` (deliberate, specified).
- Tests (RED-first): **wiremock 24hr-ticker test** — stub `/api/v3/ticker/24hr`, assert
  `price` equals the ticker `lastPrice`, `volume_24h` equals the ticker `volume`, and bid/ask
  are populated. REQ-PROV-072/073.
- @MX: `@MX:NOTE` on `fetch_spot` (24hr-ticker source of truth).

### M5 — Scientific-notation Decimal + degradation strictness (F-23) [Medium — behavioral policy]

- `decimal_from_number`: fall back to `Decimal::from_scientific` when `from_str` fails (never
  `f64`). Make the optional-field normalization degrade an unparseable optional field to
  `None` + `warn!` while required monetary fields keep failing (adjust the `collect` sites so
  one bad optional field does not poison the page).
- Tests (RED-first): `decimal_from_number` matrix — plain, high-precision, and `1.234e-11`
  scientific inputs parse EXACTLY; optional-field degradation-to-None-with-warn covered;
  required-field hard-fail covered. REQ-PROV-074/075.
- @MX: `@MX:ANCHOR`/update on `decimal_from_number` (Decimal-only parse core, `@MX:REASON`).

### M6 — Boundary-aware derivatives matching (F-24) [Medium — behavioral]

- Replace the `to_uppercase().starts_with(base)` match with a symbol-boundary match (exact OR
  prefix + non-alphanumeric boundary); prefer `market.venue` when present; deterministic
  tie-break (e.g. highest open interest) among remaining candidates.
- Tests (RED-first): `BTC` does NOT match `BTCDOM`; `BTC` matches `BTC-PERP`/`BTCUSDT`; venue
  preference; deterministic tie-break. REQ-PROV-077/078.
- @MX: `@MX:NOTE` on `fetch_derivatives` (matching contract).

### M7 — Mechanical fixes (F-25, F-26, F-27, F-28) [Low — land last on corrected behavior]

- **F-25** (REQ-PROV-079): Binance snap returns `(secs, name)`; `fetch_ohlc` divides `limit`
  by the snapped seconds. Pure test over a between-band input.
- **F-26** (REQ-PROV-080): `chain_fetch_ohlc` returns a distinct "no capable provider" error
  (new variant or synthesized from records) when a non-empty chain is all-unsupported. Test:
  a non-empty chain of all-unsupported providers → the error is NOT "empty provider chain".
- **F-27** (REQ-PROV-076): add `warn!`/`debug!` to the `last_updated`→`Utc::now()` and
  `max_supply` `.ok()` degradations; align optional strictness with M5. Test: log-emission or
  behavior-preserving assertion (degradation kept, item survives).
- **F-28** (REQ-PROV-081): iterate search/tickers/derivatives arrays by reference or via typed
  DTOs; remove `.as_array().cloned()` deep-clones. Behavior-preserving; existing wiremock
  tests stay green. Pure/clippy-verified (no behavior change).

## §G Anti-Patterns to avoid

- Editing `interval_to_seconds` to add `"daily"`/`"hourly"` — WRONG direction (F-20 fix stamps
  canonical, not extends the vocabulary).
- Reintroducing inline request/429/parse scaffolding for the F-22 endpoint — MUST use the
  Phase-3 shared helpers.
- Routing any Decimal parse through `f64` for the scientific fallback — MUST use
  `Decimal::from_scientific`.
- Shipping the bare single `UPDATE` migration — the collision-safe DELETE-shadowed-then-UPDATE
  form is the default (§ Migration Safety); the plain `UPDATE` is startup-fatal on a PK
  collision and is NOT shipped.
- Modifying `bitstamp.rs` — it is the read-only reference pattern, not in scope.

## §H Cross-References

- `research/idiomatic-rust.md` §6 Category D (F-20…F-28), §7 (Phase 4), §8 (deps + F-20 note).
- `SPEC-PROV-002/spec.md` — Phase-3 shared helpers (`transport.rs`) prerequisite.
- `SPEC-API-003` — `interval_to_seconds` canonical vocabulary (`@MX:ANCHOR`).
- `SPEC-DB-001` — `coin_candles` schema (migration `0020`, PK includes `interval`).
- Project memory `sqlx-migrate-embed-rebuild` — migration compile-time embedding.

## MX Tag Targets (summary — full contract in spec.md § @MX Annotation Targets)

| Site | Tag | Reason |
|------|-----|--------|
| Range stamp path (`coingecko_range_snap_interval` / `normalise_ohlc_item`) | `@MX:ANCHOR` | canonical-stamp contract; pairs with `interval_to_seconds` anchor |
| `Tier` decision authority (`config.rs`) | `@MX:ANCHOR` | single tier-decision point (header + base URL + capability) |
| `decimal_from_number` | `@MX:ANCHOR` (or update) | Decimal-only parse core; `from_scientific`, never `f64` |
| Binance `fetch_spot` | `@MX:NOTE` | `price` (`lastPrice`) + `volume_24h` + bid/ask from `/api/v3/ticker/24hr` |
| `fetch_derivatives` | `@MX:NOTE` | boundary + venue + deterministic-tie-break matching |
