//! Backfill worker (SPEC-SCHED-001 REQ-SCHED-020..028, 041).
//!
//! Claims `backfill_chunks` rows via `FOR UPDATE SKIP LOCKED` (oldest pending or
//! lease-expired first), fetches historical OHLC candles for the chunk's time window,
//! upserts candles, and advances the durable `cursor` on each successful batch.
//!
//! # Crash-resumable via cursor (REQ-SCHED-024/025)
//!
//! The `cursor` column is the last successfully persisted timestamp within the chunk's
//! `[range_start, range_end)` window. On restart (re-claim), the worker resumes from
//! `cursor` rather than the beginning of the range.
//!
//! # Lease + heartbeat + fencing (REQ-SCHED-022/023)
//!
//! All mutating UPDATEs after the claim guard with `AND claimed_by = $self`.
//! A heartbeat task keeps the lease alive during long fetches.

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tracing::{error, info, warn};

use crate::db::upserts::upsert_candles;
use crate::pacer::acquire_slot;
use crate::providers::{Capability, MarketQuery, OhlcCandle, Provider, ProviderError};

// ── Pure scheduling functions (unit-testable, no I/O) ────────────────────────

/// Determine the resume start for a backfill chunk (REQ-SCHED-024/025).
///
/// - If `cursor` is set, resume from just after the cursor (cursor + 1 nanosecond).
/// - If `cursor` is NULL but `range_start` is set, start from `range_start`.
/// - If both are NULL (whole-dataset single-fetch chunk), return `None` (let provider decide).
pub fn resume_start(
    cursor: Option<DateTime<Utc>>,
    range_start: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    cursor
        .map(|c| c + chrono::Duration::nanoseconds(1))
        .or(range_start)
}

/// Convert an `Option<DateTime<Utc>>` end to a `days` lookback count for the provider.
///
/// The `chain_fetch_ohlc` API accepts a `days: u32` window. We derive days from
/// `(range_end - resume_start).ceil()` when both are known. Falls back to `max_days`.
pub fn range_to_days(
    resume_start: Option<DateTime<Utc>>,
    range_end: Option<DateTime<Utc>>,
    max_days: u32,
) -> u32 {
    match (resume_start, range_end) {
        (Some(start), Some(end)) => {
            let diff = end.signed_duration_since(start);
            let days = diff.num_days().max(1) as u32;
            days.min(max_days)
        }
        _ => max_days,
    }
}

// ── SQL constants ─────────────────────────────────────────────────────────────

/// Claim one `backfill_chunks` row via `FOR UPDATE SKIP LOCKED` (REQ-SCHED-021/022).
///
/// Predicate: `status = 'pending'` OR (`status IN ('claimed','running')` AND lease expired).
/// Ordered oldest-first (`created_at ASC`) for fair claiming.
/// Increments `attempts` at claim time to bound retries (REQ-SCHED-027).
///
// @MX:ANCHOR: [AUTO] CLAIM_BACKFILL_SQL — FOR UPDATE SKIP LOCKED; at-most-one-replica per chunk
// @MX:REASON: fan_in >= 3: claim_backfill_chunk(), SQL-shape tests, DB integration tests.
//             REQ-SCHED-022: lease-expired re-claim enables crash recovery without orphaning chunks.
//             REQ-SCHED-027: attempts incremented at claim time for bound retry accounting.
// @MX:SPEC: SPEC-SCHED-001 REQ-SCHED-021 REQ-SCHED-022 REQ-SCHED-027
pub const CLAIM_BACKFILL_SQL: &str = "\
    UPDATE backfill_chunks SET \
        status           = 'claimed', \
        claimed_by       = $1, \
        lease_expires_at = now() + ($2 * INTERVAL '1 second'), \
        heartbeat_at     = now(), \
        attempts         = attempts + 1, \
        updated_at       = now() \
    WHERE id = ( \
        SELECT id FROM backfill_chunks \
        WHERE status = 'pending' \
           OR (status IN ('claimed','running') AND lease_expires_at < now()) \
        ORDER BY created_at \
        LIMIT 1 \
        FOR UPDATE SKIP LOCKED \
    ) \
    RETURNING id, job_id, market_id, dataset, interval, \
              range_start, range_end, cursor, status, \
              claimed_by, lease_expires_at, heartbeat_at, \
              attempts, last_error, created_at, updated_at";

