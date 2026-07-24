---
id: SPEC-PROV-002
title: "Provider Transport Hardening & Pacer Compliance"
version: "0.1.0"
status: in-progress
created: 2026-07-24
updated: 2026-07-24
author: manager-spec
priority: High
phase: "v0.3.0"
module: "src/providers"
lifecycle: spec-anchored
tags: "providers, transport, pacer, http-timeout, rate-limit, refactor, tdd"
issue_number: null
related_specs: [SPEC-PROV-001, SPEC-DB-001, SPEC-SCHED-001]
tier: M
---

# SPEC-PROV-002 — Provider Transport Hardening & Pacer Compliance

Behavioral-correctness hardening of the provider transport layer and the fleet-wide
egress pacer, plus a shared request-path refactor that removes the duplication which
produced this class of drift in the first place. Every outbound HTTP call is made to
route through the pacer again; every provider client gains HTTP timeouts; transient-error
classification is corrected; the duplicated request/429/parse scaffolding is extracted
into two shared helpers; and startup gains a pacer-row completeness check.

Findings addressed: **F-10** (High, search/tickers bypass the pacer + swallow 429),
**F-11** (High, no HTTP timeouts on any provider client), **F-12** (Medium, `is_transient`
treats all `Http{..}` as transient), **F-13** (Medium, `acquire_slot` silently clamps the
reserved wait to 60 s), **F-14** (Medium, request/429/parse scaffolding duplicated —
the proven drift source), **F-15** (Medium, no startup validation of pacer rows),
**F-17** (Low, `signal_cooldown` can shorten an existing cooldown), **F-18** (Low,
blocked-path race mislabeled `NotFound`), **F-19** (Informational, no `User-Agent`) —
Category C of `research/idiomatic-rust.md` §6 (lines 138–180). This is **Phase 3** of the
7-phase review-driven improvement roadmap (§7, line 382 of that document).

Schema contract: [SPEC-DB-001](../SPEC-DB-001/spec.md) — `upstream_request_pacer` columns
(`provider`, `next_allowed_at`, `min_gap_ms`, `cooldown_until`, `credit_limit`,
`credits_used`, `credit_window_start`, `updated_at`). Pacer + provider-chain semantics
baseline: [SPEC-PROV-001](./spec.md — this SPEC extends the same file's REQ-PROV-0xx
block) — REQ-PROV-005 read-only degradation, REQ-PROV-040/045 single-governor egress,
REQ-PROV-041/042 cooldown, REQ-PROV-002/003 fail-fast chain. This SPEC **extends and
hardens** those requirements; it does not relax them. Consumers of the chain and pacer
are [SPEC-SCHED-001](../SPEC-SCHED-001/spec.md) workers (`live_poller`,
`collection_queue`, `backfill`) — their retry classification consumes `is_transient`
(F-12), so the reclassification is behaviorally load-bearing for them.

## Prerequisites / Sequencing

**Independent of Phases 1–2** ([SPEC-SCHED-002](../SPEC-SCHED-002/spec.md) worker retry &
backpressure, COMPLETE; [SPEC-CANDLE-002](../SPEC-CANDLE-002/spec.md) materializer &
projection integrity, COMPLETE) — this SPEC touches the provider transport + pacer paths
only, sharing no code with those phases; recommended after them but not hard-blocked.
**Phase 4 depends on this phase** (`research/idiomatic-rust.md` §8, lines 390–393, 397):
the shared `paced()`/`get_json()` helpers and the shared client constructor introduced
here are the natural landing site for the Phase-4 provider-data-correctness fixes (F-20/F-21
tier configuration lands where client construction is consolidated) and reduce the diff of
every Phase-4 change. This forward-dependency is normative — Phase 4 MUST NOT begin before
this SPEC's helpers exist.

## HISTORY

