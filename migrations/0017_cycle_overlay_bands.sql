-- SPEC-CYCLE-001 v0.4.0 (REQ-CYCLE-064): P10/P90 confidence bands on projected
-- cycle-overlay points. Additive + nullable — real (projected = false) points carry
-- NULL bands; only projected points are populated by the composite projection model.
-- Backward compatible: existing readers ignore the new columns.

ALTER TABLE cycle_overlay_points
    ADD COLUMN IF NOT EXISTS price_low NUMERIC NULL,
    ADD COLUMN IF NOT EXISTS price_high NUMERIC NULL;
