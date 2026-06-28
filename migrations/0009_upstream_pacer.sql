-- SPEC-DB-001 migration 0009: upstream_request_pacer per-provider rate table (REQ-DB-034/035).
--
-- Per-provider, credit-aware outbound rate pacer. One row per provider.
-- All three workers (live_poller, collection_queue, backfill) consume this table for
-- fleet-wide egress coordination — none may redefine it (research §4.4, REQ-DB-034).
--
-- An outbound call acquires a slot by atomically advancing:
--   next_allowed_at = GREATEST(now(), next_allowed_at) + (min_gap_ms * interval '1 ms')
-- and incrementing credits_used, honoring both the per-minute gap AND the monthly credit budget.
--
-- credit_limit: NULL means unlimited (e.g. Binance has no monthly credit cap).
-- cooldown_until: set when the provider returns 429; all replicas pause until this instant.
--
-- Seeded with one row per known provider (coingecko, binance, coinbase, kraken) so consumers
-- can atomically UPDATE … RETURNING without a prior INSERT (REQ-DB-035).
--
-- Default min_gap_ms:
--   coingecko: 2000 ms (30 calls/min for Demo tier; credit_limit = 10000/month)
--   binance:   100  ms (600 calls/min public API; no monthly cap)
--   coinbase:  500  ms (120 calls/min public API; no monthly cap)
--   kraken:    500  ms (120 calls/min public API; no monthly cap)
--
-- Adapted from ticker-collector yf_request_pacer (migration 0017), generalised to multi-provider.
--
-- @MX:NOTE: [AUTO] upstream_request_pacer — mandatory shared egress infrastructure (REQ-DB-034/035).
--   Consumed by: live_poller (SPEC-SCHED-001), collection_queue worker, backfill worker.
--   credit_limit = NULL means unlimited (Binance/Coinbase/Kraken have no monthly credit cap).
--   The per-provider design supersedes ticker-collector's single-row yf_request_pacer.

CREATE TABLE IF NOT EXISTS upstream_request_pacer (
    provider             TEXT        NOT NULL,
    next_allowed_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    min_gap_ms           INTEGER     NOT NULL DEFAULT 1000,
    cooldown_until       TIMESTAMPTZ,
    credit_window_start  TIMESTAMPTZ NOT NULL DEFAULT date_trunc('month', now()),
    credits_used         BIGINT      NOT NULL DEFAULT 0,
    -- NULL = unlimited (no monthly credit cap)
    credit_limit         BIGINT,
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (provider)
);

-- Seed one row per known provider so workers can UPDATE … RETURNING without INSERT (REQ-DB-035).
-- ON CONFLICT DO NOTHING makes this idempotent on re-apply (REQ-DB-043).
INSERT INTO upstream_request_pacer
    (provider, next_allowed_at, min_gap_ms, cooldown_until, credit_window_start, credits_used, credit_limit, updated_at)
VALUES
    ('coingecko', now(), 2000, NULL, date_trunc('month', now()), 0, 10000, now()),
    ('binance',   now(), 100,  NULL, date_trunc('month', now()), 0, NULL,  now()),
    ('coinbase',  now(), 500,  NULL, date_trunc('month', now()), 0, NULL,  now()),
    ('kraken',    now(), 500,  NULL, date_trunc('month', now()), 0, NULL,  now())
ON CONFLICT (provider) DO NOTHING;
