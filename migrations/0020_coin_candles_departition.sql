-- De-partition coin_candles (SPEC-DB-001 REQ-DB-014 amendment).
--
-- coin_candles was RANGE(ts) partitioned one-partition-per-month (0011). At the
-- realized data volume (~1.25M rows / ~324 MB across ~200 monthly partitions) the
-- partitioning is pure overhead: every coverage/EXISTS/min-max query that cannot
-- prune to a few months forces the planner to lock and consider ~200 partitions
-- plus their ~400 indexes, costing 0.3-1.5 s of PLANNING time per statement (actual
-- execution is tens of ms). This surfaced as a steady stream of sqlx
-- "slow statement" WARN logs. A single well-indexed table plans in sub-milliseconds
-- and handles this volume trivially; monthly partitioning only pays off at hundreds
-- of millions of rows or when partition-drop retention is used (neither applies —
-- full history since 2011 is retained).
--
-- This migration converts coin_candles from a partitioned parent into a plain table,
-- preserving every row, the primary key, the tracked_coins foreign key, and the
-- btree + BRIN index contract (REQ-DB-015). It also removes the runtime
-- CREATE-TABLE-on-insert path (ensure_candle_partition), which is no longer needed
-- and cannot target a non-partitioned table.
--
-- Runs in a single transaction (sqlx default): either the swap fully succeeds or the
-- original partitioned table is left untouched.

-- Move the partitioned parent (and all its child partitions) aside.
ALTER TABLE coin_candles RENAME TO coin_candles_partitioned;

-- Flat replacement: identical columns, PK, and FK to the original (0011) definition.
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
);

-- Copy every row across (explicit column list — order-independent, robust).
INSERT INTO coin_candles
    (coin_id, vs_currency, interval, ts, open, high, low, close, volume, source)
SELECT
    coin_id, vs_currency, interval, ts, open, high, low, close, volume, source
FROM coin_candles_partitioned;

-- Restore the read-path index contract (REQ-DB-015): coin-scoped newest-first btree
-- for point/range reads, BRIN for large append-ordered time scans.
CREATE INDEX IF NOT EXISTS coin_candles_coin_id_vs_currency_interval_ts_idx
    ON coin_candles (coin_id, vs_currency, interval, ts DESC);

CREATE INDEX IF NOT EXISTS coin_candles_ts_brin
    ON coin_candles USING BRIN (ts);

-- Drop the old partitioned parent; CASCADE removes every child partition.
DROP TABLE coin_candles_partitioned;

-- Refresh planner statistics on the fresh table so the first post-migration reads
-- get accurate plans without waiting for autovacuum.
ANALYZE coin_candles;
