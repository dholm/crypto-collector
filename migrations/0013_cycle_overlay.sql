-- Migration 0013: Bitcoin halving-cycle overlay (SPEC-CYCLE-001 REQ-CYCLE-040).
--
-- Materialised, idempotently-rebuilt derived-analytics table. Every row is a pure
-- function of the persisted daily (`1d`) `coin_candles` history for the configured
-- target coin/currency (REQ-CYCLE-001/041/042); nothing here is fetched from an
-- upstream provider directly.
--
-- @MX:NOTE: full idempotent rebuild — a recompute DELETEs all rows for
--           (coin_id, vs_currency) and re-INSERTs, so there is no UPDATE path here.

CREATE TABLE IF NOT EXISTS cycle_overlay_points (
    coin_id                       TEXT        NOT NULL,
    vs_currency                   TEXT        NOT NULL,
    cycle_number                  INTEGER     NOT NULL,
    halving_date                  DATE        NOT NULL,
    days_since_halving            INTEGER     NOT NULL,
    ts                            DATE        NOT NULL,
    price                         NUMERIC     NOT NULL,
    norm_halving                  NUMERIC     NOT NULL,
    norm_cycle_low                NUMERIC     NOT NULL,
    halving_baseline_approximate  BOOLEAN     NOT NULL DEFAULT FALSE,
    updated_at                    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (coin_id, vs_currency, cycle_number, days_since_halving)
);

-- Read-route pagination index: ordered (cycle_number ASC, days_since_halving ASC)
-- scoped per (coin_id, vs_currency) — mirrors the PK order so the keyset read route
-- (REQ-CYCLE-050/051) can range-scan without a sort.
CREATE INDEX IF NOT EXISTS cycle_overlay_points_read_idx
    ON cycle_overlay_points (coin_id, vs_currency, cycle_number, days_since_halving);
