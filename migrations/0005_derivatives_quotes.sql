-- SPEC-DB-001 migration 0005: derivatives_quotes partitioned time-series (REQ-DB-013/014/015).
--
-- Consolidates all per-tick derivative observables in ONE row per (market_id, ts):
--   funding_rate, open_interest, open_interest_usd, mark_price, index_price, basis.
-- Matches CoinGecko /derivatives/tickers which returns all these fields together (research §1.4).
-- There is NO separate funding_rates or open_interest table (REQ-DB-013, research §1.4).
--
-- PK: (market_id, ts) — one tick per derivative market per capture instant.
-- All quantity columns: NUMERIC (REQ-DB-040). No DOUBLE PRECISION.
-- Partitioning: RANGE(ts), one partition per calendar month (UTC boundaries).
--
-- Parent-level indexes inherited by all child partitions (REQ-DB-015):
--   btree(market_id, ts DESC) — market-scoped reads ordered newest-first
--   BRIN(ts)                  — large append-ordered time-range scans
--
-- @MX:ANCHOR: [AUTO] derivatives_quotes partition+index contract — btree(market_id, ts DESC) + BRIN(ts)
-- @MX:REASON: All derivatives read paths depend on this index shape. The single-table design
--             (no separate funding/OI tables) is invariant per REQ-DB-013.

CREATE TABLE IF NOT EXISTS derivatives_quotes (
    market_id        BIGINT      NOT NULL
                         REFERENCES tracked_markets(id) ON DELETE CASCADE,
    ts               TIMESTAMPTZ NOT NULL,
    funding_rate     NUMERIC,
    open_interest    NUMERIC,
    open_interest_usd NUMERIC,
    mark_price       NUMERIC,
    index_price      NUMERIC,
    basis            NUMERIC,
    volume_24h       NUMERIC,
    contract_type    TEXT,
    venue            TEXT,
    source           TEXT        NOT NULL,
    PRIMARY KEY (market_id, ts)
) PARTITION BY RANGE (ts);

-- Parent-level btree: market-scoped reads ordered newest-first (REQ-DB-015).
CREATE INDEX IF NOT EXISTS derivatives_quotes_market_id_ts_idx
    ON derivatives_quotes (market_id, ts DESC);

-- Parent-level BRIN: large append-ordered time-range scans (REQ-DB-015).
CREATE INDEX IF NOT EXISTS derivatives_quotes_ts_brin
    ON derivatives_quotes USING BRIN (ts);

-- ── Monthly partitions: 2024-01 through 2027-12 (UTC boundaries) ──────────────

