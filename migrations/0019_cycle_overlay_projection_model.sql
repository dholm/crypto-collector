-- SPEC-CYCLE-001: distinguish which projection model produced a cycle_overlay_points row.
--
-- Restores the Bitbo-style cycle-repeat replay model (project_cycle_repeat) alongside the
-- composite model introduced in migrations/0015 + 0017 (da25bc0). Both models are now
-- materialised concurrently, keyed by the same (cycle_number, days_since_halving) — hence the
-- discriminator must join the PRIMARY KEY, not merely be a filterable column.
--
-- Values: 'real' (observed points, projected = FALSE), 'replay' (cycle-repeat replay,
-- served by GET /coins/{coin_id}/cycle-overlay), 'composite' (power-law + phase + mean-reversion
-- model, served by GET /coins/{coin_id}/cycle-projection).

ALTER TABLE cycle_overlay_points
    ADD COLUMN IF NOT EXISTS projection_model TEXT NOT NULL DEFAULT 'composite';

UPDATE cycle_overlay_points SET projection_model = 'real' WHERE projected = FALSE;
-- projected rows already carry the DEFAULT 'composite' (the only model materialised pre-0019).

ALTER TABLE cycle_overlay_points DROP CONSTRAINT IF EXISTS cycle_overlay_points_pkey;
ALTER TABLE cycle_overlay_points
    ADD PRIMARY KEY (coin_id, vs_currency, projection_model, cycle_number, days_since_halving);

DROP INDEX IF EXISTS cycle_overlay_points_read_idx;
CREATE INDEX IF NOT EXISTS cycle_overlay_points_read_idx
    ON cycle_overlay_points (coin_id, vs_currency, projection_model, cycle_number, days_since_halving);
