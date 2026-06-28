-- SPEC-DB-001 migration 0008: backfill coordination tables (REQ-DB-033).
--
-- backfill_jobs:   one job per (market_id, dataset); fans out into backfill_chunks.
--   UNIQUE (market_id, dataset) makes enqueue idempotent (REQ-DB-033).
--
-- backfill_chunks: the claimable work unit. Crash-resumable via lease + heartbeat + cursor.
--   cursor: durable resume marker — last successfully persisted point within [range_start, range_end).
--   Claim index: btree(created_at) WHERE status='pending' for oldest-first claiming.
--
-- Status machine for both tables: pending → claimed → running → done | failed
-- Reclaim: any claimed/running chunk with lease_expires_at < now() is re-claimable.
--
-- Adapted from ticker-collector SPEC-DB-002 migration 0016, with market_id (BIGINT FK) instead
-- of symbol (TEXT FK), and with an interval column for candle dataset granularity.

CREATE TABLE IF NOT EXISTS backfill_jobs (
    id           BIGSERIAL   NOT NULL,
    market_id    BIGINT      NOT NULL
                     REFERENCES tracked_markets(id) ON DELETE CASCADE,
    dataset      TEXT        NOT NULL,
    status       TEXT        NOT NULL DEFAULT 'pending',
    requested_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (id),
    UNIQUE (market_id, dataset)
);

CREATE TABLE IF NOT EXISTS backfill_chunks (
    id               BIGSERIAL   NOT NULL,
    job_id           BIGINT      NOT NULL
                         REFERENCES backfill_jobs(id) ON DELETE CASCADE,
    market_id        BIGINT      NOT NULL,
    dataset          TEXT        NOT NULL,
    -- Candle granularity (e.g. '1h', '1d'); NULL for non-candle datasets.
    interval         TEXT,
    -- Time window for this chunk. NULL bounds = whole-dataset single-fetch chunk.
    range_start      TIMESTAMPTZ,
    range_end        TIMESTAMPTZ,
    -- Durable resume marker: last successfully persisted ts within this chunk's window.
    cursor           TIMESTAMPTZ,
    status           TEXT        NOT NULL DEFAULT 'pending',
    claimed_by       TEXT,
    lease_expires_at TIMESTAMPTZ,
    heartbeat_at     TIMESTAMPTZ,
    attempts         INTEGER     NOT NULL DEFAULT 0,
    last_error       TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (id)
);

-- Claim index for backfill_chunks: oldest-first pending claiming.
-- Supports: SELECT ... WHERE status='pending' ORDER BY created_at FOR UPDATE SKIP LOCKED LIMIT 1
CREATE INDEX IF NOT EXISTS backfill_chunks_claim_idx
    ON backfill_chunks (created_at)
    WHERE status = 'pending';
