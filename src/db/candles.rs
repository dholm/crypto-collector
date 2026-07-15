//! Shared `coin_candles` read helpers.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// Per-interval coverage span `(interval, earliest_ts, latest_ts)` for one
/// `(coin_id, vs_currency)` series — one row per stored interval.
///
/// Consumed by the rollup materializer, the API aggregation fallback, and the
/// cycle-overlay daily derivation, which all weigh history depth and staleness
/// (not just bucket divisibility) when picking an aggregation source interval.
///
/// The naive form — `SELECT interval, MIN(ts), MAX(ts) … GROUP BY interval` —
/// forces a full scan of every row for the coin (≈1M for a deep 5m backfill),
/// costing ~1s and tripping sqlx's slow-statement threshold. This is a
/// **loose index scan**: a recursive skip over the leading
/// `(coin_id, vs_currency, interval, …)` btree columns visits only the handful
/// of *distinct* interval boundaries, then each interval's earliest/latest is an
/// `ORDER BY ts … LIMIT 1` index-endpoint seek. Result is identical; cost drops
/// from ~1s to sub-millisecond regardless of history depth.
//
// @MX:NOTE: [AUTO] interval_coverage — index-driven equivalent of a
//           `MIN(ts)/MAX(ts) … GROUP BY interval` probe. Relies on the
//           `(coin_id, vs_currency, interval, ts DESC)` btree; a schema change
//           dropping that index would silently regress this to a full scan.
pub async fn interval_coverage(
    pool: &PgPool,
    coin_id: &str,
    vs_currency: &str,
) -> Result<Vec<(String, DateTime<Utc>, DateTime<Utc>)>, sqlx::Error> {
    sqlx::query_as(
        "WITH RECURSIVE ivs AS ( \
             (SELECT interval FROM coin_candles \
               WHERE coin_id = $1 AND vs_currency = $2 \
               ORDER BY interval LIMIT 1) \
             UNION ALL \
             SELECT (SELECT c.interval FROM coin_candles c \
                      WHERE c.coin_id = $1 AND c.vs_currency = $2 \
                        AND c.interval > ivs.interval \
                      ORDER BY c.interval LIMIT 1) \
             FROM ivs \
             WHERE ivs.interval IS NOT NULL \
         ) \
         SELECT \
             iv.interval, \
             (SELECT c.ts FROM coin_candles c \
               WHERE c.coin_id = $1 AND c.vs_currency = $2 AND c.interval = iv.interval \
               ORDER BY c.ts ASC  LIMIT 1) AS earliest, \
             (SELECT c.ts FROM coin_candles c \
               WHERE c.coin_id = $1 AND c.vs_currency = $2 AND c.interval = iv.interval \
               ORDER BY c.ts DESC LIMIT 1) AS latest \
         FROM ivs iv \
         WHERE iv.interval IS NOT NULL",
    )
    .bind(coin_id)
    .bind(vs_currency)
    .fetch_all(pool)
    .await
}