CREATE TABLE IF NOT EXISTS derivatives_quotes_2024_01 PARTITION OF derivatives_quotes FOR VALUES FROM ('2024-01-01') TO ('2024-02-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2024_02 PARTITION OF derivatives_quotes FOR VALUES FROM ('2024-02-01') TO ('2024-03-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2024_03 PARTITION OF derivatives_quotes FOR VALUES FROM ('2024-03-01') TO ('2024-04-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2024_04 PARTITION OF derivatives_quotes FOR VALUES FROM ('2024-04-01') TO ('2024-05-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2024_05 PARTITION OF derivatives_quotes FOR VALUES FROM ('2024-05-01') TO ('2024-06-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2024_06 PARTITION OF derivatives_quotes FOR VALUES FROM ('2024-06-01') TO ('2024-07-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2024_07 PARTITION OF derivatives_quotes FOR VALUES FROM ('2024-07-01') TO ('2024-08-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2024_08 PARTITION OF derivatives_quotes FOR VALUES FROM ('2024-08-01') TO ('2024-09-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2024_09 PARTITION OF derivatives_quotes FOR VALUES FROM ('2024-09-01') TO ('2024-10-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2024_10 PARTITION OF derivatives_quotes FOR VALUES FROM ('2024-10-01') TO ('2024-11-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2024_11 PARTITION OF derivatives_quotes FOR VALUES FROM ('2024-11-01') TO ('2024-12-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2024_12 PARTITION OF derivatives_quotes FOR VALUES FROM ('2024-12-01') TO ('2025-01-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2025_01 PARTITION OF derivatives_quotes FOR VALUES FROM ('2025-01-01') TO ('2025-02-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2025_02 PARTITION OF derivatives_quotes FOR VALUES FROM ('2025-02-01') TO ('2025-03-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2025_03 PARTITION OF derivatives_quotes FOR VALUES FROM ('2025-03-01') TO ('2025-04-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2025_04 PARTITION OF derivatives_quotes FOR VALUES FROM ('2025-04-01') TO ('2025-05-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2025_05 PARTITION OF derivatives_quotes FOR VALUES FROM ('2025-05-01') TO ('2025-06-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2025_06 PARTITION OF derivatives_quotes FOR VALUES FROM ('2025-06-01') TO ('2025-07-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2025_07 PARTITION OF derivatives_quotes FOR VALUES FROM ('2025-07-01') TO ('2025-08-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2025_08 PARTITION OF derivatives_quotes FOR VALUES FROM ('2025-08-01') TO ('2025-09-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2025_09 PARTITION OF derivatives_quotes FOR VALUES FROM ('2025-09-01') TO ('2025-10-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2025_10 PARTITION OF derivatives_quotes FOR VALUES FROM ('2025-10-01') TO ('2025-11-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2025_11 PARTITION OF derivatives_quotes FOR VALUES FROM ('2025-11-01') TO ('2025-12-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2025_12 PARTITION OF derivatives_quotes FOR VALUES FROM ('2025-12-01') TO ('2026-01-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2026_01 PARTITION OF derivatives_quotes FOR VALUES FROM ('2026-01-01') TO ('2026-02-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2026_02 PARTITION OF derivatives_quotes FOR VALUES FROM ('2026-02-01') TO ('2026-03-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2026_03 PARTITION OF derivatives_quotes FOR VALUES FROM ('2026-03-01') TO ('2026-04-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2026_04 PARTITION OF derivatives_quotes FOR VALUES FROM ('2026-04-01') TO ('2026-05-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2026_05 PARTITION OF derivatives_quotes FOR VALUES FROM ('2026-05-01') TO ('2026-06-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2026_06 PARTITION OF derivatives_quotes FOR VALUES FROM ('2026-06-01') TO ('2026-07-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2026_07 PARTITION OF derivatives_quotes FOR VALUES FROM ('2026-07-01') TO ('2026-08-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2026_08 PARTITION OF derivatives_quotes FOR VALUES FROM ('2026-08-01') TO ('2026-09-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2026_09 PARTITION OF derivatives_quotes FOR VALUES FROM ('2026-09-01') TO ('2026-10-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2026_10 PARTITION OF derivatives_quotes FOR VALUES FROM ('2026-10-01') TO ('2026-11-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2026_11 PARTITION OF derivatives_quotes FOR VALUES FROM ('2026-11-01') TO ('2026-12-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2026_12 PARTITION OF derivatives_quotes FOR VALUES FROM ('2026-12-01') TO ('2027-01-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2027_01 PARTITION OF derivatives_quotes FOR VALUES FROM ('2027-01-01') TO ('2027-02-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2027_02 PARTITION OF derivatives_quotes FOR VALUES FROM ('2027-02-01') TO ('2027-03-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2027_03 PARTITION OF derivatives_quotes FOR VALUES FROM ('2027-03-01') TO ('2027-04-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2027_04 PARTITION OF derivatives_quotes FOR VALUES FROM ('2027-04-01') TO ('2027-05-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2027_05 PARTITION OF derivatives_quotes FOR VALUES FROM ('2027-05-01') TO ('2027-06-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2027_06 PARTITION OF derivatives_quotes FOR VALUES FROM ('2027-06-01') TO ('2027-07-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2027_07 PARTITION OF derivatives_quotes FOR VALUES FROM ('2027-07-01') TO ('2027-08-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2027_08 PARTITION OF derivatives_quotes FOR VALUES FROM ('2027-08-01') TO ('2027-09-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2027_09 PARTITION OF derivatives_quotes FOR VALUES FROM ('2027-09-01') TO ('2027-10-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2027_10 PARTITION OF derivatives_quotes FOR VALUES FROM ('2027-10-01') TO ('2027-11-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2027_11 PARTITION OF derivatives_quotes FOR VALUES FROM ('2027-11-01') TO ('2027-12-01');
CREATE TABLE IF NOT EXISTS derivatives_quotes_2027_12 PARTITION OF derivatives_quotes FOR VALUES FROM ('2027-12-01') TO ('2028-01-01');