/// Heartbeat UPDATE: renews lease (fencing guard: `AND claimed_by = $self`).
pub const HEARTBEAT_BACKFILL_SQL: &str = "\
    UPDATE backfill_chunks SET \
        lease_expires_at = now() + ($1 * INTERVAL '1 second'), \
        heartbeat_at     = now(), \
        updated_at       = now() \
    WHERE id = $2 AND claimed_by = $3";

/// Advance the cursor on a successful candle batch (REQ-SCHED-024).
///
/// Does NOT mark done — the loop calls this after each batch; `COMPLETE_BACKFILL_SQL`
/// marks done only after the full range is exhausted.
///
// @MX:NOTE: [AUTO] ADVANCE_CURSOR_SQL — durable resume marker; called after each successful batch
// @MX:SPEC: SPEC-SCHED-001 REQ-SCHED-024 REQ-SCHED-025
pub const ADVANCE_CURSOR_SQL: &str = "\
    UPDATE backfill_chunks SET \
        cursor     = $3, \
        updated_at = now() \
    WHERE id = $1 AND claimed_by = $2";

/// Mark a chunk as done after the full range is exhausted (REQ-SCHED-026).
pub const COMPLETE_BACKFILL_SQL: &str = "\
    UPDATE backfill_chunks SET \
        status           = 'done', \
        cursor           = range_end, \
        claimed_by       = NULL, \
        lease_expires_at = NULL, \
        updated_at       = now() \
    WHERE id = $1 AND claimed_by = $2";

/// Fail or retry a chunk: reset to `pending` if under max_attempts, else mark `failed` (REQ-SCHED-027).
pub const FAIL_OR_RETRY_BACKFILL_SQL: &str = "\
    UPDATE backfill_chunks SET \
        status           = CASE WHEN attempts >= $3 THEN 'failed' ELSE 'pending' END, \
        last_error       = $4, \
        claimed_by       = NULL, \
        lease_expires_at = NULL, \
        heartbeat_at     = NULL, \
        updated_at       = now() \
    WHERE id = $1 AND claimed_by = $2";

/// Enqueue a `backfill_job` + initial chunk idempotently (REQ-SCHED-028).
///
/// `ON CONFLICT DO NOTHING` absorbs duplicate job registrations.
pub const ENQUEUE_BACKFILL_JOB_SQL: &str = "\
    INSERT INTO backfill_jobs \
        (market_id, dataset, status, requested_at, updated_at) \
    VALUES ($1, $2, 'pending', now(), now()) \
    ON CONFLICT (market_id, dataset) DO NOTHING \
    RETURNING id";

/// Insert one chunk for a newly created job.
pub const INSERT_BACKFILL_CHUNK_SQL: &str = "\
    INSERT INTO backfill_chunks \
        (job_id, market_id, dataset, interval, range_start, range_end, \
         status, created_at, updated_at) \
    VALUES ($1, $2, $3, $4, $5, $6, 'pending', now(), now())";

// ── Structs ───────────────────────────────────────────────────────────────────

/// A successfully claimed `backfill_chunks` row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ClaimedChunk {
    pub id: i64,
    pub job_id: i64,
    pub market_id: i64,
    pub dataset: String,
    pub interval: Option<String>,
    pub range_start: Option<DateTime<Utc>>,
    pub range_end: Option<DateTime<Utc>>,
    pub cursor: Option<DateTime<Utc>>,
    pub status: String,
    pub claimed_by: Option<String>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ── DB functions ──────────────────────────────────────────────────────────────

