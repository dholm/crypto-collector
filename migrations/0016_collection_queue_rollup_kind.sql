-- SPEC-CANDLE-001 REQ-CANDLE-042: permit the network-free candle-rollup recompute to be
-- enqueued.
--
-- 0014_collection_queue_cycle_overlay_kind.sql widened collection_queue_kind_check to
-- ('spot', 'candles', 'metadata', 'market', 'derivatives', 'cycle_overlay'). The 'rollup'
-- kind added by SPEC-CANDLE-001 (native 1d/1w OHLCV materialization) is not yet in that
-- enumeration, so both the post-candles enqueue (REQ-CANDLE-020) and the periodic backstop
-- enqueue (REQ-CANDLE-021) would fail at runtime ("violates check constraint
-- collection_queue_kind_check"). Widen the enumeration to include 'rollup'.

ALTER TABLE collection_queue
    DROP CONSTRAINT IF EXISTS collection_queue_kind_check;

ALTER TABLE collection_queue
    ADD CONSTRAINT collection_queue_kind_check
        CHECK (kind IN ('spot', 'candles', 'metadata', 'market', 'derivatives', 'cycle_overlay', 'rollup'));