- 2026-07-24 (v0.1.0): Initial draft. Establishes **REQ-PROV-050..064** — the provider
  transport hardening & pacer compliance block, extending SPEC-PROV-001's REQ-PROV-0xx
  requirements: (Module 1) two shared request-path helpers `paced()` + `get_json()` in a
  new `src/providers/transport.rs`, onto which ~10 endpoint methods migrate mechanically
  (F-14); (Module 2) `search_coins`/`fetch_coin_tickers` route through the pacer prelude
  and signal cooldown on 429 (F-10); (Module 3) a shared client constructor applying
  timeout + connect-timeout + `User-Agent` to all three clients (F-11, F-19); (Module 4)
  status-discriminated `is_transient` (F-12); (Module 5) pacer honesty — full-wait sleep,
  monotonic `GREATEST` cooldown, `Contended` race variant (F-13, F-17, F-18); (Module 6)
  startup pacer-row completeness check wired after migrations (F-15). Brownfield — extends
  SPEC-PROV-001. Files: `src/providers/{transport(new),coingecko,binance,bitstamp,mod}.rs`,
  `src/pacer/mod.rs`, `src/config.rs`, `src/main.rs`. No new migration, no new dependency,
  no API/schema change, `Provider` trait public surface preserved.

---

## Goal

Restore the "**every outbound HTTP call routes through the pacer**" invariant
(REQ-PROV-040/045), make every provider client **time-bounded** so a black-holed upstream
can never hang a worker indefinitely, **classify transient errors correctly** so retry
logic does not spin on permanent 4xx, and **extract the duplicated request/429/parse
scaffolding into two shared helpers** so this class of drift cannot silently recur. After
this SPEC:

1. `grep -rn "timeout" src/providers/` shows timeouts applied via one shared constructor;
   no provider client is built without a total-request timeout and a connect timeout.
2. No endpoint method contains an inline `Err(RateLimited) => { cooldown; signal; return }`
   block or an inline status/parse epilogue — all route through the shared `paced()` and
   `get_json()` helpers.
