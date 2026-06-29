-- SPEC-API-002 migration 0011: remove market-keyed infrastructure, add coin-keyed time-series.
--
-- Drops:
--   derivatives_quotes   (market_id FK to tracked_markets ON DELETE CASCADE)
--   candles              (market_id FK, partitioned — parent drop removes all partitions)
--   live_quotes          (market_id FK, partitioned — parent drop removes all partitions)
--   backfill_jobs        (market_id FK to tracked_markets)
--   tracked_markets      (now orphaned; CASCADE drops any remaining FK constraints)
--
-- Creates:
--   coin_quotes    — coin-keyed spot price time-series (PK: coin_id, vs_currency, ts)
--   coin_candles   — coin-keyed OHLCV time-series (PK: coin_id, vs_currency, interval, ts)
--
-- Both new tables:
--   PARTITION BY RANGE(ts), one partition per calendar month (UTC boundaries).
--   btree index for coin-scoped keyset pagination (REQ-DB-015).
--   BRIN index for large append-ordered scans (REQ-DB-015).
--   Partitions: 2024-01 through 2027-12 (REQ-DB-016).
--
-- @MX:ANCHOR: [AUTO] coin_quotes / coin_candles partition+index contract
-- @MX:REASON: All coin-keyed read paths depend on btree+BRIN shape and monthly RANGE partitions.
--             Changing the partition key or index shape breaks keyset pagination (REQ-DB-015).

-- ── Drop market-keyed infrastructure ─────────────────────────────────────────────

-- Drop in dependency order so no FK constraint blocks the drops.
DROP TABLE IF EXISTS derivatives_quotes;
DROP TABLE IF EXISTS candles;
DROP TABLE IF EXISTS live_quotes;
DROP TABLE IF EXISTS backfill_jobs;
DROP TABLE IF EXISTS tracked_markets CASCADE;

-- ── coin_quotes: coin-keyed spot price time-series ───────────────────────────────

CREATE TABLE IF NOT EXISTS coin_quotes (
    coin_id     TEXT        NOT NULL
                    REFERENCES tracked_coins(coin_id) ON DELETE CASCADE,
    vs_currency TEXT        NOT NULL,
    ts          TIMESTAMPTZ NOT NULL,
    price       NUMERIC     NOT NULL,
    source      TEXT        NOT NULL,
    PRIMARY KEY (coin_id, vs_currency, ts)
) PARTITION BY RANGE (ts);

-- Parent-level btree index: coin-scoped reads ordered newest-first (REQ-DB-015).
CREATE INDEX IF NOT EXISTS coin_quotes_coin_id_vs_currency_ts_idx
    ON coin_quotes (coin_id, vs_currency, ts DESC);

-- Parent-level BRIN index: large append-ordered time-range scans (REQ-DB-015).
CREATE INDEX IF NOT EXISTS coin_quotes_ts_brin
    ON coin_quotes USING BRIN (ts);

-- ── Monthly partitions: 2024-01 through 2027-12 (UTC boundaries) ─────────────────

