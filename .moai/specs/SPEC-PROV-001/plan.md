# Implementation Plan — SPEC-PROV-001 (Provider Chain, CoinGecko Client & Pacing)

Research: [research.md](./research.md) (§2 providers, §3 crate evaluation, §4.4 pacer).
Schema contract: [../SPEC-DB-001/spec.md](../SPEC-DB-001/spec.md)
(`upstream_request_pacer`, data tables). Consumer: SPEC-SCHED-001.
Methodology: greenfield TDD — pure normaliser/parse tests + gated live-HTTP/live-DB
integration tests.

## Technical Approach

Define the `Provider` trait and `build_chain` (mirroring `ticker-collector`
`providers/mod.rs`). Implement the CoinGecko client first (primary), then the pacer
protocol over `upstream_request_pacer`, then optional exchange clients behind the same
trait. Numeric deserialization targets `rust_decimal::Decimal` via `serde`.

The pacer is the spine: a single `acquire_slot(provider)` helper (atomic
`UPDATE … RETURNING`) and a `signal_cooldown(provider)` helper, called by every
outbound path. This generalises ticker's `yf_request_pacer` consumer pattern to a
keyed, credit-aware table.

## Files (anticipated; confirmed in run phase)

| File | Change |
|---|---|
| `src/providers/mod.rs` (new) | `Provider` trait, capability enum, `build_chain` (fail-fast, ordered), chain orchestration with per-capability fallback. |
| `src/providers/coingecko.rs` (new) | CoinGecko `reqwest` client; tier-based base-URL/key; endpoint→capability mapping; `Decimal` deserialization; OHLC→candle (`volume=None`). |
| `src/providers/binance.rs` / `coinbase.rs` / `kraken.rs` (new, optional) | Exchange clients behind `Provider`; per-candle-volume klines; venue mapping. |
| `src/pacer/mod.rs` (new) | `acquire_slot(provider)` + `signal_cooldown(provider)` over `upstream_request_pacer`; monthly credit window reset; replica-local throttle. |
| `src/models/*.rs` (shared with SPEC-DB-001) | Internal normalised models (`Decimal` fields). |
| `src/config.rs` (shared with SPEC-DEPLOY-001) | `PROVIDERS`, `COINGECKO_*`, `PACER_*` keys. |

## Milestones (priority-ordered, no time estimates)

### Milestone 1 — Provider trait + build_chain (Priority High)
- RED: `build_chain` rejects an unknown name (REQ-PROV-002), preserves order
  (REQ-PROV-003), empty list → empty chain (mirrors ticker tests).
- GREEN: trait + `build_chain` + capability-support predicate.

### Milestone 2 — pacer protocol (Priority High)
- RED (live-DB gated): `acquire_slot` advances `next_allowed_at` by `min_gap_ms` and
  increments `credits_used`; honours `cooldown_until`; withholds when `credits_used >=
  credit_limit`; resets the window after the monthly interval (REQ-PROV-040..044). Two
  concurrent acquirers serialise (atomic `UPDATE`).
- GREEN: `acquire_slot` / `signal_cooldown` + window reset + local throttle.

### Milestone 3 — CoinGecko client (Priority High)
- RED: pure normaliser tests over captured CoinGecko JSON fixtures →
  spot/candle/metadata/market/derivative models with `Decimal` fields; OHLC fixture →
  `volume = None`, `source = "coingecko"` (REQ-PROV-010..013). Tier switch selects the
  right base URL + key header (REQ-PROV-011).
- GREEN: client + deserializers; wire `acquire_slot("coingecko")` before each call.
- Gated live-HTTP smoke test against the Demo API (no key) for a well-known coin.

### Milestone 4 — chain fallback + degradation (Priority High)
- RED: with a stub primary that errors and a stub secondary that succeeds, the chain
  returns the secondary's data and records both outcomes (REQ-PROV-004/006); with all
  stubs failing, the read path falls back to last-persisted data (REQ-PROV-005).
- GREEN: orchestration + outcome metric hook.

### Milestone 5 — exchange clients (Priority Medium, optional)
- RED: Binance kline fixture → candle with non-NULL `Decimal` volume and the venue set
  (REQ-PROV-020..022).
- GREEN: Binance client (then Coinbase/Kraken as needed); resolve OR-PROV-1.

### Milestone 6 — quota-compliance integration (Priority Medium)
- Gated test: simulate HTTP 429 → `signal_cooldown` sets `cooldown_until`; subsequent
  `acquire_slot` withholds until expiry (REQ-PROV-041/042); credit-limit exhaustion
  withholds until window reset (REQ-PROV-043/044).

## Risks

- **Quota exhaustion (highest).** Demo monthly credit (10k) is easily burned; the
  credit-aware pacer + conservative cadence defaults (SPEC-DEPLOY-001) mitigate. A
  bypass of `acquire_slot` risks an upstream ban — the pacer must wrap *every* call
  (REQ-PROV-045); a structural test asserts no client issues a request without first
  acquiring a slot.
- **Precision regressions.** A stray `f64` deserialization silently truncates; the
  normaliser tests assert `Decimal` round-trips for tiny and huge magnitudes (research
  §1.5).
- **CoinGecko coverage gaps by tier.** `/ohlc/range` and `supply_breakdown` are
  Analyst+; the client must degrade (REQ-PROV-014), not error, on the Demo tier.
- **OHLC volume gap.** CoinGecko OHLC has no volume; downstream must not treat `None`
  as `0` (REQ-PROV-013/031) — enforced by the candle model's nullable volume.
- **Exchange rate-limit heterogeneity.** Binance weight vs Kraken counter map onto the
  same pacer row; per-provider `min_gap_ms` defaults must reflect each model.

## Definition of Done

- `Provider` trait + `build_chain` (fail-fast, ordered) implemented and tested.
- CoinGecko client covers all three domains with `Decimal` types and tier switching.
- Per-provider credit-aware pacer wraps every outbound call; cooldown + credit-window
  enforced; no bypass path.
- Chain fallback + read-only degradation verified.
- Optional exchange client(s) behind the same trait (or OR-PROV-1 deferred).
- All EARS REQ-PROV-001..045 covered by tests (pure + gated integration).
- Open items OR-PROV-1..4 resolved or explicitly deferred with user sign-off.