3. `search_coins` / `fetch_coin_tickers` consume pacer slots on every call (the pacer
   row's `next_allowed_at` advances), and on an upstream 429 they call `signal_cooldown`
   before degrading to an empty result (result degradation per REQ-PROV-005 preserved;
   the missing cooldown signal is the bug).
4. `is_transient` treats `Http{status}` as transient only for `408 | 425 | 429 | 500..=599`;
   other 4xx are permanent. `RateLimited` / `Network` are unchanged.
5. `acquire_slot` honours the reservation it already made — it sleeps the full computed
   wait (no silent 60 s truncation) and surfaces sustained backlog via `warn!` + a metric;
   `signal_cooldown` never shortens an existing longer cooldown; the blocked-path race
   returns a dedicated `Contended` error instead of a misleading `NotFound`.
6. Startup verifies every chain member has an `upstream_request_pacer` row after
   migrations succeed and fails readiness with a clear message naming the missing member,
   WITHOUT breaking the DB-down-at-startup / lazy-pool / migration-retry resilience.

## Problem (Why)

- **F-10 — search/tickers bypass the pacer and swallow 429 (High).** `search_coins` /
  `fetch_coin_tickers` on `CoinGeckoProvider` delegate straight to the client
  (`coingecko.rs:1092-1106`) with **no** `local_throttle.acquire()` and **no**
  `pacer::acquire_slot` prelude — unlike every other trait method (compare `fetch_spot`
  at `:890-894`). Worse, the client methods degrade **any** non-success (including 429)
  to `Ok(vec![])` (`coingecko.rs:398-411, 462-475`) without ever calling `signal_cooldown`.
  These serve user-facing search endpoints, so volume is externally driven; a burst
  produces unmetered CoinGecko egress and can trip the very 429s the pacer exists to
  prevent — and the fleet never backs off.
- **F-11 — no HTTP timeouts on any provider client (High).** All three clients are
  `reqwest::Client::builder().gzip(true).build()` with no `.timeout()` / `.connect_timeout()`
  (`coingecko.rs:160-163`, `binance.rs:33-36`, `bitstamp.rs:69-72`); reqwest's default is
  no total-request timeout. A black-holed upstream hangs the calling worker indefinitely —
  after the pacer already charged a credit — with health probes green while collection
  silently stops. On a single-replica pod this is a real availability risk.
- **F-12 — `is_transient` classifies all `Http{..}` (incl. 4xx) as transient (Medium).**
  `ProviderError::is_transient` matches `RateLimited | Network(_) | Http { .. }` with no
  status discrimination (`providers/mod.rs:198-203`). SPEC-SCHED-001 worker retry logic
  consumes this, so a permanent `401`/`404` is retried as if transient.
- **F-13 — `acquire_slot` silently clamps the reserved wait to 60 s (Medium).** After the
  atomic reservation advances `next_allowed_at`, the caller sleeps `wait.clamp(0, 60_000)` ms
  (`pacer/mod.rs:184-189`). Under backlog, waits beyond 60 s are truncated and requests fire
  **before** their reserved slots — silently converting overload into exactly the burst the
  pacer prevents (31+ queued acquirers at a 2 s gap suffices).
- **F-14 — request/429/parse scaffolding duplicated (Medium, the proven drift source).**
  The `Err(RateLimited) => { cooldown; signal; return }` block appears 6× in `coingecko.rs`
  and again in `binance.rs`; the client-side status/parse epilogue (`429→RateLimited;
  !success→Http; json→Parse`) is copy-pasted across every endpoint. Per-site drift is
  already visible — F-10 exists precisely because two new endpoints were written outside
  the frame. Only Bitstamp factored helpers (`acquire()` / `signal_rate_limit()`,
  `bitstamp.rs:245-256`) — the model to generalise.
- **F-15 — no startup validation that every chain provider has a pacer row (Medium).** A
  missing `upstream_request_pacer` row surfaces as an error on **every** fetch at runtime;
  `build_chain` validates names only (`providers/mod.rs:349-381`). `main.rs` Step 8 builds
  the chain but never checks pacer-row completeness.
- **F-17 — `signal_cooldown` can shorten an existing longer cooldown (Low).** `SET
  cooldown_until = $2` unconditionally (`pacer/mod.rs:226-242`); a later, shorter signal
  truncates an earlier, longer one (multi-replica races, operator-set cooldowns).
- **F-18 — blocked-path race mislabeled `NotFound` (Low).** When the gated UPDATE matches
  no row and the diagnostic re-SELECT finds the block already lapsed, the fallback returns
  `NotFound(provider)` (`pacer/mod.rs:214-215`) — misleading for a "retry would have
  succeeded" race on a row that genuinely exists.
- **F-19 — no `User-Agent` on any client (Informational).** One
  `.user_agent(concat!("crypto-collector/", env!("CARGO_PKG_VERSION")))` per builder is
  cheap insurance (Bitstamp's WAF intermittently rejects default library UAs).

## Scope

In scope:
- **Shared request-path helpers (F-14)** — two async helpers in a new
  `src/providers/transport.rs` module: `paced<T>(...)` wrapping the throttle +
  `acquire_slot` prelude + the 429→`pacer_cooldown_ms`→`signal_cooldown` postlude around a
  provider call, and `get_json<T: DeserializeOwned>(...)` for the response epilogue
  (429→`RateLimited`, non-success→`Http{status,body}`, decode→`Parse`). All
  CoinGecko/Binance/Bitstamp endpoint methods migrate onto these (mechanical, behavior
  preserved; existing wiremock tests cover behavior). (REQ-PROV-050/051/052)
- **Pacer compliance for search/tickers (F-10)** — `search_coins` and `fetch_coin_tickers`
  route through the same throttle + `acquire_slot` prelude as every other method; on an
  upstream 429 they call `signal_cooldown` before degrading to empty (REQ-PROV-005 result
  degradation preserved). (REQ-PROV-053/054)
- **HTTP timeouts + User-Agent (F-11, F-19)** — one shared client-construction helper
  applies `.timeout(~30 s)`, a shorter `.connect_timeout(...)`, and
  `.user_agent(concat!("crypto-collector/", env!("CARGO_PKG_VERSION")))` to all three
  clients; timeout values env-tunable with sensible defaults. (REQ-PROV-055/056/057)
- **Transient error classification (F-12)** — `is_transient` treats `Http{status,..}` as
  transient only for `408 | 425 | 429 | 500..=599`; other 4xx permanent; `RateLimited` /
  `Network` unchanged. (REQ-PROV-058/059)
- **Pacer honesty (F-13, F-17, F-18)** — `acquire_slot` sleeps the full computed wait and
  surfaces backlog beyond the former ceiling via `warn!` + a metric (Decision D3);
  `signal_cooldown` uses `GREATEST(COALESCE(cooldown_until, 'epoch'), $2)` so it never
  shortens an existing cooldown; the blocked-path race returns a dedicated `Contended`
  variant, reserving `NotFound` for the genuinely-absent row. (REQ-PROV-060/061/062)
- **Startup pacer-row validation (F-15)** — after `build_chain`, a startup check verifies
  every chain member has an `upstream_request_pacer` row; a missing row fails startup /
  readiness with a clear message naming the member. Wired into `main.rs` AFTER migrations
  succeed and BEFORE readiness flips, preserving the DB-down / lazy-pool / migration-retry
  resilience. (REQ-PROV-063/064)
- **Keeping the pure decision cores pure** — the pacer sleep decision (full-wait vs
  observability threshold) and the transient-status classification are extracted or kept
  as pure, unit-testable cores in the existing style (`pacer_decision`, `is_transient`).
- **Module documentation + @MX tags** — update provider-module docs to describe the shared
  request path (timeout/UA defaults + env overrides), and update/add `@MX:ANCHOR`/`@MX:WARN`
  tags to encode the shared-helper enforcement point, the monotonic-cooldown invariant, and
  the startup pacer-row check.

Out of scope: see Exclusions.

## Decisions Restated (authoritative)

Settled in the plan-phase brief; encoded here in intent. Not to be re-litigated.

- **D1 — Helper module placement: a new `src/providers/transport.rs`.** The two shared
  helpers `paced<T>()` and `get_json<T>()` and the shared client constructor live in a new
  sibling module `transport.rs`, declared in `providers/mod.rs`. Rationale: it names the
  "Provider Transport Hardening" concern directly, keeps the request-path frame in one
  file all providers depend on (fan_in ≥ 3), and mirrors Bitstamp's already-factored
  `acquire()`/`signal_rate_limit()` shape (`bitstamp.rs:245-256`) generalised across
  providers. `paced()`/`get_json()` are the canonical enforcement point; direct
  `acquire_slot` calls remain valid but the helper is the drift-prevention frame.
- **D2 — Shared client constructor + env-tunable timeouts.** A single
  `transport::build_client()` applies `.gzip(true)`, `.timeout(...)`, `.connect_timeout(...)`,
  and `.user_agent("crypto-collector/<CARGO_PKG_VERSION>")`. Timeout defaults: **total
  30 s**, **connect 10 s**, tunable via `PROVIDER_HTTP_TIMEOUT_SECS` and
  `PROVIDER_HTTP_CONNECT_TIMEOUT_SECS` read through new `src/config.rs` helpers (matching
  the existing `config::*` env convention). A timeout surfaces as `ProviderError::Network`,
  which the chain and alarm already classify correctly.
- **D3 — F-13 fork resolved: sleep the full computed wait + surface backlog (NOT truncate).**
  The atomic reservation already advanced `next_allowed_at`, so the reservation is
  authoritative — firing before it is the actual defect. `acquire_slot` therefore sleeps the
  **full** computed wait (the `.clamp(0, 60_000)` is removed) and, when the computed wait
  exceeds the former 60 s ceiling, emits a `warn!` and increments a
  `pacer_backlog_wait_exceeded_total{provider}` counter for observability. This is the
  research-recommended primary option (`research/idiomatic-rust.md` F-13, line 156: "Sleep
  the full computed wait (the reservation is already made)"). The rejected alternative — a
  `Backlogged` error variant that truncates and returns — is NOT taken: truncation reopens
  the burst the pacer prevents; a distinct error would force every call site to handle a new
  failure mode for what is actually correct backpressure. The metric/warn adds observability
  WITHOUT changing behavior (no truncation). Shutdown-responsiveness of long pacer waits is
  a Phase 6 concern (drain/shutdown, F-41) — out of scope here; the sleep stays
  shutdown-agnostic exactly as today.
- **D4 — New pacer error variant name: `AcquireSlotError::Contended(String)`.** The
  blocked-path race (F-18) returns `Contended(provider)` when the gated UPDATE matched no
  row but the diagnostic re-SELECT finds the row exists and neither the cooldown nor the
  credit gate explains the block (the block lapsed between UPDATE and re-SELECT — a race).
  `NotFound(provider)` is reserved for the case where the diagnostic re-SELECT returns no
  row at all (genuinely absent). No `Backlogged` variant is added (see D3).

## Domain Model — affected sites (delta markers)

Delta markers: **[EXISTING]** relied upon unchanged, **[MODIFY]** changed, **[NEW]** net-new.

| Marker | Path | Role |
|--------|------|------|
| [NEW] | `src/providers/transport.rs` (new module) | `build_client()` shared constructor (timeout + connect-timeout + UA); `paced<T>()` prelude/postlude wrapper; `get_json<T: DeserializeOwned>()` response epilogue. The canonical request-path frame (REQ-PROV-050/051/055). |
| [MODIFY] | `src/providers/coingecko.rs:160-163` (`CoinGeckoClient::new`) | Build via `transport::build_client()` instead of the inline builder (REQ-PROV-055/056). |
| [MODIFY] | `src/providers/coingecko.rs` (endpoint methods, incl. `:220-539` epilogues, `:885-1073` cooldown blocks) | Migrate onto `paced()` + `get_json()`; remove the 6 inline cooldown blocks + inline status/parse epilogues (REQ-PROV-052). |
| [MODIFY] | `src/providers/coingecko.rs:382-475` (`search_coins`/`fetch_coin_tickers` client methods) + `:1092-1106` (trait impls) | Route through the throttle + `acquire_slot` prelude; on 429 signal cooldown before degrading to empty (REQ-PROV-053/054). |
| [MODIFY] | `src/providers/binance.rs:31-41` (`BinanceClient::new`) + endpoint methods (`:288-389`) | Build via `transport::build_client()`; migrate endpoints onto the helpers (REQ-PROV-052/055). |
| [MODIFY] | `src/providers/bitstamp.rs:67-77` (`BitstampClient::new`) + `:245-256` (`acquire`/`signal_rate_limit`) | Build via `transport::build_client()`; fold the already-factored helpers into the shared `paced()` frame (REQ-PROV-052/055). |
| [MODIFY] | `src/providers/mod.rs:196-204` (`ProviderError::is_transient`) | Status-discriminated: `Http{status,..} => matches!(status, 408 \| 425 \| 429 \| 500..=599)` (REQ-PROV-058/059). |
| [MODIFY] | `src/pacer/mod.rs:181-220` (`acquire_slot`) | Sleep the full computed wait + `warn!`/metric on backlog > former ceiling (REQ-PROV-060); blocked-path race returns `Contended` not `NotFound` (REQ-PROV-062). |
| [MODIFY] | `src/pacer/mod.rs:118-132` (`AcquireSlotError`) | Add `Contended(String)` variant (REQ-PROV-062). |
| [MODIFY] | `src/pacer/mod.rs:226-242` (`signal_cooldown`) | `SET cooldown_until = GREATEST(COALESCE(cooldown_until, 'epoch'), $2)` (REQ-PROV-061). |
| [NEW] | `src/pacer/mod.rs` (proposed pure fn) | Pure sleep-decision core: given `(next_at, now)` return `(StdDuration, backlog_exceeded: bool)` — the REQ-PROV-060 pure test target. |
| [NEW] | `src/pacer/mod.rs` (proposed) `validate_pacer_rows(pool, names)` | `SELECT provider FROM upstream_request_pacer WHERE provider = ANY($1)`; return the set of missing members (REQ-PROV-063). |
| [MODIFY] | `src/main.rs:203-227` (Step 8, after `build_chain`) | Call `validate_pacer_rows` after migrations succeed, before `set_ready`; a missing row fails startup with a message naming the member (REQ-PROV-063/064). |
| [MODIFY] | `src/config.rs` | New `provider_http_timeout_secs()` / `provider_http_connect_timeout_secs()` env helpers (REQ-PROV-057). |
| [EXISTING] | `src/config.rs:pacer_cooldown_ms(provider)` | Reused by `paced()` for the 429 postlude — unchanged. |
| [EXISTING] | `src/providers/mod.rs:349-381` (`build_chain`), trait surface | Provider trait public surface preserved (trait redesign is Phase 7). |

---

## Requirements (GEARS)

### Module 1 — Shared request-path scaffolding (F-14) [NEW/MODIFY]

- **REQ-PROV-050** [NEW] (Ubiquitous): The provider layer **shall** expose a shared async
  `paced<T>()` helper in `src/providers/transport.rs` that wraps a provider call with the
  standard prelude (`local_throttle.acquire()` then `pacer::acquire_slot(pool, provider)`)
  and the standard 429 postlude (on `ProviderError::RateLimited`, call
  `pacer::signal_cooldown(pool, provider, pacer_cooldown_ms(provider))` before returning
  the error), preserving the pre-existing behavior of the inline blocks it replaces.
- **REQ-PROV-051** [NEW] (Ubiquitous): The provider layer **shall** expose a shared async
  `get_json<T: DeserializeOwned>()` helper for the response epilogue that maps an HTTP 429
  to `ProviderError::RateLimited`, a non-success status to `ProviderError::Http { status,
  body }`, and a JSON decode failure to `ProviderError::Parse`.
- **REQ-PROV-052** [MODIFY] (Unwanted): After the migration, no CoinGecko/Binance/Bitstamp
  endpoint method **shall** contain an inline `Err(RateLimited) => { cooldown; signal;
  return }` cooldown block or an inline status/parse epilogue — every endpoint **shall**
  route through `paced()` and `get_json()`. Behavior **shall** be preserved everywhere
  except the six intended changes; existing wiremock tests **shall** continue to pass.

### Module 2 — Pacer compliance for search/tickers (F-10) [MODIFY]

- **REQ-PROV-053** [MODIFY] (Ubiquitous): `search_coins` and `fetch_coin_tickers` **shall**
  route through the same throttle + `acquire_slot` prelude as every other provider method,
  so a slot is consumed (the provider's `upstream_request_pacer.next_allowed_at` advances)
  before the outbound request.
- **REQ-PROV-054** [MODIFY] (Event-driven): When `search_coins` or `fetch_coin_tickers`
  receives an upstream HTTP 429, the system **shall** call `signal_cooldown` for the
  provider **before** degrading the result to empty. The trait-boundary degradation to
  `Ok(vec![])` **shall** broaden so that **all** upstream errors — HTTP 429, other HTTP
  statuses, `Network`, and pacer `Cooldown` — degrade to `Ok(vec![])` at the
  `search_coins`/`fetch_coin_tickers` trait boundary (today `Network` and pacer `Cooldown`
  are propagated), with `signal_cooldown` fired **before** degrading only on 429. End-to-end
  behavior is unchanged — the API handler already returns 200-empty (out-of-scope F-34) — so
  this is a trait-contract clarification, not an externally-observable behavior change; the
  `Network`/`Cooldown` degradation paths are covered-by-construction by the same match arm as
  the 429 path (REQ-PROV-005 empty-result contract preserved).

### Module 3 — HTTP timeouts + User-Agent (F-11, F-19) [NEW/MODIFY]

- **REQ-PROV-055** [NEW] (Ubiquitous): A single shared client constructor
  (`transport::build_client()`) **shall** apply a total-request `.timeout(...)`, a shorter
  `.connect_timeout(...)`, and a `.user_agent(concat!("crypto-collector/",
  env!("CARGO_PKG_VERSION")))` to the CoinGecko, Binance, and Bitstamp `reqwest::Client`s.
- **REQ-PROV-056** [MODIFY] (Unwanted): No provider `reqwest::Client` **shall** be
  constructed without a **strictly-positive** total-request timeout — `grep -rn "timeout"
  src/providers/` **shall** show timeouts applied via the shared constructor, no client
  builder **shall** remain that omits `.timeout(...)`, and the applied timeout **shall not**
  be `Duration::ZERO`. A resolved timeout value of `0` (from an explicit `=0` or an
  unparseable env value) is guarded back to the default rather than used to build a
  zero-duration (unbounded) client — see REQ-PROV-057.
- **REQ-PROV-057** [NEW] (Where — capability/config): Where the operator sets
  `PROVIDER_HTTP_TIMEOUT_SECS` or `PROVIDER_HTTP_CONNECT_TIMEOUT_SECS` to a **positive**
  value, the constructor **shall** use those values; where the variable is unset, unparseable,
  **or explicitly `0`**, the constructor **shall** fall back to the defaults (total 30 s,
  connect 10 s). The `src/config.rs` resolution helper **shall** guard a resolved `0` back to
  the default so a zero-duration timeout can never reach the client builder (REQ-PROV-056),
  following the existing env-config convention.

### Module 4 — Transient error classification (F-12) [MODIFY]

- **REQ-PROV-058** [MODIFY] (Ubiquitous): `ProviderError::is_transient` **shall** classify
  `Http { status, .. }` as transient only when `status` is one of `408 | 425 | 429 |
  500..=599`; `RateLimited` and `Network(_)` **shall** remain transient unchanged.
- **REQ-PROV-059** [MODIFY] (Unwanted): A permanent client error — an `Http { status, .. }`
  with a 4xx status other than `408 | 425 | 429` (e.g. `400`, `401`, `403`, `404`) **shall
  not** be classified as transient, so SPEC-SCHED-001 worker retry logic does not spin on a
  permanent failure.

### Module 5 — Pacer honesty (F-13, F-17, F-18) [MODIFY/NEW]

- **REQ-PROV-060** [MODIFY] (Ubiquitous): After the atomic reservation advances
  `next_allowed_at`, `acquire_slot` **shall** sleep the **full** computed wait (the
  `.clamp(0, 60_000)` truncation is removed) so a request never fires before its reserved
  slot; **and** when the computed wait exceeds the former 60 s ceiling it **shall** emit a
  `warn!` and increment a `pacer_backlog_wait_exceeded_total{provider}` metric (Decision D3,
  observability only — no behavior change). The sleep-duration decision **shall** be a pure,
  DB-free, unit-testable core.
- **REQ-PROV-061** [MODIFY] (Ubiquitous): `signal_cooldown` **shall** set `cooldown_until =
  GREATEST(COALESCE(cooldown_until, 'epoch'), $2)` so a later, shorter cooldown signal
  **shall not** shorten an existing longer cooldown.
- **REQ-PROV-062** [MODIFY] (Event-driven): When the gated `acquire_slot` UPDATE matches no
  row but the diagnostic re-SELECT finds the row exists and neither the cooldown gate nor
  the credit gate explains the block (a lapsed-block race), the system **shall** return
  `AcquireSlotError::Contended(provider)`; `NotFound(provider)` **shall** be returned only
  when the diagnostic re-SELECT finds no row at all (Decision D4).

### Module 6 — Startup pacer-row validation (F-15) [NEW]

- **REQ-PROV-063** [NEW] (Event-driven): When the provider chain is built at startup (after
  migrations have succeeded and the DB is confirmed reachable), the system **shall** verify
  that every chain member has a row in `upstream_request_pacer` via `SELECT provider FROM
  upstream_request_pacer WHERE provider = ANY($1)`.
- **REQ-PROV-064** [NEW] (Unwanted / Event-driven): When one or more chain members lack a
  pacer row, the system **shall** fail startup / readiness with a clear error naming the
  missing member(s), and **shall not** flip readiness to ready. This check **shall not**
  break the DB-down-at-startup / lazy-pool / migration-retry resilience — it runs only
  after `migrate_with_retry` reports success, and it **shall not** be reached while the DB
  is still unreachable.

---

## Edge Cases (summarized; full behavior in acceptance.md)

- **Slow / hanging upstream.** A wiremock endpoint delayed beyond the configured timeout
  causes the request to error (as `ProviderError::Network`) at the timeout instead of
  hanging the worker. (REQ-PROV-055/056)
- **Search 429.** `search_coins` receives a 429 → `signal_cooldown` is called (the pacer
  row's `cooldown_until` is set) → the result still degrades to empty. (REQ-PROV-053/054)
- **Permanent vs transient status.** `is_transient` returns `true` for 408/425/429/500/503
  and `false` for 400/401/403/404. (REQ-PROV-058/059)
- **Cooldown never shortened.** A `signal_cooldown` with a shorter cooldown after a longer
  one already set leaves the longer `cooldown_until` in place (`GREATEST`). (REQ-PROV-061)
- **Backlog beyond 60 s.** A computed wait > 60 s produces the full-length sleep duration
  and sets the backlog-exceeded flag (pure test); no truncation. (REQ-PROV-060)
- **Blocked-path race vs genuinely-absent row.** A lapsed-block race returns `Contended`;
  an entirely-missing pacer row returns `NotFound`. (REQ-PROV-062)
- **Missing pacer row at startup.** A chain member without a pacer row fails startup with a
  message naming the member; a chain whose members all have rows starts normally.
  (REQ-PROV-063/064)
- **DB unreachable at startup.** The pacer-row check is not reached until migrations
  succeed, so a DB outage still keeps `/healthz/live` answerable during retry (unchanged
  resilience). (REQ-PROV-064)

## Exclusions (What NOT to Build)

The following are explicitly **out of scope** for SPEC-PROV-002.

### Out of Scope — Provider trait redesign
- No change to the `Provider` trait's public surface (method set, signatures, or
  capability model). Trait default-body consolidation and redesign is **Phase 7** (F-50).
  This SPEC preserves the trait exactly; only the internal request path behind it changes.

### Out of Scope — Per-provider pacing in the chain fallback (F-16)
- The design flaw where the pacer slot is charged to the first capability-supporting
  provider rather than the provider that actually serves the request (F-16) is **Phase 7**
  (it rides the `chain_try` consolidation). This SPEC does NOT change which provider a slot
  is charged to; it only restores that search/tickers acquire a slot at all (F-10).

### Out of Scope — Provider data correctness & tier configuration (Phase 4)
- Interval-stamp canonicalisation (F-20), paid-tier header/base-URL routing (F-21), Binance
  `volume_24h` (F-22), and the rest of Category D are **Phase 4**. This SPEC deliberately
  lands first to provide the shared helpers + client constructor Phase 4 builds on.

### Out of Scope — New migration / new dependency / API or schema change
- No new migration, no new `upstream_request_pacer` column, no new table. The `Contended`
  variant is an in-code enum addition, not a schema change. No new crate dependency
  (`Cargo.toml` diff empty). No REST API contract or response-schema change.

### Out of Scope — Shutdown-aware pacer waits
- Making long `acquire_slot` sleeps observe the shutdown signal (so a worker blocked on a
  multi-minute pacer wait cancels promptly on SIGTERM) is a **Phase 6** lifecycle concern
  (F-41 drain timeout). The full-wait sleep here stays shutdown-agnostic exactly as today.

### Out of Scope — `f64` in monetary paths
- The Decimal-only monetary invariant (REQ-PROV-012) is untouched; no `f64` is introduced
  in any provider or pacer path.

## @MX Annotation Targets (high fan_in / invariant contracts)

- **`@MX:ANCHOR`** on the new `transport::paced` / `transport::get_json` helpers — the
  **shared request-path frame**: every provider endpoint routes through these; they are the
  single enforcement point for throttle + slot + 429 postlude + response epilogue (fan_in ≥
  3: CoinGecko, Binance, Bitstamp). (`@MX:REASON` required — drift-prevention invariant;
  F-10 existed because endpoints were written outside the frame.)
- **`@MX:ANCHOR`** on `transport::build_client` — the **timeout/UA invariant**: every
  provider client is constructed here; none may be built without a total-request timeout.
  (`@MX:REASON` required — availability invariant; a client without a timeout can hang a
  worker indefinitely.)
- **`@MX:WARN`** (update the existing tag on `acquire_slot`) — keep the "single fleet-wide
  egress governor" text accurate now that `paced()` is the standard enforcement point, and
  record that the sleep honours the full reserved wait (no 60 s truncation) with backlog
  surfaced via `warn!` + metric. (`@MX:REASON` required.)
- **`@MX:WARN`** (add on `signal_cooldown` — currently untagged; only `acquire_slot`
  carries a pacer MX tag today) — the **monotonic-cooldown invariant**: `GREATEST` ensures a
  cooldown is never shortened. (`@MX:REASON` required — a shortened cooldown reopens the 429
  window.)
- **`@MX:NOTE`** on the `main.rs` startup pacer-row check — ordering: runs after migrations
  succeed, before `set_ready`; a missing row fails readiness (REQ-PROV-063/064).

Full MX placement, tag text, and update/remove policy in plan.md § MX Tag Targets.

## Open Items

**0 unresolved.** The forks the brief flagged are settled as D1 (module placement =
`transport.rs`), D2 (shared constructor + env-tunable timeouts, defaults 30 s/10 s), D3
(F-13 = sleep-full + backlog observability, not truncate), and D4 (new pacer variant =
`Contended`). No `[NEEDS CLARIFICATION]` markers remain; residual micro-decisions (exact
`paced()` generic signature, precise metric label set) are plan.md/run-phase concerns, not
requirement ambiguities.