CREATE TABLE IF NOT EXISTS coin_quotes_2024_01 PARTITION OF coin_quotes FOR VALUES FROM ('2024-01-01') TO ('2024-02-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2024_02 PARTITION OF coin_quotes FOR VALUES FROM ('2024-02-01') TO ('2024-03-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2024_03 PARTITION OF coin_quotes FOR VALUES FROM ('2024-03-01') TO ('2024-04-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2024_04 PARTITION OF coin_quotes FOR VALUES FROM ('2024-04-01') TO ('2024-05-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2024_05 PARTITION OF coin_quotes FOR VALUES FROM ('2024-05-01') TO ('2024-06-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2024_06 PARTITION OF coin_quotes FOR VALUES FROM ('2024-06-01') TO ('2024-07-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2024_07 PARTITION OF coin_quotes FOR VALUES FROM ('2024-07-01') TO ('2024-08-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2024_08 PARTITION OF coin_quotes FOR VALUES FROM ('2024-08-01') TO ('2024-09-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2024_09 PARTITION OF coin_quotes FOR VALUES FROM ('2024-09-01') TO ('2024-10-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2024_10 PARTITION OF coin_quotes FOR VALUES FROM ('2024-10-01') TO ('2024-11-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2024_11 PARTITION OF coin_quotes FOR VALUES FROM ('2024-11-01') TO ('2024-12-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2024_12 PARTITION OF coin_quotes FOR VALUES FROM ('2024-12-01') TO ('2025-01-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2025_01 PARTITION OF coin_quotes FOR VALUES FROM ('2025-01-01') TO ('2025-02-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2025_02 PARTITION OF coin_quotes FOR VALUES FROM ('2025-02-01') TO ('2025-03-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2025_03 PARTITION OF coin_quotes FOR VALUES FROM ('2025-03-01') TO ('2025-04-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2025_04 PARTITION OF coin_quotes FOR VALUES FROM ('2025-04-01') TO ('2025-05-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2025_05 PARTITION OF coin_quotes FOR VALUES FROM ('2025-05-01') TO ('2025-06-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2025_06 PARTITION OF coin_quotes FOR VALUES FROM ('2025-06-01') TO ('2025-07-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2025_07 PARTITION OF coin_quotes FOR VALUES FROM ('2025-07-01') TO ('2025-08-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2025_08 PARTITION OF coin_quotes FOR VALUES FROM ('2025-08-01') TO ('2025-09-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2025_09 PARTITION OF coin_quotes FOR VALUES FROM ('2025-09-01') TO ('2025-10-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2025_10 PARTITION OF coin_quotes FOR VALUES FROM ('2025-10-01') TO ('2025-11-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2025_11 PARTITION OF coin_quotes FOR VALUES FROM ('2025-11-01') TO ('2025-12-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2025_12 PARTITION OF coin_quotes FOR VALUES FROM ('2025-12-01') TO ('2026-01-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2026_01 PARTITION OF coin_quotes FOR VALUES FROM ('2026-01-01') TO ('2026-02-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2026_02 PARTITION OF coin_quotes FOR VALUES FROM ('2026-02-01') TO ('2026-03-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2026_03 PARTITION OF coin_quotes FOR VALUES FROM ('2026-03-01') TO ('2026-04-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2026_04 PARTITION OF coin_quotes FOR VALUES FROM ('2026-04-01') TO ('2026-05-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2026_05 PARTITION OF coin_quotes FOR VALUES FROM ('2026-05-01') TO ('2026-06-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2026_06 PARTITION OF coin_quotes FOR VALUES FROM ('2026-06-01') TO ('2026-07-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2026_07 PARTITION OF coin_quotes FOR VALUES FROM ('2026-07-01') TO ('2026-08-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2026_08 PARTITION OF coin_quotes FOR VALUES FROM ('2026-08-01') TO ('2026-09-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2026_09 PARTITION OF coin_quotes FOR VALUES FROM ('2026-09-01') TO ('2026-10-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2026_10 PARTITION OF coin_quotes FOR VALUES FROM ('2026-10-01') TO ('2026-11-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2026_11 PARTITION OF coin_quotes FOR VALUES FROM ('2026-11-01') TO ('2026-12-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2026_12 PARTITION OF coin_quotes FOR VALUES FROM ('2026-12-01') TO ('2027-01-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2027_01 PARTITION OF coin_quotes FOR VALUES FROM ('2027-01-01') TO ('2027-02-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2027_02 PARTITION OF coin_quotes FOR VALUES FROM ('2027-02-01') TO ('2027-03-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2027_03 PARTITION OF coin_quotes FOR VALUES FROM ('2027-03-01') TO ('2027-04-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2027_04 PARTITION OF coin_quotes FOR VALUES FROM ('2027-04-01') TO ('2027-05-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2027_05 PARTITION OF coin_quotes FOR VALUES FROM ('2027-05-01') TO ('2027-06-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2027_06 PARTITION OF coin_quotes FOR VALUES FROM ('2027-06-01') TO ('2027-07-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2027_07 PARTITION OF coin_quotes FOR VALUES FROM ('2027-07-01') TO ('2027-08-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2027_08 PARTITION OF coin_quotes FOR VALUES FROM ('2027-08-01') TO ('2027-09-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2027_09 PARTITION OF coin_quotes FOR VALUES FROM ('2027-09-01') TO ('2027-10-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2027_10 PARTITION OF coin_quotes FOR VALUES FROM ('2027-10-01') TO ('2027-11-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2027_11 PARTITION OF coin_quotes FOR VALUES FROM ('2027-11-01') TO ('2027-12-01');
CREATE TABLE IF NOT EXISTS coin_quotes_2027_12 PARTITION OF coin_quotes FOR VALUES FROM ('2027-12-01') TO ('2028-01-01');

-- ── coin_candles: coin-keyed OHLCV time-series ───────────────────────────────────

CREATE TABLE IF NOT EXISTS coin_candles (
    coin_id     TEXT        NOT NULL
                    REFERENCES tracked_coins(coin_id) ON DELETE CASCADE,
    vs_currency TEXT        NOT NULL,
    interval    TEXT        NOT NULL,
    ts          TIMESTAMPTZ NOT NULL,
    open        NUMERIC     NOT NULL,
    high        NUMERIC     NOT NULL,
    low         NUMERIC     NOT NULL,
    close       NUMERIC     NOT NULL,
    volume      NUMERIC,        -- nullable: CoinGecko OHLC has no volume (REQ-DB-011)
    source      TEXT        NOT NULL,
    PRIMARY KEY (coin_id, vs_currency, interval, ts)
) PARTITION BY RANGE (ts);

-- Parent-level btree index: coin-scoped reads ordered newest-first (REQ-DB-015).
CREATE INDEX IF NOT EXISTS coin_candles_coin_id_vs_currency_interval_ts_idx
    ON coin_candles (coin_id, vs_currency, interval, ts DESC);

-- Parent-level BRIN index: large append-ordered time-range scans (REQ-DB-015).
CREATE INDEX IF NOT EXISTS coin_candles_ts_brin
    ON coin_candles USING BRIN (ts);

-- ── Monthly partitions: 2024-01 through 2027-12 (UTC boundaries) ─────────────────

CREATE TABLE IF NOT EXISTS coin_candles_2024_01 PARTITION OF coin_candles FOR VALUES FROM ('2024-01-01') TO ('2024-02-01');
CREATE TABLE IF NOT EXISTS coin_candles_2024_02 PARTITION OF coin_candles FOR VALUES FROM ('2024-02-01') TO ('2024-03-01');
CREATE TABLE IF NOT EXISTS coin_candles_2024_03 PARTITION OF coin_candles FOR VALUES FROM ('2024-03-01') TO ('2024-04-01');
CREATE TABLE IF NOT EXISTS coin_candles_2024_04 PARTITION OF coin_candles FOR VALUES FROM ('2024-04-01') TO ('2024-05-01');
CREATE TABLE IF NOT EXISTS coin_candles_2024_05 PARTITION OF coin_candles FOR VALUES FROM ('2024-05-01') TO ('2024-06-01');
CREATE TABLE IF NOT EXISTS coin_candles_2024_06 PARTITION OF coin_candles FOR VALUES FROM ('2024-06-01') TO ('2024-07-01');
CREATE TABLE IF NOT EXISTS coin_candles_2024_07 PARTITION OF coin_candles FOR VALUES FROM ('2024-07-01') TO ('2024-08-01');
CREATE TABLE IF NOT EXISTS coin_candles_2024_08 PARTITION OF coin_candles FOR VALUES FROM ('2024-08-01') TO ('2024-09-01');
CREATE TABLE IF NOT EXISTS coin_candles_2024_09 PARTITION OF coin_candles FOR VALUES FROM ('2024-09-01') TO ('2024-10-01');
CREATE TABLE IF NOT EXISTS coin_candles_2024_10 PARTITION OF coin_candles FOR VALUES FROM ('2024-10-01') TO ('2024-11-01');
CREATE TABLE IF NOT EXISTS coin_candles_2024_11 PARTITION OF coin_candles FOR VALUES FROM ('2024-11-01') TO ('2024-12-01');
CREATE TABLE IF NOT EXISTS coin_candles_2024_12 PARTITION OF coin_candles FOR VALUES FROM ('2024-12-01') TO ('2025-01-01');
CREATE TABLE IF NOT EXISTS coin_candles_2025_01 PARTITION OF coin_candles FOR VALUES FROM ('2025-01-01') TO ('2025-02-01');
CREATE TABLE IF NOT EXISTS coin_candles_2025_02 PARTITION OF coin_candles FOR VALUES FROM ('2025-02-01') TO ('2025-03-01');
CREATE TABLE IF NOT EXISTS coin_candles_2025_03 PARTITION OF coin_candles FOR VALUES FROM ('2025-03-01') TO ('2025-04-01');
CREATE TABLE IF NOT EXISTS coin_candles_2025_04 PARTITION OF coin_candles FOR VALUES FROM ('2025-04-01') TO ('2025-05-01');
CREATE TABLE IF NOT EXISTS coin_candles_2025_05 PARTITION OF coin_candles FOR VALUES FROM ('2025-05-01') TO ('2025-06-01');
CREATE TABLE IF NOT EXISTS coin_candles_2025_06 PARTITION OF coin_candles FOR VALUES FROM ('2025-06-01') TO ('2025-07-01');
CREATE TABLE IF NOT EXISTS coin_candles_2025_07 PARTITION OF coin_candles FOR VALUES FROM ('2025-07-01') TO ('2025-08-01');
CREATE TABLE IF NOT EXISTS coin_candles_2025_08 PARTITION OF coin_candles FOR VALUES FROM ('2025-08-01') TO ('2025-09-01');
CREATE TABLE IF NOT EXISTS coin_candles_2025_09 PARTITION OF coin_candles FOR VALUES FROM ('2025-09-01') TO ('2025-10-01');
CREATE TABLE IF NOT EXISTS coin_candles_2025_10 PARTITION OF coin_candles FOR VALUES FROM ('2025-10-01') TO ('2025-11-01');
CREATE TABLE IF NOT EXISTS coin_candles_2025_11 PARTITION OF coin_candles FOR VALUES FROM ('2025-11-01') TO ('2025-12-01');
CREATE TABLE IF NOT EXISTS coin_candles_2025_12 PARTITION OF coin_candles FOR VALUES FROM ('2025-12-01') TO ('2026-01-01');
CREATE TABLE IF NOT EXISTS coin_candles_2026_01 PARTITION OF coin_candles FOR VALUES FROM ('2026-01-01') TO ('2026-02-01');
CREATE TABLE IF NOT EXISTS coin_candles_2026_02 PARTITION OF coin_candles FOR VALUES FROM ('2026-02-01') TO ('2026-03-01');
CREATE TABLE IF NOT EXISTS coin_candles_2026_03 PARTITION OF coin_candles FOR VALUES FROM ('2026-03-01') TO ('2026-04-01');
CREATE TABLE IF NOT EXISTS coin_candles_2026_04 PARTITION OF coin_candles FOR VALUES FROM ('2026-04-01') TO ('2026-05-01');
CREATE TABLE IF NOT EXISTS coin_candles_2026_05 PARTITION OF coin_candles FOR VALUES FROM ('2026-05-01') TO ('2026-06-01');
CREATE TABLE IF NOT EXISTS coin_candles_2026_06 PARTITION OF coin_candles FOR VALUES FROM ('2026-06-01') TO ('2026-07-01');
CREATE TABLE IF NOT EXISTS coin_candles_2026_07 PARTITION OF coin_candles FOR VALUES FROM ('2026-07-01') TO ('2026-08-01');
CREATE TABLE IF NOT EXISTS coin_candles_2026_08 PARTITION OF coin_candles FOR VALUES FROM ('2026-08-01') TO ('2026-09-01');
CREATE TABLE IF NOT EXISTS coin_candles_2026_09 PARTITION OF coin_candles FOR VALUES FROM ('2026-09-01') TO ('2026-10-01');
CREATE TABLE IF NOT EXISTS coin_candles_2026_10 PARTITION OF coin_candles FOR VALUES FROM ('2026-10-01') TO ('2026-11-01');
CREATE TABLE IF NOT EXISTS coin_candles_2026_11 PARTITION OF coin_candles FOR VALUES FROM ('2026-11-01') TO ('2026-12-01');
CREATE TABLE IF NOT EXISTS coin_candles_2026_12 PARTITION OF coin_candles FOR VALUES FROM ('2026-12-01') TO ('2027-01-01');
CREATE TABLE IF NOT EXISTS coin_candles_2027_01 PARTITION OF coin_candles FOR VALUES FROM ('2027-01-01') TO ('2027-02-01');
CREATE TABLE IF NOT EXISTS coin_candles_2027_02 PARTITION OF coin_candles FOR VALUES FROM ('2027-02-01') TO ('2027-03-01');
CREATE TABLE IF NOT EXISTS coin_candles_2027_03 PARTITION OF coin_candles FOR VALUES FROM ('2027-03-01') TO ('2027-04-01');
CREATE TABLE IF NOT EXISTS coin_candles_2027_04 PARTITION OF coin_candles FOR VALUES FROM ('2027-04-01') TO ('2027-05-01');
CREATE TABLE IF NOT EXISTS coin_candles_2027_05 PARTITION OF coin_candles FOR VALUES FROM ('2027-05-01') TO ('2027-06-01');
CREATE TABLE IF NOT EXISTS coin_candles_2027_06 PARTITION OF coin_candles FOR VALUES FROM ('2027-06-01') TO ('2027-07-01');
CREATE TABLE IF NOT EXISTS coin_candles_2027_07 PARTITION OF coin_candles FOR VALUES FROM ('2027-07-01') TO ('2027-08-01');
CREATE TABLE IF NOT EXISTS coin_candles_2027_08 PARTITION OF coin_candles FOR VALUES FROM ('2027-08-01') TO ('2027-09-01');
CREATE TABLE IF NOT EXISTS coin_candles_2027_09 PARTITION OF coin_candles FOR VALUES FROM ('2027-09-01') TO ('2027-10-01');
CREATE TABLE IF NOT EXISTS coin_candles_2027_10 PARTITION OF coin_candles FOR VALUES FROM ('2027-10-01') TO ('2027-11-01');
CREATE TABLE IF NOT EXISTS coin_candles_2027_11 PARTITION OF coin_candles FOR VALUES FROM ('2027-11-01') TO ('2027-12-01');
CREATE TABLE IF NOT EXISTS coin_candles_2027_12 PARTITION OF coin_candles FOR VALUES FROM ('2027-12-01') TO ('2028-01-01');