/// Claim one `backfill_chunks` row via `FOR UPDATE SKIP LOCKED`.
///
/// Returns `None` when no claimable chunk exists.
pub async fn claim_backfill_chunk(
    pool: &PgPool,
    claimed_by: &str,
    lease_secs: i64,
) -> Result<Option<ClaimedChunk>, sqlx::Error> {
    let row: Option<ClaimedChunk> = sqlx::query_as(CLAIM_BACKFILL_SQL)
        .bind(claimed_by)
        .bind(lease_secs)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Renew the lease on an owned chunk. Returns `false` if the fencing guard fired.
pub async fn heartbeat_backfill_chunk(
    pool: &PgPool,
    id: i64,
    claimed_by: &str,
    lease_secs: i64,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(HEARTBEAT_BACKFILL_SQL)
        .bind(lease_secs)
        .bind(id)
        .bind(claimed_by)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Advance the durable cursor after successfully persisting a candle batch (REQ-SCHED-024).
pub async fn advance_cursor(
    pool: &PgPool,
    id: i64,
    claimed_by: &str,
    cursor: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(ADVANCE_CURSOR_SQL)
        .bind(id)
        .bind(claimed_by)
        .bind(cursor)
        .execute(pool)
        .await?;
    Ok(())
}

/// Mark a chunk as completely done (REQ-SCHED-026).
pub async fn complete_backfill_chunk(
    pool: &PgPool,
    id: i64,
    claimed_by: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(COMPLETE_BACKFILL_SQL)
        .bind(id)
        .bind(claimed_by)
        .execute(pool)
        .await?;
    Ok(())
}

/// Fail or retry a chunk (REQ-SCHED-027).
pub async fn fail_or_retry_backfill_chunk(
    pool: &PgPool,
    id: i64,
    claimed_by: &str,
    max_attempts: i32,
    error: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(FAIL_OR_RETRY_BACKFILL_SQL)
        .bind(id)
        .bind(claimed_by)
        .bind(max_attempts)
        .bind(error)
        .execute(pool)
        .await?;
    Ok(())
}

/// Enqueue a backfill job + initial chunk idempotently (REQ-SCHED-028).
///
/// Returns `true` if a new job was created, `false` if it already exists.
pub async fn enqueue_backfill_job(
    pool: &PgPool,
    market_id: i64,
    dataset: &str,
    interval: Option<&str>,
    range_start: Option<DateTime<Utc>>,
    range_end: Option<DateTime<Utc>>,
) -> Result<bool, sqlx::Error> {
    let job_id: Option<i64> = sqlx::query_scalar(ENQUEUE_BACKFILL_JOB_SQL)
        .bind(market_id)
        .bind(dataset)
        .fetch_optional(pool)
        .await?;

    let Some(job_id) = job_id else {
        return Ok(false); // already exists
    };

    sqlx::query(INSERT_BACKFILL_CHUNK_SQL)
        .bind(job_id)
        .bind(market_id)
        .bind(dataset)
        .bind(interval)
        .bind(range_start)
        .bind(range_end)
        .execute(pool)
        .await?;

    Ok(true)
}

// ── Chain dispatch helper ─────────────────────────────────────────────────────

async fn chain_fetch_ohlc_for_chunk(
    chain: &[Arc<dyn Provider>],
    market: &MarketQuery,
    days: u32,
) -> Result<Vec<OhlcCandle>, ProviderError> {
    let (result, _) = crate::providers::chain_fetch_ohlc(chain, market, days).await;
    result
}

fn first_ohlc_provider(chain: &[Arc<dyn Provider>]) -> Option<String> {
    chain
        .iter()
        .find(|p| p.supports(Capability::Ohlc))
        .map(|p| p.name().to_string())
}

// ── Worker loop ───────────────────────────────────────────────────────────────

/// Process one claimed backfill chunk to completion (or failure).
///
/// Returns the max candle timestamp on success (for cursor advance), or an error string.
async fn process_chunk(
    pool: &PgPool,
    chain: &[Arc<dyn Provider>],
    chunk: &ClaimedChunk,
) -> Result<Option<DateTime<Utc>>, String> {
    // Resolve market context.
    type MarketRow = (i64, Option<String>, String, String, Option<String>);
    let row: Option<MarketRow> =
        sqlx::query_as("SELECT id, coin_id, base, quote, venue FROM tracked_markets WHERE id = $1")
            .bind(chunk.market_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| e.to_string())?;

    let (_, coin_id, base, quote, venue) =
        row.ok_or_else(|| format!("market {} not found", chunk.market_id))?;

    let mq = MarketQuery {
        market_id: chunk.market_id,
        coin_id,
        base,
        quote: quote.clone(),
        venue,
        vs_currency: quote.to_lowercase(),
    };

    // Acquire pacer slot OUTSIDE any transaction (REQ-SCHED-041).
    let provider_name =
        first_ohlc_provider(chain).ok_or_else(|| "no provider supports OHLC".to_string())?;

    acquire_slot(pool, &provider_name)
        .await
        .map_err(|e| format!("pacer: {e}"))?;

    // Compute resume start and effective days (REQ-SCHED-024/025).
    let start = resume_start(chunk.cursor, chunk.range_start);
    let days = range_to_days(start, chunk.range_end, 90); // 90-day cap per CoinGecko API

    let candles = chain_fetch_ohlc_for_chunk(chain, &mq, days)
        .await
        .map_err(|e| e.to_string())?;

    if candles.is_empty() {
        return Ok(None);
    }

    // Filter to range (provider may return slightly outside bounds).
    let filtered: Vec<OhlcCandle> = match (start, chunk.range_end) {
        (Some(s), Some(e)) => candles
            .into_iter()
            .filter(|c| c.ts >= s && c.ts < e)
            .collect(),
        (Some(s), None) => candles.into_iter().filter(|c| c.ts >= s).collect(),
        (None, Some(e)) => candles.into_iter().filter(|c| c.ts < e).collect(),
        (None, None) => candles,
    };

    // Idempotent upsert (REQ-SCHED-040).
    upsert_candles(pool, &filtered)
        .await
        .map_err(|e| e.to_string())?;

    // Return the max timestamp for cursor advancement.
    let max_ts = filtered.iter().map(|c| c.ts).max();
    Ok(max_ts)
}

/// Run the backfill worker loop (REQ-SCHED-020/050/051).
#[allow(clippy::too_many_arguments)]
pub async fn run_backfill_worker(
    pool: PgPool,
    chain: Arc<Vec<Arc<dyn Provider>>>,
    claimed_by: String,
    lease_secs: i64,
    heartbeat_interval_secs: u64,
    max_attempts: i32,
    idle_sleep: StdDuration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    info!("backfill_worker: started (replica={claimed_by})");

    loop {
        if *shutdown.borrow() {
            break;
        }

        let chunk = match claim_backfill_chunk(&pool, &claimed_by, lease_secs).await {
            Ok(Some(c)) => c,
            Ok(None) => {
                tokio::select! {
                    _ = shutdown.changed() => {}
                    _ = tokio::time::sleep(idle_sleep) => {}
                }
                continue;
            }
            Err(e) => {
                error!("backfill_worker: claim error: {e}");
                tokio::time::sleep(StdDuration::from_secs(1)).await;
                continue;
            }
        };

        let chunk_id = chunk.id;
        let claimed_by_clone = claimed_by.clone();
        let pool_hb = pool.clone();
        let lease_clone = lease_secs;

        // Heartbeat task (REQ-SCHED-022).
        let hb_handle = tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(StdDuration::from_secs(heartbeat_interval_secs));
            loop {
                interval.tick().await;
                match heartbeat_backfill_chunk(&pool_hb, chunk_id, &claimed_by_clone, lease_clone)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        warn!("backfill_worker: heartbeat fencing fired for chunk {chunk_id}");
                        break;
                    }
                    Err(e) => {
                        error!("backfill_worker: heartbeat error for chunk {chunk_id}: {e}");
                    }
                }
            }
        });

        let result = process_chunk(&pool, &chain, &chunk).await;
        hb_handle.abort();

        match result {
            Ok(Some(max_ts)) => {
                // Advance cursor (REQ-SCHED-024).
                if let Err(e) = advance_cursor(&pool, chunk.id, &claimed_by, max_ts).await {
                    error!(
                        "backfill_worker: cursor advance error for chunk {}: {e}",
                        chunk.id
                    );
                }

                // Mark done if range is exhausted (REQ-SCHED-026).
                let exhausted = chunk.range_end.is_none_or(|end| max_ts >= end);
                if exhausted {
                    if let Err(e) = complete_backfill_chunk(&pool, chunk.id, &claimed_by).await {
                        error!(
                            "backfill_worker: complete error for chunk {}: {e}",
                            chunk.id
                        );
                    }
                    info!("backfill_worker: chunk {} done", chunk.id);
                } else {
                    // More data in range: release for next cycle (re-claim will resume from cursor).
                    if let Err(e) = fail_or_retry_backfill_chunk(
                        &pool,
                        chunk.id,
                        &claimed_by,
                        i32::MAX,
                        "partial",
                    )
                    .await
                    {
                        error!(
                            "backfill_worker: partial-release error for chunk {}: {e}",
                            chunk.id
                        );
                    }
                }
            }
            Ok(None) => {
                // No candles returned (empty range or provider returned nothing).
                // Treat as done — the chunk's range simply has no data.
                if let Err(e) = complete_backfill_chunk(&pool, chunk.id, &claimed_by).await {
                    error!(
                        "backfill_worker: complete (empty) error for chunk {}: {e}",
                        chunk.id
                    );
                }
                info!("backfill_worker: chunk {} done (empty range)", chunk.id);
            }
            Err(e) => {
                warn!(
                    "backfill_worker: chunk {} failed (attempts={}/{}): {e}",
                    chunk.id, chunk.attempts, max_attempts
                );
                if let Err(db_err) =
                    fail_or_retry_backfill_chunk(&pool, chunk.id, &claimed_by, max_attempts, &e)
                        .await
                {
                    error!(
                        "backfill_worker: fail update error for chunk {}: {db_err}",
                        chunk.id
                    );
                }
            }
        }
    }

    info!("backfill_worker: stopped");
    Ok(())
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(y: i32, mo: u32, d: u32, h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, m, 0).unwrap()
    }

    // ── REQ-SCHED-024/025: resume_start ──────────────────────────────────────

    #[test]
    fn resume_start_uses_cursor_plus_nanosecond() {
        let cursor = ts(2026, 1, 10, 0, 0);
        let range_start = ts(2026, 1, 1, 0, 0);
        let result = resume_start(Some(cursor), Some(range_start));
        // Must be cursor + 1ns (strictly after cursor)
        assert!(result.is_some());
        let r = result.unwrap();
        assert!(r > cursor, "resume must be strictly after cursor");
        assert!(
            r < cursor + chrono::Duration::seconds(1),
            "resume must be just after cursor"
        );
    }

    #[test]
    fn resume_start_falls_back_to_range_start_when_no_cursor() {
        let range_start = ts(2026, 1, 1, 0, 0);
        let result = resume_start(None, Some(range_start));
        assert_eq!(result, Some(range_start));
    }

    #[test]
    fn resume_start_returns_none_when_both_null() {
        let result = resume_start(None, None);
        assert!(
            result.is_none(),
            "whole-dataset chunk: resume_start must be None"
        );
    }

    #[test]
    fn resume_start_cursor_wins_over_range_start() {
        let cursor = ts(2026, 1, 15, 0, 0);
        let range_start = ts(2026, 1, 1, 0, 0);
        let result = resume_start(Some(cursor), Some(range_start));
        // Must be based on cursor, not range_start
        assert!(
            result.unwrap() > range_start,
            "cursor must win over range_start"
        );
    }

    // ── range_to_days ─────────────────────────────────────────────────────────

    #[test]
    fn range_to_days_computes_from_window() {
        let start = ts(2026, 1, 1, 0, 0);
        let end = ts(2026, 1, 8, 0, 0); // 7 days
        let days = range_to_days(Some(start), Some(end), 90);
        assert_eq!(days, 7);
    }

    #[test]
    fn range_to_days_clamps_to_max() {
        let start = ts(2026, 1, 1, 0, 0);
        let end = ts(2026, 12, 31, 0, 0); // 364 days
        let days = range_to_days(Some(start), Some(end), 90);
        assert_eq!(days, 90, "range exceeding max must be clamped");
    }

    #[test]
    fn range_to_days_returns_max_when_no_bounds() {
        let days = range_to_days(None, None, 30);
        assert_eq!(days, 30);
    }

    #[test]
    fn range_to_days_minimum_is_one() {
        // Same start and end would be 0 days; must clamp to 1.
        let t = ts(2026, 1, 1, 0, 0);
        let days = range_to_days(Some(t), Some(t), 90);
        assert_eq!(days, 1);
    }

    // ── SQL-shape assertions ──────────────────────────────────────────────────

    #[test]
    fn claim_backfill_sql_uses_skip_locked() {
        assert!(
            CLAIM_BACKFILL_SQL.contains("FOR UPDATE SKIP LOCKED"),
            "claim SQL must use FOR UPDATE SKIP LOCKED (REQ-SCHED-021)"
        );
    }

    #[test]
    fn claim_backfill_sql_includes_lease_expired_predicate() {
        assert!(
            CLAIM_BACKFILL_SQL.contains("lease_expires_at < now()"),
            "claim SQL must include lease-expired predicate (REQ-SCHED-022)"
        );
    }

    #[test]
    fn claim_backfill_sql_orders_oldest_first() {
        assert!(
            CLAIM_BACKFILL_SQL.contains("ORDER BY created_at"),
            "claim SQL must order by created_at for oldest-first claiming"
        );
    }

    #[test]
    fn claim_backfill_sql_increments_attempts() {
        assert!(
            CLAIM_BACKFILL_SQL.contains("attempts + 1"),
            "claim SQL must increment attempts at claim time"
        );
    }

    #[test]
    fn claim_backfill_sql_limits_one() {
        assert!(
            CLAIM_BACKFILL_SQL.contains("LIMIT 1"),
            "claim SQL must LIMIT 1"
        );
    }

    #[test]
    fn heartbeat_backfill_sql_uses_fencing_guard() {
        assert!(
            HEARTBEAT_BACKFILL_SQL.contains("AND claimed_by = $3"),
            "heartbeat SQL must use claimed_by fencing guard"
        );
    }

    #[test]
    fn advance_cursor_sql_uses_fencing_guard() {
        assert!(
            ADVANCE_CURSOR_SQL.contains("AND claimed_by = $2"),
            "cursor advance SQL must use claimed_by fencing guard (REQ-SCHED-024)"
        );
    }

    #[test]
    fn fail_or_retry_sql_uses_conditional_status() {
        assert!(
            FAIL_OR_RETRY_BACKFILL_SQL
                .contains("CASE WHEN attempts >= $3 THEN 'failed' ELSE 'pending' END"),
            "fail-or-retry SQL must conditionally mark failed vs pending (REQ-SCHED-027)"
        );
    }

    #[test]
    fn enqueue_job_sql_uses_on_conflict_do_nothing() {
        assert!(
            ENQUEUE_BACKFILL_JOB_SQL.contains("ON CONFLICT (market_id, dataset) DO NOTHING"),
            "enqueue job SQL must be idempotent via ON CONFLICT (REQ-SCHED-028)"
        );
    }

    // ── DB-gated integration tests ─────────────────────────────────────────────

    /// REQ-SCHED-021/022/024/026: claim → cursor advance → complete cycle.
    #[tokio::test]
    #[ignore]
    async fn db_claim_advance_cursor_complete_cycle() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
        let pool = crate::db::connect(&url).await.expect("connect");

        // Need a market and a job first.
        let market_id: i64 = sqlx::query_scalar(
            "INSERT INTO tracked_markets (base, quote, status) VALUES ('BTC', 'USD', 'active') \
             ON CONFLICT (base, quote) DO UPDATE SET status='active' RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("upsert market");

        let job_id: i64 = sqlx::query_scalar(
            "INSERT INTO backfill_jobs (market_id, dataset, status, requested_at, updated_at) \
             VALUES ($1, 'ohlc_1d', 'pending', now(), now()) \
             ON CONFLICT (market_id, dataset) DO UPDATE SET status='pending' \
             RETURNING id",
        )
        .bind(market_id)
        .fetch_one(&pool)
        .await
        .expect("upsert job");

        // Insert a chunk with a known range.
        let range_start = chrono::Utc::now() - chrono::Duration::days(10);
        let range_end = chrono::Utc::now();
        let chunk_id: i64 = sqlx::query_scalar(
            "INSERT INTO backfill_chunks \
             (job_id, market_id, dataset, interval, range_start, range_end, status, created_at, updated_at) \
             VALUES ($1, $2, 'ohlc_1d', '1d', $3, $4, 'pending', now(), now()) RETURNING id",
        )
        .bind(job_id)
        .bind(market_id)
        .bind(range_start)
        .bind(range_end)
        .fetch_one(&pool)
        .await
        .expect("insert chunk");

        // Claim.
        let chunk = claim_backfill_chunk(&pool, "test-replica-1", 300)
            .await
            .expect("claim")
            .expect("should find chunk");
        assert_eq!(chunk.id, chunk_id);
        assert_eq!(chunk.attempts, 1);

        // Advance cursor.
        let cursor_ts = range_end - chrono::Duration::days(3);
        advance_cursor(&pool, chunk.id, "test-replica-1", cursor_ts)
            .await
            .expect("advance cursor");

        // Verify cursor was persisted.
        let saved_cursor: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT cursor FROM backfill_chunks WHERE id = $1")
                .bind(chunk.id)
                .fetch_one(&pool)
                .await
                .expect("fetch cursor");
        assert!(saved_cursor.is_some(), "cursor must be persisted");

        // Complete.
        complete_backfill_chunk(&pool, chunk.id, "test-replica-1")
            .await
            .expect("complete");

        let status: String = sqlx::query_scalar("SELECT status FROM backfill_chunks WHERE id = $1")
            .bind(chunk.id)
            .fetch_one(&pool)
            .await
            .expect("fetch status");
        assert_eq!(status, "done");

        // Cleanup.
        sqlx::query("DELETE FROM backfill_chunks WHERE id = $1")
            .bind(chunk.id)
            .execute(&pool)
            .await
            .expect("cleanup chunk");
        sqlx::query("DELETE FROM backfill_jobs WHERE id = $1")
            .bind(job_id)
            .execute(&pool)
            .await
            .expect("cleanup job");
    }
}
