-- SPEC-CYCLE-001 REQ-CYCLE-041: permit the periodic cycle-overlay recompute to be enqueued.
--
-- 0007_collection_queue.sql declared `kind` with an inline, unnamed
-- CHECK (kind IN ('spot', 'candles', 'metadata', 'market', 'derivatives')), which Postgres
-- auto-named `collection_queue_kind_check`. The 'cycle_overlay' recompute kind added by
-- SPEC-CYCLE-001 was never added to that enumeration, so the periodic enqueue failed at
-- runtime ("violates check constraint collection_queue_kind_check") and the overlay was
-- never rebuilt. Widen the enumeration to include 'cycle_overlay'.

ALTER TABLE collection_queue
    DROP CONSTRAINT IF EXISTS collection_queue_kind_check;

ALTER TABLE collection_queue
    ADD CONSTRAINT collection_queue_kind_check
        CHECK (kind IN ('spot', 'candles', 'metadata', 'market', 'derivatives', 'cycle_overlay'));
