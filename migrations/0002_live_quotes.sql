-- SPEC-DB-001 migration 0002: live_quotes partitioned time-series (REQ-DB-010/014/015/016).
--
-- PK: (market_id, ts) — one snapshot per market per capture instant.
-- as_of: provider quote instant (may differ from capture ts).
-- All price/size/volume columns: NUMERIC (REQ-DB-040). No DOUBLE PRECISION.
-- Partitioning: RANGE(ts), one partition per calendar month (UTC boundaries).
--
-- Parent-level indexes inherited by all child partitions (REQ-DB-015):
--   btree(market_id, ts DESC) — market-scoped reads and keyset pagination
--   BRIN(ts)                  — large append-ordered time-range scans
--
-- Initial partitions: 2024-01 through 2027-12 (REQ-DB-016: current year 2026 + next year 2027;
-- 2024-2025 added for historical backfill coverage). Future months per OR-DB-3.
--
-- @MX:ANCHOR: [AUTO] live_quotes partition+index contract — btree(market_id, ts DESC) + BRIN(ts)
-- @MX:REASON: All live-quote read paths depend on this exact index shape for market-scoped
--             keyset pagination. The partition key (ts, monthly RANGE) is invariant.
--             Changing either requires migrating all downstream read queries (REQ-DB-014/015).

CREATE TABLE IF NOT EXISTS live_quotes (
    market_id   BIGINT      NOT NULL
                    REFERENCES tracked_markets(id) ON DELETE CASCADE,
    ts          TIMESTAMPTZ NOT NULL,
    as_of       TIMESTAMPTZ,
    price       NUMERIC     NOT NULL,
    bid         NUMERIC,
    ask         NUMERIC,
    bid_size    NUMERIC,
    ask_size    NUMERIC,
    volume_24h  NUMERIC,
    vs_currency TEXT        NOT NULL,
    source      TEXT        NOT NULL,
    PRIMARY KEY (market_id, ts)
) PARTITION BY RANGE (ts);

-- Parent-level btree index: market-scoped reads ordered newest-first (REQ-DB-015).
CREATE INDEX IF NOT EXISTS live_quotes_market_id_ts_idx
    ON live_quotes (market_id, ts DESC);

-- Parent-level BRIN index: large append-ordered time-range scans (REQ-DB-015).
CREATE INDEX IF NOT EXISTS live_quotes_ts_brin
    ON live_quotes USING BRIN (ts);

-- ── Monthly partitions: 2024-01 through 2027-12 (UTC boundaries) ──────────────

