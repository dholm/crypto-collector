-- SPEC-DB-001 migration 0001: Asset registries (REQ-DB-001/002/003/005).
--
-- tracked_coins: coin-keyed registry.
--   PK: coin_id TEXT (e.g. CoinGecko "bitcoin")
--   status domain: active | paused | error
--
-- tracked_markets: pair-keyed registry (base/quote/venue?).
--   PK: id BIGSERIAL (surrogate)
--   coin_id FK → tracked_coins (ON DELETE SET NULL) — links base asset to its coin record
--   Unique: (base, quote, COALESCE(venue, '')) so NULL-venue (aggregator) and named-venue rows
--           for the same pair coexist without collision (REQ-DB-003).
--
-- Live-poller contract columns consumed by SPEC-SCHED-001 REQ-SCHED-003:
--   last_polled_at TIMESTAMPTZ NULL          — when last successfully polled
--   live_poll_claimed_until TIMESTAMPTZ NULL — self-expiring in-flight claim marker
--   live_poll_interval INTERVAL NULL         — per-market cadence override; NULL = global default
--
-- @MX:ANCHOR: [AUTO] tracked_markets live-poller contract columns
-- @MX:REASON: SPEC-SCHED-001 poller claim query depends on last_polled_at, live_poll_claimed_until,
--             live_poll_interval, and the partial index (last_polled_at) WHERE status='active'.
--             Renaming, retyping, or dropping these breaks the poller (REQ-DB-002/005).

CREATE TABLE IF NOT EXISTS tracked_coins (
    coin_id           TEXT        NOT NULL,
    symbol            TEXT        NOT NULL,
    name              TEXT        NOT NULL,
    status            TEXT        NOT NULL DEFAULT 'active'
                                      CHECK (status IN ('active', 'paused', 'error')),
    registered_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_collected_at TIMESTAMPTZ,
    error             TEXT,
    PRIMARY KEY (coin_id)
);

CREATE TABLE IF NOT EXISTS tracked_markets (
    id                      BIGSERIAL   NOT NULL,
    base                    TEXT        NOT NULL,
    quote                   TEXT        NOT NULL,
    venue                   TEXT,
    coin_id                 TEXT
                                REFERENCES tracked_coins(coin_id) ON DELETE SET NULL,
    kind                    TEXT        NOT NULL
                                CHECK (kind IN ('spot', 'derivative')),
    status                  TEXT        NOT NULL DEFAULT 'active'
                                CHECK (status IN ('active', 'paused', 'error')),
    registered_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_collected_at       TIMESTAMPTZ,
    error                   TEXT,
    -- Live-poller contract columns (SPEC-SCHED-001 REQ-SCHED-003):
    last_polled_at          TIMESTAMPTZ,
    live_poll_claimed_until TIMESTAMPTZ,
    live_poll_interval      INTERVAL,
    PRIMARY KEY (id)
);

-- Uniqueness on (base, quote, COALESCE(venue, '')) — NULL venue (aggregator) and named venue
-- for the same pair are treated as distinct rows, preventing duplicate aggregator entries
-- while allowing venue-specific rows alongside. Plain UNIQUE(base,quote,venue) would not work
-- because NULL ≠ NULL in SQL (REQ-DB-003).
CREATE UNIQUE INDEX IF NOT EXISTS tracked_markets_pair_unique_idx
    ON tracked_markets (base, quote, COALESCE(venue, ''));

-- Partial claim index for the live-quote poller's due-and-not-in-flight claim query.
-- Supports: WHERE status = 'active' AND last_polled_at + live_poll_interval <= now()
--            AND (live_poll_claimed_until IS NULL OR live_poll_claimed_until < now())
-- Partial predicate on status = 'active' prunes all paused/error rows (REQ-DB-005).
CREATE INDEX IF NOT EXISTS tracked_markets_live_poll_claim_idx
    ON tracked_markets (last_polled_at)
    WHERE status = 'active';
