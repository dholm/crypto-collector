-- SPEC-DB-001 migration 0004: coin_market_snapshots partitioned time-series (REQ-DB-012/014/015).
--
-- Stores continuously-changing coin market aggregates: price, market_cap, FDV, supply, volume.
-- These change on every poll and are stored as time-series rows — NOT as coin_metadata revisions —
-- to avoid revision churn (REQ-DB-022, research §4.3).
--
-- PK: (coin_id, vs_currency, ts) — one snapshot per (coin, quote-currency, capture-time).
-- All quantity columns: NUMERIC (REQ-DB-040). No DOUBLE PRECISION.
-- Partitioning: RANGE(ts), one partition per calendar month (UTC boundaries).
--
-- Parent-level indexes inherited by all child partitions (REQ-DB-015):
--   btree(coin_id, vs_currency, ts DESC) — coin+currency scoped reads
--   BRIN(ts)                             — large append-ordered time-range scans
--
-- @MX:ANCHOR: [AUTO] coin_market_snapshots partition+index contract
-- @MX:REASON: btree(coin_id, vs_currency, ts DESC) + BRIN(ts) used by all market snapshot reads.
--             These aggregates must NOT be in coin_metadata (REQ-DB-022, research §4.3).

CREATE TABLE IF NOT EXISTS coin_market_snapshots (
    coin_id                 TEXT        NOT NULL
                                REFERENCES tracked_coins(coin_id) ON DELETE CASCADE,
    vs_currency             TEXT        NOT NULL,
    ts                      TIMESTAMPTZ NOT NULL,
    price                   NUMERIC     NOT NULL,
    market_cap              NUMERIC,
    fully_diluted_valuation NUMERIC,
    circulating_supply      NUMERIC,
    total_supply            NUMERIC,
    volume_24h              NUMERIC,
    source                  TEXT        NOT NULL,
    PRIMARY KEY (coin_id, vs_currency, ts)
) PARTITION BY RANGE (ts);

-- Parent-level btree: coin + currency scoped reads ordered newest-first (REQ-DB-015).
CREATE INDEX IF NOT EXISTS coin_market_snapshots_coin_id_vs_currency_ts_idx
    ON coin_market_snapshots (coin_id, vs_currency, ts DESC);

-- Parent-level BRIN: large append-ordered time-range scans (REQ-DB-015).
CREATE INDEX IF NOT EXISTS coin_market_snapshots_ts_brin
    ON coin_market_snapshots USING BRIN (ts);

-- ── Monthly partitions: 2024-01 through 2027-12 (UTC boundaries) ──────────────

CREATE TABLE IF NOT EXISTS coin_market_snapshots_2024_01 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2024-01-01') TO ('2024-02-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2024_02 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2024-02-01') TO ('2024-03-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2024_03 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2024-03-01') TO ('2024-04-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2024_04 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2024-04-01') TO ('2024-05-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2024_05 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2024-05-01') TO ('2024-06-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2024_06 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2024-06-01') TO ('2024-07-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2024_07 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2024-07-01') TO ('2024-08-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2024_08 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2024-08-01') TO ('2024-09-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2024_09 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2024-09-01') TO ('2024-10-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2024_10 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2024-10-01') TO ('2024-11-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2024_11 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2024-11-01') TO ('2024-12-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2024_12 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2024-12-01') TO ('2025-01-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2025_01 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2025-01-01') TO ('2025-02-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2025_02 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2025-02-01') TO ('2025-03-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2025_03 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2025-03-01') TO ('2025-04-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2025_04 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2025-04-01') TO ('2025-05-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2025_05 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2025-05-01') TO ('2025-06-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2025_06 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2025-06-01') TO ('2025-07-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2025_07 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2025-07-01') TO ('2025-08-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2025_08 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2025-08-01') TO ('2025-09-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2025_09 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2025-09-01') TO ('2025-10-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2025_10 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2025-10-01') TO ('2025-11-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2025_11 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2025-11-01') TO ('2025-12-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2025_12 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2025-12-01') TO ('2026-01-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2026_01 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2026-01-01') TO ('2026-02-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2026_02 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2026-02-01') TO ('2026-03-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2026_03 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2026-03-01') TO ('2026-04-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2026_04 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2026-04-01') TO ('2026-05-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2026_05 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2026-05-01') TO ('2026-06-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2026_06 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2026-06-01') TO ('2026-07-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2026_07 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2026-07-01') TO ('2026-08-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2026_08 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2026-08-01') TO ('2026-09-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2026_09 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2026-09-01') TO ('2026-10-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2026_10 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2026-10-01') TO ('2026-11-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2026_11 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2026-11-01') TO ('2026-12-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2026_12 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2026-12-01') TO ('2027-01-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2027_01 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2027-01-01') TO ('2027-02-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2027_02 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2027-02-01') TO ('2027-03-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2027_03 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2027-03-01') TO ('2027-04-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2027_04 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2027-04-01') TO ('2027-05-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2027_05 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2027-05-01') TO ('2027-06-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2027_06 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2027-06-01') TO ('2027-07-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2027_07 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2027-07-01') TO ('2027-08-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2027_08 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2027-08-01') TO ('2027-09-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2027_09 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2027-09-01') TO ('2027-10-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2027_10 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2027-10-01') TO ('2027-11-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2027_11 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2027-11-01') TO ('2027-12-01');
CREATE TABLE IF NOT EXISTS coin_market_snapshots_2027_12 PARTITION OF coin_market_snapshots FOR VALUES FROM ('2027-12-01') TO ('2028-01-01');
