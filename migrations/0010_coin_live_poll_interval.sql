-- SPEC-API-002 migration 0010: per-coin live-poll scheduling columns (REQ-API-112/113/114).
--
-- Adds the three columns that the live-poller (SPEC-SCHED-001) reads when claiming coins:
--   live_poll_interval      INTERVAL    -- per-coin cadence override; NULL = global default
--   last_polled_at          TIMESTAMPTZ -- cursor advanced ONLY on success (REQ-SCHED-005)
--   live_poll_claimed_until TIMESTAMPTZ -- self-expiring in-flight marker (REQ-SCHED-007)
--
-- Partial claim index mirrors the one on tracked_markets (0001_registries.sql).
-- Index predicate WHERE status = 'active' matches the poller claim SQL predicate.
--
-- @MX:ANCHOR: [AUTO] tracked_coins live-poller contract — last_polled_at, live_poll_claimed_until, live_poll_interval
-- @MX:REASON: SPEC-SCHED-001 REQ-SCHED-003 poller claim query reads these columns and the partial
--             index WHERE status='active'. Any rename/retype breaks the coin poller.

ALTER TABLE tracked_coins
    ADD COLUMN IF NOT EXISTS live_poll_interval      INTERVAL,
    ADD COLUMN IF NOT EXISTS last_polled_at          TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS live_poll_claimed_until TIMESTAMPTZ;

-- Partial claim index: efficient due-coin selection by live poller.
-- Supports: SELECT ... WHERE status='active' AND (last_polled_at IS NULL OR ...) FOR UPDATE SKIP LOCKED
CREATE INDEX IF NOT EXISTS tracked_coins_live_poll_claim_idx
    ON tracked_coins (last_polled_at)
    WHERE status = 'active';
