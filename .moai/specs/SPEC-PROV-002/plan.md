# SPEC-PROV-002 — Implementation Plan

Development mode: **TDD** (brownfield RED → GREEN → REFACTOR — the provider clients already
carry wiremock coverage). This plan is ordered by **decision-reversibility**: the
highest-change-likelihood decisions (the pacer behavior contract, the new error variant,
the shared-helper type interfaces) lead; the mechanical endpoint migration and doc/tag
updates follow, so human review focuses on the decisions most likely to change.

## Goal

Harden the provider transport + pacer paths per REQ-PROV-050..064 — shared request-path
helpers (F-14), search/tickers pacer compliance (F-10), HTTP timeouts + UA (F-11, F-19),
transient classification (F-12), pacer honesty (F-13, F-17, F-18), startup pacer-row
validation (F-15) — WITHOUT a new migration, WITHOUT a new dependency, WITHOUT touching the
`Provider` trait's public surface, and WITHOUT introducing `f64` in any monetary path.

---

## 1. Pacer honesty — the F-13 fork (D3, highest stakes)

Highest change-likelihood decision: it changes the behavior of the single fleet-wide egress
governor (`acquire_slot`), which every outbound call depends on.

**Requirement:** REQ-PROV-060. **Site:** `acquire_slot` (`pacer/mod.rs:181-191`).

Current (`:184-189`):
```
let now = Utc::now();
let wait = next_at.signed_duration_since(now);
if wait > Duration::zero() {
    let ms = wait.num_milliseconds().clamp(0, 60_000) as u64;   // silent truncation
    tokio::time::sleep(StdDuration::from_millis(ms)).await;
}
```

**Change (D3 — sleep the full computed wait + surface backlog):** the atomic UPDATE already
advanced `next_allowed_at`, so the reservation is authoritative — firing before it is the
defect. Remove the `.clamp(0, 60_000)`; sleep the full `wait`. When the computed wait
exceeds the former 60 s ceiling, `warn!` and increment
`pacer_backlog_wait_exceeded_total{provider}` (observability only — the metric/warn changes
nothing about how long we sleep).

Extract the timing decision as a **pure, DB-free core** (the REQ-PROV-060 unit-test target),
mirroring the existing `pacer_decision` pure-core style:
```
const PACER_BACKLOG_WARN_MS: i64 = 60_000;   // former clamp ceiling — now an observability threshold, not a truncation

/// Given the reserved instant and now, return how long to sleep and whether the wait
/// exceeded the observability threshold. NEVER truncates — the reservation is authoritative.
fn sleep_plan(next_at: DateTime<Utc>, now: DateTime<Utc>) -> (StdDuration, bool /* backlog_exceeded */)
```
`acquire_slot` calls `sleep_plan`, sleeps the returned duration, and (when the flag is set)
`warn!` + increments the counter.

**Trade-off / justification:** the rejected fork — a `Backlogged` error that truncates and
returns — reopens exactly the burst the pacer exists to prevent, and forces every call site
to handle a new failure mode for what is actually correct backpressure. Sleeping the full
wait IS the correct backpressure; the wait is bounded in practice by the pacer's own
`min_gap_ms × queue-depth`. Shutdown-responsiveness of a multi-minute wait is a Phase 6
concern (F-41) — explicitly out of scope; the sleep stays shutdown-agnostic as today.
Reversibility of D3 is HIGH (delete a `.clamp`, add a pure fn + a metric), which is why it
leads this plan.

**@MX:WARN** update on `acquire_slot` (see §10).

## 2. New pacer error variant + blocked-path race (D4)

New type-interface change (a new `AcquireSlotError` variant) consumers can observe.

**Requirement:** REQ-PROV-062. **Sites:** `AcquireSlotError` (`pacer/mod.rs:118-132`),
blocked-path fallback (`pacer/mod.rs:202-216`).

