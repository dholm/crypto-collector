# SPEC-PROV-002 — Acceptance Criteria

Every criterion is testable. DB-gated tests follow the project convention: `#[ignore]` +
`DATABASE_URL`, run with `--test-threads=1` (the DB-gated suite shares a global claim queue,
per CLAUDE.md Integration Tests). Pure and wiremock tests run under plain `cargo test`.

Development mode is **TDD** (brownfield) — every criterion below is written as a testable
assertion, and the RED-first tests (transient matrix, full-wait sleep, search-429) MUST fail
against current `main` before the fix. AC → REQ traceability is stated per scenario;
`acceptance.md` is the SSOT for AC count.

---

## Scenario 1 — Shared request-path helpers: single frame, behavior preserved

**Covers:** REQ-PROV-050, REQ-PROV-051, REQ-PROV-052 · **AC-PROV-050** · grep + wiremock (behavior-preservation)

- **Given** the CoinGecko, Binance, and Bitstamp providers after migration onto the shared
  `src/providers/transport.rs` helpers,
- **When** the source is inspected,
- **Then** `transport::paced()` (throttle + `acquire_slot` prelude + 429→`signal_cooldown`
  postlude) and `transport::get_json()` (429→`RateLimited`; non-success→`Http{status,body}`;
  decode→`Parse`) exist and are the request-path frame,
- **And** `grep -rn "Err(ProviderError::RateLimited) => {" src/providers/` finds **no** inline
  cooldown block outside `transport.rs` (all endpoints route through `paced()`),
- **And** every pre-existing CoinGecko/Binance/Bitstamp wiremock test still passes unchanged
  (behavior preserved through the mechanical migration — only the two search/tickers-429
  tests are updated, per Scenario 2).

## Scenario 2 — Search/tickers pacer compliance: slot consumed + cooldown signalled on 429

**Covers:** REQ-PROV-053, REQ-PROV-054 · **AC-PROV-053** · DB-gated + wiremock

- **Given** the CoinGecko provider and a stubbed upstream `/api/v3/search` (or
  `/coins/{id}/tickers`) that returns HTTP 429,
- **When** `search_coins` (or `fetch_coin_tickers`) is invoked through the trait,
- **Then** the call consumes a pacer slot — the provider's `upstream_request_pacer.next_allowed_at`
  has advanced (DB-gated, slot prelude, REQ-PROV-053),
- **And** the provider's `upstream_request_pacer.cooldown_until` is set (DB-gated —
  `signal_cooldown` fired on the 429 before degrading, REQ-PROV-054),
- **And** the returned result is still an **empty** list (`Ok(vec![])`) — the trait-boundary
  degradation broadens so ALL upstream errors (429 / other-HTTP / `Network` / pacer `Cooldown`)
  degrade to empty; the behavior added on 429 is the `signal_cooldown` call (REQ-PROV-005
  empty-result contract preserved),
- **And** (wiremock, no DB) a non-429 upstream error still degrades to empty with a `warn!`
  and does NOT signal cooldown.

## Scenario 3 — HTTP timeout: a hanging upstream errors instead of blocking forever

**Covers:** REQ-PROV-055, REQ-PROV-056, REQ-PROV-057 · **AC-PROV-055** · wiremock

- **Given** a provider client built via `transport::build_client()` with a short test-only
  `PROVIDER_HTTP_TIMEOUT_SECS` and a wiremock endpoint that delays its response beyond that
  timeout,
- **When** an outbound request is made,
- **Then** the request **errors at the configured timeout** (surfacing as
  `ProviderError::Network`) instead of hanging indefinitely (REQ-PROV-055),
- **And** `grep -rn "timeout" src/providers/` shows timeouts applied via the shared
  constructor, and `grep -rn "Client::builder" src/providers/` shows only `transport.rs`
  constructs a client — no provider client is built without a total-request timeout
  (REQ-PROV-056),
- **And** the constructor attaches `User-Agent: crypto-collector/<CARGO_PKG_VERSION>` and
  honours `PROVIDER_HTTP_TIMEOUT_SECS` / `PROVIDER_HTTP_CONNECT_TIMEOUT_SECS` overrides,
  falling back to defaults 30 s / 10 s (REQ-PROV-057).

## Scenario 4 — Transient classification: permanent 4xx no longer retried (pure, no DB)

**Covers:** REQ-PROV-058, REQ-PROV-059 · **AC-PROV-058** · pure test

