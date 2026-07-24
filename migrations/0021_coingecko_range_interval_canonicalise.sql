-- migrations/0021_coingecko_range_interval_canonicalise.sql
-- F-20 (SPEC-PROV-003): canonicalise CoinGecko range-path interval stamps
-- ('daily'->'1d', 'hourly'->'1h'). Idempotent + collision-safe: no-op when zero such rows
-- exist; never raises a PK unique-violation. coin_candles PK is
-- (coin_id, vs_currency, interval, ts) — a bare UPDATE would collide when a non-canonical row
-- and its canonical twin share (coin_id, vs_currency, ts).

-- Step 1: drop any non-canonical row that is shadowed by an existing canonical twin
-- (the canonical twin already carries the correct data).
DELETE FROM coin_candles c
WHERE c.interval IN ('daily', 'hourly')
  AND EXISTS (
      SELECT 1 FROM coin_candles t
      WHERE t.coin_id = c.coin_id
        AND t.vs_currency = c.vs_currency
        AND t.ts = c.ts
        AND t.interval = CASE c.interval WHEN 'daily' THEN '1d' WHEN 'hourly' THEN '1h' END
  );

-- Step 2: canonicalise the remaining non-canonical rows (now collision-free).
UPDATE coin_candles
SET interval = CASE interval WHEN 'daily' THEN '1d' WHEN 'hourly' THEN '1h' END
WHERE interval IN ('daily', 'hourly');