Add the variant:
```
#[error("provider '{0}' pacer row was contended (block lapsed mid-check); retry")]
Contended(String),
```
In the `None` arm's diagnostic re-SELECT (`:202-216`): keep `NotFound` **only** for
`row == None` (genuinely absent). When the row exists but neither the cooldown gate
(`cooldown_until > now`) nor the credit gate (`credits_used >= limit`) fires — the current
`// Fallback — treat as NotFound` branch at `:214-215` — return `Contended(provider)`
instead. `Contended` is transient-by-nature: the block lapsed, so a retry would likely
succeed. (Whether workers auto-retry on `Contended` is a SCHED consumer choice; this SPEC
only supplies the honest label — no retry loop is added inside `acquire_slot`.)

Consumer sweep: `grep -rn "AcquireSlotError::" src/` to confirm every `match` on the error
is either non-exhaustive (has a `_ =>`) or gains a `Contended` arm — a compile-time check.

## 3. Monotonic cooldown — GREATEST (F-17)

SQL-semantics change on the shared cooldown setter.

**Requirement:** REQ-PROV-061. **Site:** `signal_cooldown` (`pacer/mod.rs:226-242`).

Current: `SET cooldown_until = $2` (unconditional). **Change:**
```
SET cooldown_until = GREATEST(COALESCE(cooldown_until, 'epoch'::timestamptz), $2),
    updated_at = now()
WHERE provider = $1
```
A later, shorter cooldown can no longer truncate an earlier, longer one; a NULL existing
cooldown coalesces to epoch so the new value always wins on first signal. Behavior-preserving
for the common single-signal case; only the shorten-an-existing case changes.

**@MX:WARN** add on `signal_cooldown` (see §10) — `signal_cooldown` is currently untagged.

## 4. Transient error classification (F-12)

Consumer-visible behavior contract — SPEC-SCHED-001 worker retry consumes `is_transient`.

**Requirements:** REQ-PROV-058/059. **Site:** `ProviderError::is_transient`
(`providers/mod.rs:196-204`).

Current: `RateLimited | Network(_) | Http { .. }` all transient. **Change:**
```
pub fn is_transient(&self) -> bool {
    match self {
        ProviderError::RateLimited | ProviderError::Network(_) => true,
        ProviderError::Http { status, .. } => matches!(status, 408 | 425 | 429 | 500..=599),
        _ => false,
    }
}
```
`Http` is `{ status: u16, body }` (confirmed via `bitstamp.rs:119`). Pure, DB-free — the
REQ-PROV-058/059 test target: a table-driven matrix asserting 400/401/403/404 → `false`;
408/425/429/500/503 → `true`; `RateLimited`/`Network` → `true`.

## 5. Shared request-path helpers — the new type surface (D1, D2)

New type interfaces the whole provider module will depend on; high change-likelihood, so
reviewed before the mechanical migration that consumes them.

**Requirements:** REQ-PROV-050/051/055/057. **Site:** new module `src/providers/transport.rs`
(declared `mod transport;` in `providers/mod.rs`).

**(a) `build_client()` (REQ-PROV-055/056/057) — zero-guarded timeout resolution.**
```
pub fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .gzip(true)
        .timeout(Duration::from_secs(config::provider_http_timeout_secs()))          // guarded > 0, default 30
        .connect_timeout(Duration::from_secs(config::provider_http_connect_timeout_secs())) // guarded > 0, default 10
        .user_agent(concat!("crypto-collector/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("reqwest client")
}
```
New `src/config.rs` helpers `provider_http_timeout_secs()` (env `PROVIDER_HTTP_TIMEOUT_SECS`,
default 30) and `provider_http_connect_timeout_secs()` (env `PROVIDER_HTTP_CONNECT_TIMEOUT_SECS`,
default 10). These MUST be a **zero-guarded** resolution — NOT a bare `parse_env_u64(var,
default)`. The existing `parse_env_u64` (`config.rs:590`) returns `0` for an explicit
`PROVIDER_HTTP_TIMEOUT_SECS=0` (a valid parse: `"0".parse().ok() == Some(0)`), which would
build a `Duration::from_secs(0)` (unbounded) client and violate REQ-PROV-056. Use a dedicated
`resolve_timeout_secs(var, default)` guard (NonZero-style — always returns a strictly-positive
value) that treats a resolved `0` (whether from an explicit `=0` or an unparseable value) as
**fall back to the default**. Rationale: REQ-PROV-056 guarantees a positive timeout, so `0` is
not a valid client-timeout input — it is coerced to the default rather than passed through. The
guard is a pure, unit-testable core. `CoinGeckoClient::new`/`BinanceClient::new`/`BitstampClient::new`
each drop their inline builder and call `transport::build_client()`.

