-- SPEC-PROV-001 v1.2.0: seed the pacer row for the Bitstamp provider.
--
-- Every provider that calls acquire_slot() needs a row in upstream_request_pacer —
-- without one, acquire_slot returns NotFound and the provider's fetch errors out.
-- Bitstamp was added as a chain member (deep-history OHLC) but had no pacer row, so
-- its range fetches failed and the deep-history backfill silently found no data.
--
-- 500 ms min gap (Bitstamp's public API is generous but we stay conservative — the
-- deep backfill is a one-time bulk walk, not latency-sensitive), no credit_limit
-- (keyless public endpoint). ON CONFLICT DO NOTHING keeps it idempotent on re-apply.
INSERT INTO upstream_request_pacer
    (provider, next_allowed_at, min_gap_ms, cooldown_until, credit_window_start, credits_used, credit_limit, updated_at)
VALUES
    ('bitstamp', now(), 500, NULL, date_trunc('month', now()), 0, NULL, now())
ON CONFLICT (provider) DO NOTHING;
