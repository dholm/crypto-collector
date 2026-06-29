-- Migration 0012: coin-keyed backfill coordination tables.
--
-- Migration 0011 dropped the market-keyed backfill_jobs (and backfill_chunks via CASCADE).
-- This migration creates coin-keyed replacements targeting tracked_coins.
--
-- @MX:NOTE: backfill_jobs UNIQUE (coin_id, dataset) — idempotent enqueue (REQ-DB-033)

CREATE TABLE IF NOT EXISTS backfill_jobs (
    id           BIGSERIAL   NOT NULL,
    coin_id      TEXT        NOT NULL
                     REFERENCES tracked_coins(coin_id) ON DELETE CASCADE,
    dataset      TEXT        NOT NULL,
    status       TEXT        NOT NULL DEFAULT 'pending',
    requested_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (id),
    UNIQUE (coin_id, dataset)
);

CREATE TABLE IF NOT EXISTS backfill_chunks (
    id               BIGSERIAL   NOT NULL,
    job_id           BIGINT      NOT NULL
                         REFERENCES backfill_jobs(id) ON DELETE CASCADE,
    coin_id          TEXT        NOT NULL,
    dataset          TEXT        NOT NULL,
    interval         TEXT,
    range_start      TIMESTAMPTZ,
    range_end        TIMESTAMPTZ,
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

CREATE INDEX IF NOT EXISTS backfill_chunks_claim_idx
    ON backfill_chunks (created_at)
    WHERE status = 'pending';