**(b) `paced<T>()` (REQ-PROV-050).** Generalises Bitstamp's `acquire()` + `signal_rate_limit()`
(`bitstamp.rs:245-256`) across providers — the prelude + 429 postlude around a provider call:
```
pub async fn paced<T, F, Fut>(
    pool: &PgPool,
    throttle: &LocalThrottle,
    provider: &str,
    call: F,
) -> Result<T, ProviderError>
where F: FnOnce() -> Fut, Fut: Future<Output = Result<T, ProviderError>> {
    throttle.acquire().await;
    pacer::acquire_slot(pool, provider).await.map_err(ProviderError::Pacer)?;
    match call().await {
        Err(ProviderError::RateLimited) => {
            let _ = pacer::signal_cooldown(pool, provider, config::pacer_cooldown_ms(provider)).await;
            Err(ProviderError::RateLimited)
        }
        other => other,
    }
}
```
This is byte-for-byte the behavior of each inline `acquire → call → match RateLimited =>
{cooldown; signal; return}` block (compare `coingecko.rs:890-908`), so migrating onto it is
behavior-preserving. Exact closure/`Future` signature is a run-phase micro-decision.

**(c) `get_json<T: DeserializeOwned>()` (REQ-PROV-051).** The response epilogue:
```
pub async fn get_json<T: DeserializeOwned>(resp: reqwest::Response, ctx: &str) -> Result<T, ProviderError> {
    let status = resp.status().as_u16();
    if status == 429 { return Err(ProviderError::RateLimited); }
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ProviderError::Http { status, body });
    }
    resp.json::<T>().await.map_err(|e| ProviderError::Parse(format!("{ctx} parse error: {e}")))
}
```
Mirrors Bitstamp's inline epilogue (`bitstamp.rs:108-126`) exactly. Endpoints that
special-case a status (e.g. Bitstamp's `404 → Ok(vec![])`, `bitstamp.rs:114-116`) keep that
branch at the call site before delegating the generic tail to `get_json`.

**@MX:ANCHOR** on `paced` + `get_json` and on `build_client` (see §10).

## 6. Search/tickers pacer compliance (F-10)

Targeted behavior fix riding the §5 helpers.

**Requirements:** REQ-PROV-053/054. **Sites:** `search_coins`/`fetch_coin_tickers` client
methods (`coingecko.rs:382-475`) + trait impls (`coingecko.rs:1092-1106`).

The trait impls currently delegate straight to the client with no prelude. Route them
through `paced()` so a slot is consumed (REQ-PROV-053), and make the **client** methods
distinguish 429 from other non-success so the 429 postlude fires:

- In the client methods, change the `if !status.is_success()` degrade-to-empty branch
  (`:398-411`, `:462-475`) to first check `status == 429`: on 429 return
  `Err(ProviderError::RateLimited)` (so `paced()`'s postlude calls `signal_cooldown`); on
  any OTHER non-success keep the existing `warn!` + `Ok(vec![])` degradation (REQ-PROV-005).
- In the trait impls, wrap the client call in `paced(&self.pool, &self.local_throttle,
  "coingecko", || self.client.search_coins(q, cap))` and, because REQ-PROV-005 wants search
  to still degrade to empty rather than propagate an error to the caller, map a returned
  `Err` back to `Ok(vec![])` at the trait boundary **after** the cooldown signal has fired
  inside `paced()`. This **broadens** the trait-boundary degradation: `search_coins` /
  `fetch_coin_tickers` today propagate `Network` and pacer `Cooldown` errors, and after this
  change ALL upstream errors (429, other HTTP, `Network`, pacer `Cooldown`) degrade to
  `Ok(vec![])` at the trait boundary. The `Network`/`Cooldown` paths are covered-by-construction
  by the same `Err(_) => Ok(vec![])` arm as the 429 path — no extra wiremock case is required
  beyond the existing 429 and non-429 coverage. End-to-end behavior is unchanged (the API
  handler already returns 200-empty, out-of-scope F-34), so this is a trait-contract
  clarification, not a behavior change. Net effect: slot consumed + cooldown signalled on 429 +
  empty result preserved for every error.

This is the one place where "degrade the result" (REQ-PROV-005, preserved) and "signal the
cooldown" (the F-10 bug fix) are both satisfied — the signal happens inside `paced()` before
the trait-boundary degrade-to-empty.

## 7. HTTP timeouts + User-Agent wiring (F-11, F-19)

Mechanical once §5(a) exists.

**Requirements:** REQ-PROV-055/056. Replace the three inline
`reqwest::Client::builder().gzip(true).build().expect(...)` sites (`coingecko.rs:160-163`,
`binance.rs:33-36`, `bitstamp.rs:69-72`) with `transport::build_client()`. Verify
REQ-PROV-056 via `grep -rn "Client::builder" src/providers/` → only `transport.rs` remains,
and `grep -rn "timeout" src/providers/` → shows the shared constructor.

## 8. Startup pacer-row validation (F-15)

New startup behavior; moderate change-likelihood.

**Requirements:** REQ-PROV-063/064. **Sites:** new `pacer::validate_pacer_rows`,
`main.rs:203-227` (Step 8).

Add to `pacer/mod.rs`:
```
/// Return chain members that have NO upstream_request_pacer row (REQ-PROV-063).
pub async fn missing_pacer_rows(pool: &PgPool, providers: &[String]) -> Result<Vec<String>, sqlx::Error> {
    let present: Vec<String> = sqlx::query_scalar(
        "SELECT provider FROM upstream_request_pacer WHERE provider = ANY($1)",
    ).bind(providers).fetch_all(pool).await?;
    Ok(providers.iter().filter(|p| !present.contains(p)).cloned().collect())
}
```
Wire into `main.rs` immediately after `build_chain` (Step 8, `:203-227`) — which is AFTER
`migrate_with_retry` returns `migrated == true` (Step 7, `:187-201`), so the DB is confirmed
reachable and the lazy-pool / DB-down resilience is untouched. On a non-empty missing set,
return `Err(anyhow!("provider(s) missing upstream_request_pacer row: {missing:?} — add rows
before starting"))` via the existing `.context(...)` fail-fast path (same shape as the
`build_chain` error handling at `:210-224`, which already best-effort raises a startup alarm
then exits non-zero). Because this returns before `health_state.set_ready()` (Step 10,
`:371-372`), readiness never flips — REQ-PROV-064.

**@MX:NOTE** on the check site (see §10).

## 9. Mechanical endpoint migration onto the helpers (lowest change-likelihood)

Deferred to the bottom: this is the largest diff but the most mechanical and the most
test-covered, so it warrants the least design review.

**Requirement:** REQ-PROV-052. Migrate every CoinGecko endpoint (`fetch_spot`,
`fetch_ohlc`, `fetch_ohlc_range`, `fetch_coin_metadata`, `fetch_coin_market`,
`fetch_derivatives`, plus the client-side `fetch_*` methods) onto `paced()` +
`get_json()`, removing the 6 inline cooldown blocks (`coingecko.rs:~885-1073`) and the 6
inline status/parse epilogues (`coingecko.rs:~220-539`). Do the same for Binance
(`binance.rs:~288-389`) and fold Bitstamp's `acquire()`/`signal_rate_limit()` into the
shared `paced()` frame. Behavior is preserved — the existing wiremock tests are the
regression guard and MUST stay green (update ONLY the two search/tickers-429 tests where the
intended behavior changed, per §6). Verify REQ-PROV-052 with a grep that no inline
`Err(ProviderError::RateLimited) => {` cooldown block remains outside `transport.rs`.

## 10. MX Tag Targets

| Tag | Site | Text (intent) | Sub-lines |
|-----|------|---------------|-----------|
| `@MX:ANCHOR` | `transport::paced` / `transport::get_json` | shared request-path frame: all provider endpoints route through these — single enforcement point for throttle + slot + 429 postlude + response epilogue | `@MX:REASON` (drift prevention — F-10 arose from endpoints written outside the frame; fan_in ≥ 3), `@MX:SPEC SPEC-PROV-002 REQ-PROV-050 REQ-PROV-051 REQ-PROV-052` |
| `@MX:ANCHOR` | `transport::build_client` | timeout/UA invariant: every provider client is built here; none without a total-request timeout | `@MX:REASON` (availability — a client without a timeout can hang a worker), `@MX:SPEC SPEC-PROV-002 REQ-PROV-055 REQ-PROV-056` |
| `@MX:WARN` (update) | `acquire_slot` (`pacer/mod.rs:150-154`) | keep "single fleet-wide egress governor" accurate now that `paced()` is the standard enforcement point; sleep honours the full reserved wait (no 60 s truncation), backlog surfaced via `warn!` + `pacer_backlog_wait_exceeded_total` | `@MX:REASON` (early-fire = burst the pacer prevents), `@MX:SPEC` extend to include `SPEC-PROV-002 REQ-PROV-060` |
| `@MX:WARN` (add) | `signal_cooldown` | monotonic-cooldown invariant: `GREATEST` never shortens an existing cooldown (`signal_cooldown` is currently untagged — this is a new tag, not an update) | `@MX:REASON` (a shortened cooldown reopens the 429 window), `@MX:SPEC SPEC-PROV-002 REQ-PROV-061` |
| `@MX:NOTE` | `main.rs` startup pacer-row check | runs after migrations succeed, before `set_ready`; a missing pacer row fails readiness naming the member | `@MX:SPEC SPEC-PROV-002 REQ-PROV-063 REQ-PROV-064` |

Also update the provider-module docs (the `providers/mod.rs` / `transport.rs` module-level
doc comments) to describe the shared request path, the timeout/UA defaults, and the
`PROVIDER_HTTP_TIMEOUT_SECS` / `PROVIDER_HTTP_CONNECT_TIMEOUT_SECS` overrides.

## 11. Test Plan (TDD, brownfield RED → GREEN → REFACTOR)

Pure unit tests (colocated, no DB):
- `is_transient` matrix (REQ-PROV-058/059): 400/401/403/404 → `false`; 408/425/429/500/503
  → `true`; `RateLimited`/`Network` → `true`. RED against current `main` (404 currently
  `true`).
- `sleep_plan` pure core (REQ-PROV-060): wait > 60 s → full duration + `backlog_exceeded =
  true`; wait ≤ 60 s → wait + `false`; wait ≤ 0 → zero + `false`. RED against current
  `main` (currently clamps to 60 s).

Wiremock tests (no DB — the existing provider-client test harness):
- Slow/hanging endpoint (`wiremock` fixed delay > configured timeout) → the request errors
  (`ProviderError::Network`) at the timeout instead of hanging (REQ-PROV-055/056). Use a
  short test-only timeout override so the test is fast.
- `search_coins` 429 (wiremock returns 429) → assert the trait impl returns `Ok(vec![])`
  (degradation preserved) AND — with a fake/stub pool or a `paced`-level unit — that
  `signal_cooldown` was invoked (REQ-PROV-053/054). The DB-observable half (cooldown row
  set) is the DB-gated test below.
- Behavior-preservation: the pre-existing CoinGecko/Binance/Bitstamp wiremock suites MUST
  stay green through the §9 migration (the regression guard).

DB-gated integration tests (`#[ignore]` + `DATABASE_URL`, run `--test-threads=1` — the
DB-gated suite shares a global claim queue, per CLAUDE.md Integration Tests):
- Search/tickers 429 → pacer row's `cooldown_until` is set after the call (REQ-PROV-054),
  and `next_allowed_at` advanced (slot consumed, REQ-PROV-053).
- `signal_cooldown` monotonicity (REQ-PROV-061): set a long cooldown, then a shorter one →
  the long `cooldown_until` survives (`GREATEST`).
- Blocked-path race → `Contended`, genuinely-absent row → `NotFound` (REQ-PROV-062). The
  `Contended` path is race-timing-sensitive; assert at minimum that an absent provider name
  returns `NotFound` and a present-but-lapsed-block row does not return `NotFound`.
- Startup validation (REQ-PROV-063/064): `missing_pacer_rows(pool, [chain member without a
  row])` returns that member; a full chain returns empty.

Quality gate: `cargo test`, `cargo clippy --all-targets --all-features -- -D warnings`,
`cargo fmt --check` — all clean.

## 12. Risks, Trade-offs & Dependencies

- **F-10 REQ-PROV-005 vs cooldown-signal tension.** The one subtlety (§6): search must both
  degrade to empty AND signal cooldown on 429. The order is load-bearing — the signal fires
  inside `paced()` before the trait-boundary degrade-to-empty. Getting the order wrong
  re-introduces the F-10 bug (no signal) while looking correct (still empty).
- **Migration diff size vs risk.** §9 is a large but mechanical diff; the risk is a silent
  behavior change during extraction. Mitigation: the existing wiremock suites are the
  regression guard and MUST stay green; only the two intended-change tests (search/tickers
  429) are updated.
- **`is_transient` reclassification is behaviorally load-bearing for SCHED-001.** Making
  4xx permanent changes worker retry behavior — a permanent 404 will now stop being retried.
  This is the intended fix (F-12) but is a behavior change to a consumer; the acceptance
  matrix pins it.
- **Full-wait sleep unbounded in theory.** Under extreme backlog `acquire_slot` can sleep
  minutes (D3). This is correct backpressure; shutdown-responsiveness of long waits is a
  Phase 6 concern (F-41), explicitly deferred.
- **`Contended` variant is additive.** Every existing `match` on `AcquireSlotError` must
  gain a `Contended` arm or already be non-exhaustive — a compile-time check, no runtime
  risk.
- **No migration / no dependency / no API change / trait surface preserved** — smallest
  blast radius consistent with the scope; all changes are internal to `src/providers/`,
  `src/pacer/`, `src/config.rs`, and the `main.rs` startup wiring.
- **Sequencing:** independent of Phases 1–2 (both COMPLETE). **Phase 4 depends on this
  phase** — the shared helpers + client constructor are Phase 4's landing site
  (`research/idiomatic-rust.md` §8). Commit-direct-to-main per CLAUDE.local.md (Route A,
  Hybrid Trunk); no feature branch, no per-phase PR (Tier M default).

## 13. PRESERVE list (scope discipline)

Touch ONLY:
- `src/providers/transport.rs` (new)
- `src/providers/coingecko.rs`, `src/providers/binance.rs`, `src/providers/bitstamp.rs`
- `src/providers/mod.rs` (`is_transient`, add `mod transport;`)
- `src/pacer/mod.rs`
- `src/config.rs` (new timeout env helpers only)
- `src/main.rs` (Step 8 pacer-row check wiring only)
- their colocated tests + any `#[ignore]` DB integration test file the project convention
  places pacer/provider DB tests in (mirror where SPEC-PROV-001's pacer DB tests live —
  `src/pacer/mod.rs` `#[ignore]` tests).

Do NOT modify: `src/providers/coinbase.rs` / `kraken.rs` (stub providers — no request path
to migrate unless they carry a real client; verify with a quick read before deciding),
`src/api/*` (no API change), any migration, `Cargo.toml` (no new dependency), the
`Provider` trait's public method set/signatures (Phase 7), `src/collectors/*` worker
dispatch, or any unrelated file.
