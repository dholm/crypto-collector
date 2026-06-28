-- SPEC-DB-001 migration 0006: coin_metadata revisioned table (REQ-DB-020/021/022/023).
--
-- Stores slowly-changing descriptive coin metadata using the revision pattern
-- (adapted from ticker-collector SPEC-FUND-002, migration 0010).
--
-- PK: (coin_id, revision) — 0-based revision counter.
-- Revision is incremented ONLY when a tracked value changes (IS NOT DISTINCT FROM comparison).
-- When metadata is re-collected unchanged, only last_seen_at advances (no new revision row).
--
-- max_supply: NUMERIC — fixed for most assets; NULL for assets without a hard cap (e.g. ETH).
-- links / contract_addresses: JSONB — structured external links and on-chain addresses.
-- categories: TEXT[] — list of CoinGecko taxonomy categories.
--
-- Continuously-changing aggregates (market_cap, supply, price, FDV) live in coin_market_snapshots,
-- NOT here, to prevent revision churn on every poll (REQ-DB-022, research §4.3).
--
-- As-of index: btree(coin_id, first_seen_at DESC) supports "greatest first_seen_at <= as_of" reads
-- needed for point-in-time metadata lookups (REQ-DB-023).

CREATE TABLE IF NOT EXISTS coin_metadata (
    coin_id            TEXT        NOT NULL
                           REFERENCES tracked_coins(coin_id) ON DELETE CASCADE,
    revision           INTEGER     NOT NULL DEFAULT 0,
    name               TEXT        NOT NULL,
    symbol             TEXT        NOT NULL,
    categories         TEXT[],
    description        TEXT,
    homepage           TEXT,
    links              JSONB,
    contract_addresses JSONB,
    -- Nullable: ETH, DOGE, XMR have no hard cap.
    max_supply         NUMERIC,
    genesis_date       DATE,
    first_seen_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (coin_id, revision)
);

-- As-of index: supports "greatest first_seen_at <= as_of" point-in-time reads (REQ-DB-023).
-- Use case: SELECT * FROM coin_metadata WHERE coin_id = $1 AND first_seen_at <= $as_of
--           ORDER BY first_seen_at DESC LIMIT 1
CREATE INDEX IF NOT EXISTS coin_metadata_coin_id_first_seen_at_idx
    ON coin_metadata (coin_id, first_seen_at DESC);
