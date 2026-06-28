# Acceptance Criteria — SPEC-PROV-001 (Provider Chain, CoinGecko Client & Pacing)

Each scenario maps to EARS requirements in `spec.md`. Pure scenarios run offline over
JSON fixtures; pacer/HTTP scenarios are gated (`#[ignore]`) on a live DB / network.

## Scenario 1 — Unknown provider name fails fast (REQ-PROV-002)

- **Given** `PROVIDERS = "coingecko,notreal"`
- **When** `build_chain` runs at startup
- **Then** it returns an error naming `notreal` and listing the valid names
  (`coingecko, binance, coinbase, kraken`); the process does not start.

## Scenario 2 — Declared order is fallback priority (REQ-PROV-003)

- **Given** `PROVIDERS = "coingecko,binance"`
- **When** the chain is built
- **Then** `chain[0].name() == "coingecko"` and `chain[1].name() == "binance"`, and
  capability calls attempt CoinGecko before Binance.

## Scenario 3 — Capability fallback advances on failure/unsupported (REQ-PROV-004/006)

- **Given** a chain `[stub_primary, stub_secondary]` where the primary errors on
  `fetch_ohlc` and the secondary succeeds
- **When** the orchestrator requests OHLC
- **Then** it returns the secondary's candles, and records
  `collection_requests_total` for the primary (failure) and the secondary (success).

## Scenario 4 — All providers fail → read-only degradation (REQ-PROV-005)

- **Given** every provider in the chain errors for a read-serving datum
- **When** an API read for that datum is served
- **Then** the system returns the last-persisted value (not a 5xx for missing
  upstream) and records the upstream failure.

## Scenario 5 — CoinGecko tier switches base URL and key header (REQ-PROV-011)

- **Given** `COINGECKO_TIER = demo`
- **When** the client builds a request
- **Then** it targets `api.coingecko.com` and sets `x-cg-demo-api-key`; and **given**
  `COINGECKO_TIER = pro`, it targets `pro-api.coingecko.com` and sets
  `x-cg-pro-api-key`.

## Scenario 6 — Numeric fields parse to Decimal, never f64 (REQ-PROV-012)

- **Given** a CoinGecko fixture with a tiny price (`0.00000000001234`) and a huge
  supply (`589000000000000`)
- **When** the response is normalised
- **Then** the model fields are `rust_decimal::Decimal` equal to the exact input
  values (no precision loss); a code-level check confirms no monetary field is typed
  `f64`.

## Scenario 7 — CoinGecko OHLC has no volume → volume None, source tagged (REQ-PROV-013/031)

- **Given** a `/coins/{id}/ohlc` fixture (4-tuples, no volume)
- **When** it is normalised to candles
- **Then** each candle has `open/high/low/close` as `Decimal`, `volume == None`, and
  `source == "coingecko"` — `None` is not coerced to `0`.

## Scenario 8 — Tier-limited endpoint degrades, not errors (REQ-PROV-014)

- **Given** `COINGECKO_TIER = demo` and a backfill request needing `/ohlc/range`
  (Analyst+)
- **When** the client cannot use the range endpoint
- **Then** it falls back to the day-bucketed OHLC and surfaces the granularity
  limitation to the caller (does not fabricate finer candles or hard-error).

## Scenario 9 — Exchange provider supplies volume-bearing candles with venue (REQ-PROV-020/021/022)

- **Given** a Binance kline fixture and `tracked_markets` row `(BTC, USDT, binance)`
- **When** the Binance provider normalises the klines
- **Then** each candle has a non-NULL `Decimal` volume, `source == "binance"`, and is
  associated with the `binance` venue (distinct from a NULL-venue aggregator row).

## Scenario 10 — Pacer enforces min-gap and increments credits (REQ-PROV-040)

- **Given** `upstream_request_pacer` row for `coingecko` with `min_gap_ms = 2000`
- **When** two `acquire_slot("coingecko")` calls run back-to-back
- **Then** the second's returned `next_allowed_at` is ≥ 2000ms after the first's, and
  `credits_used` increments by exactly one per acquisition.

## Scenario 11 — 429 sets a fleet-wide cooldown (REQ-PROV-041/042)

- **Given** an outbound CoinGecko call returns HTTP 429
- **When** `signal_cooldown("coingecko")` runs
- **Then** `cooldown_until` is set to `now() + cooldown`, and subsequent
  `acquire_slot("coingecko")` calls (on any replica) withhold until it expires.

## Scenario 12 — Monthly credit budget withholds and resets (REQ-PROV-043/044)

- **Given** `credit_limit = 10000` and `credits_used = 10000` within the current
  window
- **When** `acquire_slot("coingecko")` is attempted
- **Then** it withholds (no slot granted) until the window elapses; once
  `now() - credit_window_start >= 1 month`, `credits_used` resets to 0,
  `credit_window_start` advances, and acquisition resumes.

## Scenario 13 — No outbound call bypasses the pacer (REQ-PROV-045)

- **Given** the provider client code
- **When** inspected (structural test) and exercised
- **Then** every outbound HTTP request is preceded by an `acquire_slot` for that
  provider; no client path issues a request without acquiring a slot, and no second
  pacing table exists.

## Scenario 14 — Timestamps normalised to UTC (REQ-PROV-032)

- **Given** provider responses with epoch-seconds and epoch-millis timestamps
- **When** normalised
- **Then** all produced timestamps are `DateTime<Utc>` representing the correct
  instant.

## Quality Gate / Definition of Done

- [ ] `build_chain` fail-fast + ordered; capability fallback + read-only degradation
      (1–4).
- [ ] CoinGecko tier base-URL/key switching (5).
- [ ] All numeric fields `Decimal`, exact for tiny/huge magnitudes; no `f64` monetary
      fields (6).
- [ ] CoinGecko OHLC volume `None` + source tag; tier-limited endpoints degrade (7, 8).
- [ ] Exchange provider supplies volume + venue via the same trait (9).
- [ ] Pacer: min-gap + credit increment, fleet-wide cooldown on 429, monthly-credit
      withhold + reset, no bypass (10–13).
- [ ] Timestamps UTC (14).
- [ ] `cargo sqlx prepare` verified against live Postgres for pacer queries.
- [ ] Open items OR-PROV-1..4 resolved or explicitly deferred with user sign-off.