CREATE TABLE IF NOT EXISTS live_quotes_2024_01 PARTITION OF live_quotes FOR VALUES FROM ('2024-01-01') TO ('2024-02-01');
CREATE TABLE IF NOT EXISTS live_quotes_2024_02 PARTITION OF live_quotes FOR VALUES FROM ('2024-02-01') TO ('2024-03-01');
CREATE TABLE IF NOT EXISTS live_quotes_2024_03 PARTITION OF live_quotes FOR VALUES FROM ('2024-03-01') TO ('2024-04-01');
CREATE TABLE IF NOT EXISTS live_quotes_2024_04 PARTITION OF live_quotes FOR VALUES FROM ('2024-04-01') TO ('2024-05-01');
CREATE TABLE IF NOT EXISTS live_quotes_2024_05 PARTITION OF live_quotes FOR VALUES FROM ('2024-05-01') TO ('2024-06-01');
CREATE TABLE IF NOT EXISTS live_quotes_2024_06 PARTITION OF live_quotes FOR VALUES FROM ('2024-06-01') TO ('2024-07-01');
CREATE TABLE IF NOT EXISTS live_quotes_2024_07 PARTITION OF live_quotes FOR VALUES FROM ('2024-07-01') TO ('2024-08-01');
CREATE TABLE IF NOT EXISTS live_quotes_2024_08 PARTITION OF live_quotes FOR VALUES FROM ('2024-08-01') TO ('2024-09-01');
CREATE TABLE IF NOT EXISTS live_quotes_2024_09 PARTITION OF live_quotes FOR VALUES FROM ('2024-09-01') TO ('2024-10-01');
CREATE TABLE IF NOT EXISTS live_quotes_2024_10 PARTITION OF live_quotes FOR VALUES FROM ('2024-10-01') TO ('2024-11-01');
CREATE TABLE IF NOT EXISTS live_quotes_2024_11 PARTITION OF live_quotes FOR VALUES FROM ('2024-11-01') TO ('2024-12-01');
CREATE TABLE IF NOT EXISTS live_quotes_2024_12 PARTITION OF live_quotes FOR VALUES FROM ('2024-12-01') TO ('2025-01-01');
CREATE TABLE IF NOT EXISTS live_quotes_2025_01 PARTITION OF live_quotes FOR VALUES FROM ('2025-01-01') TO ('2025-02-01');
CREATE TABLE IF NOT EXISTS live_quotes_2025_02 PARTITION OF live_quotes FOR VALUES FROM ('2025-02-01') TO ('2025-03-01');
CREATE TABLE IF NOT EXISTS live_quotes_2025_03 PARTITION OF live_quotes FOR VALUES FROM ('2025-03-01') TO ('2025-04-01');
CREATE TABLE IF NOT EXISTS live_quotes_2025_04 PARTITION OF live_quotes FOR VALUES FROM ('2025-04-01') TO ('2025-05-01');
CREATE TABLE IF NOT EXISTS live_quotes_2025_05 PARTITION OF live_quotes FOR VALUES FROM ('2025-05-01') TO ('2025-06-01');
CREATE TABLE IF NOT EXISTS live_quotes_2025_06 PARTITION OF live_quotes FOR VALUES FROM ('2025-06-01') TO ('2025-07-01');
CREATE TABLE IF NOT EXISTS live_quotes_2025_07 PARTITION OF live_quotes FOR VALUES FROM ('2025-07-01') TO ('2025-08-01');
CREATE TABLE IF NOT EXISTS live_quotes_2025_08 PARTITION OF live_quotes FOR VALUES FROM ('2025-08-01') TO ('2025-09-01');
CREATE TABLE IF NOT EXISTS live_quotes_2025_09 PARTITION OF live_quotes FOR VALUES FROM ('2025-09-01') TO ('2025-10-01');
CREATE TABLE IF NOT EXISTS live_quotes_2025_10 PARTITION OF live_quotes FOR VALUES FROM ('2025-10-01') TO ('2025-11-01');
CREATE TABLE IF NOT EXISTS live_quotes_2025_11 PARTITION OF live_quotes FOR VALUES FROM ('2025-11-01') TO ('2025-12-01');
CREATE TABLE IF NOT EXISTS live_quotes_2025_12 PARTITION OF live_quotes FOR VALUES FROM ('2025-12-01') TO ('2026-01-01');
CREATE TABLE IF NOT EXISTS live_quotes_2026_01 PARTITION OF live_quotes FOR VALUES FROM ('2026-01-01') TO ('2026-02-01');
CREATE TABLE IF NOT EXISTS live_quotes_2026_02 PARTITION OF live_quotes FOR VALUES FROM ('2026-02-01') TO ('2026-03-01');
CREATE TABLE IF NOT EXISTS live_quotes_2026_03 PARTITION OF live_quotes FOR VALUES FROM ('2026-03-01') TO ('2026-04-01');
CREATE TABLE IF NOT EXISTS live_quotes_2026_04 PARTITION OF live_quotes FOR VALUES FROM ('2026-04-01') TO ('2026-05-01');
CREATE TABLE IF NOT EXISTS live_quotes_2026_05 PARTITION OF live_quotes FOR VALUES FROM ('2026-05-01') TO ('2026-06-01');
CREATE TABLE IF NOT EXISTS live_quotes_2026_06 PARTITION OF live_quotes FOR VALUES FROM ('2026-06-01') TO ('2026-07-01');
CREATE TABLE IF NOT EXISTS live_quotes_2026_07 PARTITION OF live_quotes FOR VALUES FROM ('2026-07-01') TO ('2026-08-01');
CREATE TABLE IF NOT EXISTS live_quotes_2026_08 PARTITION OF live_quotes FOR VALUES FROM ('2026-08-01') TO ('2026-09-01');
CREATE TABLE IF NOT EXISTS live_quotes_2026_09 PARTITION OF live_quotes FOR VALUES FROM ('2026-09-01') TO ('2026-10-01');
CREATE TABLE IF NOT EXISTS live_quotes_2026_10 PARTITION OF live_quotes FOR VALUES FROM ('2026-10-01') TO ('2026-11-01');
CREATE TABLE IF NOT EXISTS live_quotes_2026_11 PARTITION OF live_quotes FOR VALUES FROM ('2026-11-01') TO ('2026-12-01');
CREATE TABLE IF NOT EXISTS live_quotes_2026_12 PARTITION OF live_quotes FOR VALUES FROM ('2026-12-01') TO ('2027-01-01');
CREATE TABLE IF NOT EXISTS live_quotes_2027_01 PARTITION OF live_quotes FOR VALUES FROM ('2027-01-01') TO ('2027-02-01');
CREATE TABLE IF NOT EXISTS live_quotes_2027_02 PARTITION OF live_quotes FOR VALUES FROM ('2027-02-01') TO ('2027-03-01');
CREATE TABLE IF NOT EXISTS live_quotes_2027_03 PARTITION OF live_quotes FOR VALUES FROM ('2027-03-01') TO ('2027-04-01');
CREATE TABLE IF NOT EXISTS live_quotes_2027_04 PARTITION OF live_quotes FOR VALUES FROM ('2027-04-01') TO ('2027-05-01');
CREATE TABLE IF NOT EXISTS live_quotes_2027_05 PARTITION OF live_quotes FOR VALUES FROM ('2027-05-01') TO ('2027-06-01');
CREATE TABLE IF NOT EXISTS live_quotes_2027_06 PARTITION OF live_quotes FOR VALUES FROM ('2027-06-01') TO ('2027-07-01');
CREATE TABLE IF NOT EXISTS live_quotes_2027_07 PARTITION OF live_quotes FOR VALUES FROM ('2027-07-01') TO ('2027-08-01');
CREATE TABLE IF NOT EXISTS live_quotes_2027_08 PARTITION OF live_quotes FOR VALUES FROM ('2027-08-01') TO ('2027-09-01');
CREATE TABLE IF NOT EXISTS live_quotes_2027_09 PARTITION OF live_quotes FOR VALUES FROM ('2027-09-01') TO ('2027-10-01');
CREATE TABLE IF NOT EXISTS live_quotes_2027_10 PARTITION OF live_quotes FOR VALUES FROM ('2027-10-01') TO ('2027-11-01');
CREATE TABLE IF NOT EXISTS live_quotes_2027_11 PARTITION OF live_quotes FOR VALUES FROM ('2027-11-01') TO ('2027-12-01');
CREATE TABLE IF NOT EXISTS live_quotes_2027_12 PARTITION OF live_quotes FOR VALUES FROM ('2027-12-01') TO ('2028-01-01');