- **Given** `ProviderError::is_transient`,
- **When** it is evaluated across a status matrix,
- **Then** `Http{status}` is transient (`true`) for `408`, `425`, `429`, `500`, `503`
  (REQ-PROV-058),
- **And** `Http{status}` is permanent (`false`) for `400`, `401`, `403`, `404` (REQ-PROV-059),
- **And** `RateLimited` and `Network(_)` remain transient (`true`) unchanged,
- **And** this test FAILS against current `main` (where `404` is currently `true`) — proving
  it tests the defect (RED-first).

## Scenario 5 — Pacer honesty: full-wait sleep, monotonic cooldown, honest race label

**Covers:** REQ-PROV-060, REQ-PROV-061, REQ-PROV-062 · **AC-PROV-060** · pure + DB-gated

**Sub-case 5a — full-wait sleep (REQ-PROV-060, pure):**
- **Given** the pure `sleep_plan(next_at, now)` core,
- **When** the computed wait exceeds the former 60 s ceiling,
- **Then** it returns the **full** wait duration (no truncation to 60 s) and
  `backlog_exceeded = true`; a wait ≤ 60 s returns the wait and `false`; a non-positive wait
  returns zero and `false`,
- **And** this FAILS against current `main` (which clamps to 60 000 ms) — RED-first.

**Sub-case 5b — monotonic cooldown (REQ-PROV-061, DB-gated):**
- **Given** a provider whose `cooldown_until` is set to a long future instant,
- **When** `signal_cooldown` is called with a **shorter** cooldown,
- **Then** the existing longer `cooldown_until` survives (`GREATEST(COALESCE(..,'epoch'),$2)`);
  a first signal against a NULL cooldown always applies.

**Sub-case 5c — honest race label (REQ-PROV-062, DB-gated):**
- **Given** the blocked-path fallback in `acquire_slot`,
- **When** the diagnostic re-SELECT finds **no** row for the provider,
- **Then** `AcquireSlotError::NotFound(provider)` is returned,
- **And** when the row **exists** but neither the cooldown gate nor the credit gate explains
  the block (a lapsed-block race), `AcquireSlotError::Contended(provider)` is returned — the
  fallback no longer mislabels a contended, present row as `NotFound`.

## Scenario 6 — Startup pacer-row validation: missing row fails readiness naming the member

**Covers:** REQ-PROV-063, REQ-PROV-064 · **AC-PROV-063** · DB-gated

- **Given** a provider chain whose members are configured and the DB migrations have
  succeeded,
- **When** startup validates pacer rows via `missing_pacer_rows(pool, chain_names)`
  (`SELECT provider FROM upstream_request_pacer WHERE provider = ANY($1)`),
- **Then** a chain member **without** a pacer row is returned in the missing set, and startup
  fails with an error **naming that member** (REQ-PROV-063/064),
- **And** a chain whose members all have rows returns an empty missing set and startup
  proceeds to `set_ready` (REQ-PROV-063),
- **And** the check runs only AFTER `migrate_with_retry` succeeds, so a DB-down startup keeps
  `/healthz/live` answerable during retry (resilience unchanged — REQ-PROV-064).

## Edge Cases

- **Non-429 upstream error on search/tickers.** Degrades to empty with `warn!`, no cooldown
  signal (only 429 signals cooldown). This includes `Network` and pacer `Cooldown` errors —
  all upstream errors are covered-by-construction by the same `Err(_) => Ok(vec![])`
  trait-boundary arm, so no extra wiremock case is required beyond the existing 429 / non-429
  coverage. End-to-end behavior is unchanged (the API handler already returns 200-empty,
  out-of-scope F-34). (REQ-PROV-054)
- **Endpoint with a status special-case (Bitstamp 404 → empty).** The call site keeps its
  `404 → Ok(vec![])` branch before delegating the generic tail to `get_json`; the special
  case is preserved. (REQ-PROV-051)
- **First cooldown signal (NULL existing).** `GREATEST(COALESCE(cooldown_until,'epoch'),$2)`
  applies the new value. (REQ-PROV-061)
- **Genuinely-absent pacer row at runtime.** `acquire_slot` still returns `NotFound` (not
  `Contended`) — the two are distinguished by whether the re-SELECT finds a row. (REQ-PROV-062)
- **DB unreachable at startup.** The pacer-row check is unreachable until migrations succeed;
  liveness stays answerable throughout the retry window. (REQ-PROV-064)
