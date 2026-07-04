-- Migration 0015: Projected current-cycle points for the Bitcoin halving-cycle overlay
-- (SPEC-CYCLE-001 REQ-CYCLE-060/061/062).
--
-- Adds `projected` to distinguish real (`false`) from projected (`true`) points. Projected
-- points repeat the last completed cycle's shape onto the current cycle out to the next
-- halving; they sort naturally after real current-cycle points under the existing
-- `(cycle_number, days_since_halving)` order, so the read-route cursor/keyset contract
-- (REQ-CYCLE-051) is unaffected — see `src/collectors/cycle_overlay.rs`.

ALTER TABLE cycle_overlay_points
    ADD COLUMN IF NOT EXISTS projected BOOLEAN NOT NULL DEFAULT FALSE;
