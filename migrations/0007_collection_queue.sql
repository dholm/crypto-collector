-- SPEC-DB-001 migration 0007: collection_queue durable work queue (REQ-DB-030/031/032/036).
--
-- Durable work queue for coin and market data collection tasks.
-- Workers claim rows atomically via SELECT ... FOR UPDATE SKIP LOCKED LIMIT 1.
-- Lease + heartbeat pattern: a crashed worker's lease_expires_at expires and the row
-- becomes re-claimable without blocking other workers.
--
-- Status machine: pending → claimed → running → done | failed
-- target_kind: 'coin' or 'market'
-- target_id:   coin_id (text) or market_id (bigint as text)
-- kind:        'spot' | 'candles' | 'metadata' | 'market' | 'derivatives'
--
-- Dedup partial unique index (REQ-DB-031):
--   At most one live item per (target_kind, target_id, kind).
--   Partial on live statuses so a new item can be enqueued once the prior one reaches done/failed.
--
-- Two claim indexes (REQ-DB-032/036):
--   Pending path:    btree(enqueued_at) WHERE status='pending'    — oldest-first fair claiming
--   Lease-expired:   btree(lease_expires_at) WHERE status IN ('claimed','running') — re-claim
--
-- @MX:NOTE: [AUTO] claim query shape:
--   Pending:      SELECT ... WHERE status='pending' ORDER BY enqueued_at FOR UPDATE SKIP LOCKED LIMIT 1
--   Lease-expired: SELECT ... WHERE status IN ('claimed','running') AND lease_expires_at < now()
--                  ORDER BY lease_expires_at FOR UPDATE SKIP LOCKED LIMIT 1

CREATE TABLE IF NOT EXISTS collection_queue (
    id               BIGSERIAL   NOT NULL,
    target_kind      TEXT        NOT NULL
                         CHECK (target_kind IN ('coin', 'market')),
    target_id        TEXT        NOT NULL,
    kind             TEXT        NOT NULL
                         CHECK (kind IN ('spot', 'candles', 'metadata', 'market', 'derivatives')),
    status           TEXT        NOT NULL DEFAULT 'pending'
                         CHECK (status IN ('pending', 'claimed', 'running', 'done', 'failed')),
    claimed_by       TEXT,
    lease_expires_at TIMESTAMPTZ,
    heartbeat_at     TIMESTAMPTZ,
    attempts         INTEGER     NOT NULL DEFAULT 0,
    last_error       TEXT,
    enqueued_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (id)
);

-- Dedup: at most one live row per (target_kind, target_id, kind).
-- Partial predicate covers only live statuses; done/failed rows do not block re-enqueue (REQ-DB-031).
CREATE UNIQUE INDEX IF NOT EXISTS collection_queue_dedup_idx
    ON collection_queue (target_kind, target_id, kind)
    WHERE status IN ('pending', 'claimed', 'running');

-- Pending-path claim index: oldest-first fair claiming of pending items (REQ-DB-032).
-- Supports: SELECT ... WHERE status='pending' ORDER BY enqueued_at FOR UPDATE SKIP LOCKED LIMIT 1
CREATE INDEX IF NOT EXISTS collection_queue_claim_pending_idx
    ON collection_queue (enqueued_at)
    WHERE status = 'pending';

-- Lease-expired re-claim index: reclaims items whose lease has expired (REQ-DB-036).
-- Supports: SELECT ... WHERE status IN ('claimed','running') AND lease_expires_at < now()
--           ORDER BY lease_expires_at FOR UPDATE SKIP LOCKED LIMIT 1
CREATE INDEX IF NOT EXISTS collection_queue_claim_lease_expired_idx
    ON collection_queue (lease_expires_at)
    WHERE status IN ('claimed', 'running');