- **Timeout env unset vs explicit `0`.** An **unset** `PROVIDER_HTTP_TIMEOUT_SECS` /
  `PROVIDER_HTTP_CONNECT_TIMEOUT_SECS` falls back to the defaults (30 s / 10 s). An
  **explicit `=0`** (or an unparseable value) is likewise guarded back to the default by the
  pure `resolve_timeout_secs` helper — it is NOT passed through as a zero-duration timeout. In
  both cases the built client's applied timeout is **strictly positive** (never
  `Duration::ZERO`), so a client is never built with an unbounded timeout. The guard is a pure,
  unit-testable core (assert `resolve_timeout_secs("VAR", 30) == 30` for unset / `"0"` /
  unparseable, and `== 45` for `"45"`). (REQ-PROV-056/057)

## Quality Gate — AC-PROV-QG

**Covers:** all REQs · non-negotiable

- `cargo test` — all suites pass (new pure + wiremock tests green; DB-gated tests green with
  `--test-threads=1` against a live Postgres).
- `cargo clippy --all-targets --all-features -- -D warnings` — zero warnings.
- `cargo fmt --check` — clean.
- No new migration, no new dependency (`Cargo.toml` diff empty), no `f64` in any monetary
  path (Decimal-only, REQ-PROV-012).
- The `Provider` trait's public method set and signatures are unchanged (trait redesign is
  Phase 7).
- `grep -rn "timeout" src/providers/` shows timeouts applied via the shared constructor; no
  provider client is built without one.
- `grep -rn "Err(ProviderError::RateLimited) => {" src/providers/` finds no inline
  cooldown block outside `transport.rs`.

## Definition of Done

- [ ] REQ-PROV-050 — `transport::paced()` exists (throttle + `acquire_slot` prelude + 429→`signal_cooldown` postlude).
- [ ] REQ-PROV-051 — `transport::get_json<T: DeserializeOwned>()` exists (429→`RateLimited`, non-success→`Http`, decode→`Parse`).
- [ ] REQ-PROV-052 — all CoinGecko/Binance/Bitstamp endpoints route through the helpers; no inline cooldown/epilogue remains (Scenario 1 grep).
- [ ] REQ-PROV-053 — `search_coins`/`fetch_coin_tickers` consume a pacer slot (`next_allowed_at` advances) (Scenario 2, DB-gated).
- [ ] REQ-PROV-054 — search/tickers 429 signals `signal_cooldown` before degrading to empty; empty result preserved (Scenario 2, DB-gated).
- [ ] REQ-PROV-055 — shared `build_client()` applies timeout + connect-timeout + User-Agent to all three clients (Scenario 3).
- [ ] REQ-PROV-056 — no provider client built without a total-request timeout (Scenario 3 grep).
- [ ] REQ-PROV-057 — timeouts env-tunable (`PROVIDER_HTTP_TIMEOUT_SECS` / `PROVIDER_HTTP_CONNECT_TIMEOUT_SECS`), defaults 30 s / 10 s (Scenario 3).
- [ ] REQ-PROV-058 — `is_transient` true only for `408|425|429|500..=599`; RateLimited/Network unchanged (Scenario 4, pure).
- [ ] REQ-PROV-059 — permanent 4xx (400/401/403/404) not transient (Scenario 4, pure).
- [ ] REQ-PROV-060 — `acquire_slot` sleeps the full computed wait (no 60 s truncation) + `warn!`/metric on backlog; pure `sleep_plan` core (Scenario 5a).
- [ ] REQ-PROV-061 — `signal_cooldown` uses `GREATEST`; never shortens an existing cooldown (Scenario 5b, DB-gated).
- [ ] REQ-PROV-062 — blocked-path race returns `Contended`; `NotFound` reserved for absent row (Scenario 5c, DB-gated).
- [ ] REQ-PROV-063 — startup verifies every chain member has a pacer row after migrations (Scenario 6, DB-gated).
- [ ] REQ-PROV-064 — missing pacer row fails readiness naming the member; DB-down resilience preserved (Scenario 6, DB-gated).
- [ ] `sleep_plan` + `is_transient` extracted/kept as pure, DB-free unit-tested cores.
- [ ] provider-module docs + `@MX:ANCHOR` (shared frame, build_client) + `@MX:WARN` (acquire_slot, signal_cooldown) + `@MX:NOTE` (startup check) added/updated per plan.md § MX Tag Targets.
- [ ] AC-PROV-QG passes (fmt, clippy -D warnings, test, no migration/dependency, trait surface unchanged).
