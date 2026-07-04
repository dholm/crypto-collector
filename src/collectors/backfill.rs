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

use crate::db::upserts::upsert_coin_candle;
use crate::pacer::acquire_slot;
use crate::providers::{Capability, MarketQuery, OhlcCandle, Provider, ProviderError};

/// Dataset tag used for the startup once-per-coin historical backfill job
/// (`enqueue_startup_backfills`). Matches the `ON CONFLICT (coin_id, dataset)`
/// idempotency key on `backfill_jobs`.
pub const STARTUP_BACKFILL_DATASET: &str = "candles";

/// Empty-page forward-skip step size, expressed as a candle count and multiplied by
/// the chunk's `interval_secs` to get the skip span (REQ-SCHED-024/025/026,
/// see [`next_cursor_for_page`]). 1000 matches Binance's OHLC page cap — the largest
/// page size among supported providers — so a single skip conservatively covers what
/// one provider page could have returned.
const EMPTY_PAGE_SKIP_CANDLES: i64 = 1000;

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

/// Decide whether `process_chunk` should use the date-range-bounded fetch
/// (`chain_fetch_ohlc_range`) instead of the `days`-based recent-window fetch
/// (`chain_fetch_ohlc`). Range path requires both bounds to be known; chunks
/// missing either (e.g. legacy whole-dataset chunks with `range_start`/`range_end`
/// both `NULL`) fall back to the `days`-based path.
pub fn should_use_range_path(
    start: Option<DateTime<Utc>>,
    range_end: Option<DateTime<Utc>>,
) -> bool {
    start.is_some() && range_end.is_some()
}

/// Compute the next durable cursor and completion decision for one processed
/// backfill chunk page (REQ-SCHED-024/025/026).
///
/// - Non-empty page (`max_ts` is `Some`): advance the cursor to `max_ts`; the chunk
///   is done iff `max_ts >= range_end` (or `range_end` is `None`, e.g. legacy
///   whole-dataset chunks). Unchanged from prior behavior.
/// - Empty page (`max_ts` is `None`) **on the range path** (`resume_start` and
///   `range_end` both known — mirrors [`should_use_range_path`]): a single empty or
///   fully-filtered page must NOT end a multi-year backfill (a data gap, provider
///   hiccup, or an out-of-window page is not "no more data"). Instead advance the
///   cursor forward by `page_span_secs`, capped at `range_end`, so the walk makes
///   guaranteed forward progress and terminates once the advanced cursor reaches
///   `range_end`.
/// - Empty page off the range path (either bound unknown, e.g. legacy whole-dataset
///   chunks): unchanged — complete with no cursor advance.
///
// @MX:NOTE: [AUTO] next_cursor_for_page — empty-page-forward-skip invariant
//   A single empty RANGE-path page must advance the cursor by page_span_secs
//   (capped at range_end) and stay pending, never silently completing the chunk.
//   This guarantees termination (cursor is strictly monotonic and bounded by
//   range_end) while preventing gaps/hiccups from truncating a historical backfill.
// @MX:SPEC: SPEC-SCHED-001 REQ-SCHED-024 REQ-SCHED-025 REQ-SCHED-026
pub fn next_cursor_for_page(
    resume_start: Option<DateTime<Utc>>,
    range_end: Option<DateTime<Utc>>,
    max_ts: Option<DateTime<Utc>>,
    page_span_secs: i64,
) -> (Option<DateTime<Utc>>, bool) {
    match max_ts {
        Some(ts) => {
            let done = range_end.is_none_or(|end| ts >= end);
            (Some(ts), done)
        }
        None => match (resume_start, range_end) {
            (Some(start), Some(end)) => {
                let advanced = (start + chrono::Duration::seconds(page_span_secs.max(1))).min(end);
                let done = advanced >= end;
                (Some(advanced), done)
            }
            // Legacy / whole-dataset path: unchanged — complete, no cursor advance.
            _ => (None, true),
        },
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
    RETURNING id, job_id, coin_id, dataset, interval, \
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
        (coin_id, dataset, status, requested_at, updated_at) \
    VALUES ($1, $2, 'pending', now(), now()) \
    ON CONFLICT (coin_id, dataset) DO NOTHING \
    RETURNING id";

/// Insert one chunk for a newly created job.
pub const INSERT_BACKFILL_CHUNK_SQL: &str = "\
    INSERT INTO backfill_chunks \
        (job_id, coin_id, dataset, interval, range_start, range_end, \
         status, created_at, updated_at) \
    VALUES ($1, $2, $3, $4, $5, $6, 'pending', now(), now())";

// ── Structs ───────────────────────────────────────────────────────────────────

/// A successfully claimed `backfill_chunks` row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ClaimedChunk {
    pub id: i64,
    pub job_id: i64,
    pub coin_id: String,
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
    coin_id: &str,
    dataset: &str,
    interval: Option<&str>,
    range_start: Option<DateTime<Utc>>,
    range_end: Option<DateTime<Utc>>,
) -> Result<bool, sqlx::Error> {
    let job_id: Option<i64> = sqlx::query_scalar(ENQUEUE_BACKFILL_JOB_SQL)
        .bind(coin_id)
        .bind(dataset)
        .fetch_optional(pool)
        .await?;

    let Some(job_id) = job_id else {
        return Ok(false); // already exists
    };

    sqlx::query(INSERT_BACKFILL_CHUNK_SQL)
        .bind(job_id)
        .bind(coin_id)
        .bind(dataset)
        .bind(interval)
        .bind(range_start)
        .bind(range_end)
        .execute(pool)
        .await?;

    Ok(true)
}

/// Enqueue a historical candle backfill job for every currently tracked coin, once
/// per coin, idempotently (startup hook — see `main.rs`).
///
/// Reuses `enqueue_backfill_job`'s `ON CONFLICT (coin_id, dataset) DO NOTHING`
/// idempotency key (dataset = `STARTUP_BACKFILL_DATASET` = `"candles"`), so re-deploys
/// never duplicate or restart a backfill that has already been enqueued (completed or
/// still in progress) — only coins with no existing `candles` job get a new one.
///
/// `lookback_days` sets `range_start = now - lookback_days`; `range_end = now`.
/// Returns `(enqueued, skipped)` counts. Does not fail the caller's startup sequence —
/// callers should log a warning and continue on `Err` (see `main.rs`).
///
// @MX:ANCHOR: [AUTO] enqueue_startup_backfills — once-per-coin idempotent historical backfill trigger
// @MX:REASON: fan_in >= 3: main.rs startup hook, DB integration tests, future re-trigger callers.
//             Idempotency invariant: ON CONFLICT (coin_id, dataset) DO NOTHING means re-deploys
//             never duplicate or restart a backfill already enqueued for a coin.
pub async fn enqueue_startup_backfills(
    pool: &PgPool,
    lookback_days: u32,
) -> Result<(u64, u64), sqlx::Error> {
    let coin_ids: Vec<String> = sqlx::query_scalar("SELECT coin_id FROM tracked_coins")
        .fetch_all(pool)
        .await?;

    let range_end = Utc::now();
    let range_start = range_end - chrono::Duration::days(lookback_days as i64);

    let mut enqueued = 0u64;
    let mut skipped = 0u64;

    for coin_id in &coin_ids {
        let created = enqueue_backfill_job(
            pool,
            coin_id,
            STARTUP_BACKFILL_DATASET,
            None,
            Some(range_start),
            Some(range_end),
        )
        .await?;

        if created {
            enqueued += 1;
        } else {
            skipped += 1;
        }
    }

    Ok((enqueued, skipped))
}

// ── Chain dispatch helper ─────────────────────────────────────────────────────

async fn chain_fetch_ohlc_for_chunk(
    chain: &[Arc<dyn Provider>],
    market: &MarketQuery,
    days: u32,
    interval_secs: i64,
) -> Result<Vec<OhlcCandle>, ProviderError> {
    let (result, _) = crate::providers::chain_fetch_ohlc(chain, market, days, interval_secs).await;
    result
}

/// Date-range-bounded counterpart of `chain_fetch_ohlc_for_chunk` (see
/// `providers::chain_fetch_ohlc_range`), used when both `start` and `range_end` are
/// known so the worker can fetch an arbitrary historical window rather than a
/// "most recent N days" window.
async fn chain_fetch_ohlc_range_for_chunk(
    chain: &[Arc<dyn Provider>],
    market: &MarketQuery,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    interval_secs: i64,
) -> Result<Vec<OhlcCandle>, ProviderError> {
    let (result, _) =
        crate::providers::chain_fetch_ohlc_range(chain, market, start, end, interval_secs).await;
    result
}

fn first_ohlc_provider(chain: &[Arc<dyn Provider>]) -> Option<String> {
    chain
        .iter()
        .find(|p| p.supports(Capability::Ohlc))
        .map(|p| p.name().to_string())
}

/// First provider supporting `OhlcRange`, falling back to the first `Ohlc`-supporting
/// provider when none declare range support (REQ backfill pacer-slot keying).
fn first_range_provider(chain: &[Arc<dyn Provider>]) -> Option<String> {
    chain
        .iter()
        .find(|p| p.supports(Capability::OhlcRange))
        .map(|p| p.name().to_string())
        .or_else(|| first_ohlc_provider(chain))
}

// ── Worker loop ───────────────────────────────────────────────────────────────

/// Process one claimed backfill chunk to completion (or failure).
///
/// Returns `(max_ts, interval_secs)` on success: `max_ts` is the max candle timestamp
/// persisted this page (`None` when the page was empty); `interval_secs` is the
/// candle granularity used, which the caller needs to compute the empty-page
/// forward-skip span (see [`next_cursor_for_page`]).
async fn process_chunk(
    pool: &PgPool,
    chain: &[Arc<dyn Provider>],
    chunk: &ClaimedChunk,
) -> Result<(Option<DateTime<Utc>>, i64), String> {
    // Look up coin's trading symbol and per-coin poll interval from tracked_coins.
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT symbol, live_poll_interval::TEXT FROM tracked_coins WHERE coin_id = $1",
    )
    .bind(&chunk.coin_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?;
    let (symbol, live_poll_interval) =
        row.ok_or_else(|| format!("coin {} not found", chunk.coin_id))?;

    let mq = MarketQuery {
        market_id: 0,
        coin_id: Some(chunk.coin_id.clone()),
        base: symbol,
        quote: "USDT".to_string(),
        venue: None,
        vs_currency: "usd".to_string(),
    };

    // Compute resume start (REQ-SCHED-024/025).
    let start = resume_start(chunk.cursor, chunk.range_start);

    // Candle granularity = per-coin poll interval (or global default).
    let global_interval = crate::config::live_quote_poll_interval_secs();
    let interval_secs = crate::config::effective_candle_interval_secs(
        live_poll_interval.as_deref(),
        global_interval,
    );

    // When both bounds of the chunk's window are known, use the date-range-bounded
    // fetch so a multi-year backfill can actually reach back that far — the
    // `days`-based `fetch_ohlc` path only ever windows relative to "now" and cannot
    // target an arbitrary historical range. Chunks lacking one or both bounds (e.g.
    // legacy whole-dataset chunks) keep using the `days`-based path.
    let use_range_path = should_use_range_path(start, chunk.range_end);

    // Acquire pacer slot OUTSIDE any transaction (REQ-SCHED-041). Key on the first
    // range-capable provider when taking the range path, else the first OHLC provider.
    let provider_name = if use_range_path {
        first_range_provider(chain)
    } else {
        first_ohlc_provider(chain)
    }
    .ok_or_else(|| "no provider supports OHLC".to_string())?;

    acquire_slot(pool, &provider_name)
        .await
        .map_err(|e| format!("pacer: {e}"))?;

    let candles = if use_range_path {
        let range_start = start.expect("checked by use_range_path");
        let range_end = chunk.range_end.expect("checked by use_range_path");
        chain_fetch_ohlc_range_for_chunk(chain, &mq, range_start, range_end, interval_secs)
            .await
            .map_err(|e| e.to_string())?
    } else {
        let days = range_to_days(start, chunk.range_end, 90); // fallback: recent-window path
        chain_fetch_ohlc_for_chunk(chain, &mq, days, interval_secs)
            .await
            .map_err(|e| e.to_string())?
    };

    if candles.is_empty() {
        return Ok((None, interval_secs));
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

    // Idempotent upsert into coin_candles (REQ-SCHED-040).
    for c in &filtered {
        let coin_candle = crate::models::quote::CoinCandle {
            coin_id: chunk.coin_id.clone(),
            vs_currency: c.vs_currency.clone(),
            interval: c.interval.clone(),
            ts: c.ts,
            open: c.open,
            high: c.high,
            low: c.low,
            close: c.close,
            volume: c.volume,
            source: c.source.clone(),
        };
        upsert_coin_candle(pool, &coin_candle)
            .await
            .map_err(|e| e.to_string())?;
    }

    // Return the max timestamp for cursor advancement.
    let max_ts = filtered.iter().map(|c| c.ts).max();
    Ok((max_ts, interval_secs))
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
            Ok((max_ts, interval_secs)) => {
                // Empty-page forward-skip span: a fixed step tied to the candle
                // interval and the largest provider page cap (Binance: 1000 candles
                // per page) guarantees forward progress and termination even when a
                // single page is empty or fully filtered out of range (REQ-SCHED-024/025/026).
                let page_span_secs = interval_secs.max(1) * EMPTY_PAGE_SKIP_CANDLES;

                let start = resume_start(chunk.cursor, chunk.range_start);
                let (next_cursor, done) =
                    next_cursor_for_page(start, chunk.range_end, max_ts, page_span_secs);

                if let Some(cursor) = next_cursor {
                    // Advance cursor (REQ-SCHED-024). Also covers the empty-page
                    // forward-skip: the computed cursor still durably records progress.
                    if let Err(e) = advance_cursor(&pool, chunk.id, &claimed_by, cursor).await {
                        error!(
                            "backfill_worker: cursor advance error for chunk {}: {e}",
                            chunk.id
                        );
                    }
                }

                if done {
                    if let Err(e) = complete_backfill_chunk(&pool, chunk.id, &claimed_by).await {
                        error!(
                            "backfill_worker: complete error for chunk {}: {e}",
                            chunk.id
                        );
                    }
                    info!("backfill_worker: chunk {} done", chunk.id);
                } else {
                    // More data in range (or an empty page was skipped forward):
                    // release for next cycle. Uses i32::MAX for max_attempts so a
                    // partial release / forward-skip is never counted as a failure
                    // toward the chunk's retry/fail limit (matches the non-empty
                    // partial-release path below).
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

    // ── next_cursor_for_page: empty-page-forward-skip invariant ──────────────

    #[test]
    fn next_cursor_empty_page_advances_forward_when_below_range_end() {
        let start = ts(2016, 1, 1, 0, 0);
        let end = ts(2026, 1, 1, 0, 0); // far in the future — well beyond one skip
        let page_span_secs = 60 * 1000; // 1m candles, 1000-candle page
        let (next, done) = next_cursor_for_page(Some(start), Some(end), None, page_span_secs);
        assert_eq!(
            next,
            Some(start + chrono::Duration::seconds(page_span_secs))
        );
        assert!(!done, "must not complete on a mid-range empty page");
    }

    #[test]
    fn next_cursor_empty_page_completes_when_advanced_cursor_reaches_range_end() {
        let start = ts(2026, 1, 1, 0, 0);
        let end = ts(2026, 1, 1, 0, 10); // 10 minutes away — closer than one full skip
        let page_span_secs = 60 * 1000; // would overshoot range_end
        let (next, done) = next_cursor_for_page(Some(start), Some(end), None, page_span_secs);
        assert_eq!(next, Some(end), "advance must be capped at range_end");
        assert!(
            done,
            "must complete once the advanced cursor reaches range_end"
        );
    }

    #[test]
    fn next_cursor_nonempty_page_partial_when_below_range_end() {
        let start = ts(2026, 1, 1, 0, 0);
        let end = ts(2026, 1, 8, 0, 0);
        let max_ts = ts(2026, 1, 3, 0, 0); // below range_end
        let (next, done) = next_cursor_for_page(Some(start), Some(end), Some(max_ts), 60_000);
        assert_eq!(next, Some(max_ts), "cursor advances to max_ts, unchanged");
        assert!(!done, "must partial-release when max_ts < range_end");
    }

    #[test]
    fn next_cursor_nonempty_page_done_when_at_or_past_range_end() {
        let start = ts(2026, 1, 1, 0, 0);
        let end = ts(2026, 1, 8, 0, 0);
        let max_ts = ts(2026, 1, 8, 0, 0); // == range_end
        let (next, done) = next_cursor_for_page(Some(start), Some(end), Some(max_ts), 60_000);
        assert_eq!(next, Some(max_ts));
        assert!(done, "must complete when max_ts >= range_end");
    }

    #[test]
    fn next_cursor_legacy_empty_page_completes_without_cursor_advance() {
        // Legacy whole-dataset chunk: neither bound known — behavior unchanged.
        let (next, done) = next_cursor_for_page(None, None, None, 60_000);
        assert_eq!(next, None, "legacy empty path must not synthesize a cursor");
        assert!(done, "legacy empty path completes immediately, as before");
    }

    #[test]
    fn next_cursor_legacy_empty_page_missing_range_end_completes_unchanged() {
        // Only resume_start known (range_end missing) — not on the range path.
        let start = ts(2026, 1, 1, 0, 0);
        let (next, done) = next_cursor_for_page(Some(start), None, None, 60_000);
        assert_eq!(next, None);
        assert!(done);
    }

    // ── should_use_range_path: worker range-path selection (pure logic) ───────

    #[test]
    fn should_use_range_path_true_when_both_bounds_known() {
        let start = ts(2026, 1, 1, 0, 0);
        let end = ts(2026, 1, 8, 0, 0);
        assert!(should_use_range_path(Some(start), Some(end)));
    }

    #[test]
    fn should_use_range_path_false_when_start_missing() {
        let end = ts(2026, 1, 8, 0, 0);
        assert!(!should_use_range_path(None, Some(end)));
    }

    #[test]
    fn should_use_range_path_false_when_end_missing() {
        let start = ts(2026, 1, 1, 0, 0);
        assert!(!should_use_range_path(Some(start), None));
    }

    #[test]
    fn should_use_range_path_false_when_both_missing() {
        assert!(!should_use_range_path(None, None));
    }

    // ── first_range_provider: prefers OhlcRange, falls back to Ohlc ──────────

    struct StubProvider {
        provider_name: &'static str,
        caps: &'static [Capability],
    }

    #[async_trait::async_trait]
    impl Provider for StubProvider {
        fn name(&self) -> &str {
            self.provider_name
        }
        fn supports(&self, cap: Capability) -> bool {
            self.caps.contains(&cap)
        }
        async fn fetch_spot(
            &self,
            _m: &MarketQuery,
        ) -> Result<crate::providers::SpotQuote, ProviderError> {
            Err(ProviderError::NotSupported(Capability::Spot))
        }
        async fn fetch_ohlc(
            &self,
            _m: &MarketQuery,
            _d: u32,
            _i: i64,
        ) -> Result<Vec<OhlcCandle>, ProviderError> {
            Ok(vec![])
        }
        async fn fetch_coin_metadata(
            &self,
            _id: &str,
        ) -> Result<crate::providers::CoinMeta, ProviderError> {
            Err(ProviderError::NotSupported(Capability::CoinMetadata))
        }
        async fn fetch_coin_market(
            &self,
            _id: &str,
            _vs: &str,
        ) -> Result<crate::providers::CoinMarket, ProviderError> {
            Err(ProviderError::NotSupported(Capability::CoinMarket))
        }
        async fn fetch_derivatives(
            &self,
            _m: &MarketQuery,
        ) -> Result<crate::providers::DerivTick, ProviderError> {
            Err(ProviderError::NotSupported(Capability::Derivatives))
        }
        async fn search_coins(
            &self,
            _q: &str,
            _cap: usize,
        ) -> Result<Vec<crate::providers::CoinSearchResult>, ProviderError> {
            Ok(vec![])
        }
        async fn fetch_coin_tickers(
            &self,
            _coin_id: &str,
            _cap: usize,
        ) -> Result<Vec<crate::providers::MarketSearchResult>, ProviderError> {
            Ok(vec![])
        }
    }

    #[test]
    fn first_range_provider_prefers_range_capable() {
        let chain: Vec<Arc<dyn Provider>> = vec![
            Arc::new(StubProvider {
                provider_name: "coingecko",
                caps: &[Capability::Ohlc],
            }),
            Arc::new(StubProvider {
                provider_name: "binance",
                caps: &[Capability::Ohlc, Capability::OhlcRange],
            }),
        ];
        assert_eq!(first_range_provider(&chain).as_deref(), Some("binance"));
    }

    #[test]
    fn first_range_provider_falls_back_to_ohlc_when_none_support_range() {
        let chain: Vec<Arc<dyn Provider>> = vec![Arc::new(StubProvider {
            provider_name: "coingecko",
            caps: &[Capability::Ohlc],
        })];
        assert_eq!(first_range_provider(&chain).as_deref(), Some("coingecko"));
    }

    #[test]
    fn first_range_provider_none_when_chain_empty() {
        let chain: Vec<Arc<dyn Provider>> = vec![];
        assert_eq!(first_range_provider(&chain), None);
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
            ENQUEUE_BACKFILL_JOB_SQL.contains("ON CONFLICT (coin_id, dataset) DO NOTHING"),
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

        // bitcoin must exist in tracked_coins (seeded by migrations or prior test data).
        let job_id: i64 = sqlx::query_scalar(
            "INSERT INTO backfill_jobs (coin_id, dataset, status, requested_at, updated_at) \
             VALUES ('bitcoin', 'ohlc_1d', 'pending', now(), now()) \
             ON CONFLICT (coin_id, dataset) DO UPDATE SET status='pending' \
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("upsert job");

        // Insert a chunk with a known range.
        let range_start = chrono::Utc::now() - chrono::Duration::days(10);
        let range_end = chrono::Utc::now();
        let chunk_id: i64 = sqlx::query_scalar(
            "INSERT INTO backfill_chunks \
             (job_id, coin_id, dataset, interval, range_start, range_end, status, created_at, updated_at) \
             VALUES ($1, 'bitcoin', 'ohlc_1d', '1d', $2, $3, 'pending', now(), now()) RETURNING id",
        )
        .bind(job_id)
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

    /// `enqueue_startup_backfills` idempotency: re-invocation for the same coin must
    /// skip rather than duplicate/restart (ON CONFLICT DO NOTHING invariant).
    #[tokio::test]
    #[ignore]
    async fn db_enqueue_startup_backfills_is_idempotent() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
        let pool = crate::db::connect(&url).await.expect("connect");

        // Use a throwaway coin_id unlikely to collide with seeded fixtures, and clean
        // up any prior run's leftovers before asserting.
        let coin_id = "test-startup-backfill-coin";
        sqlx::query("DELETE FROM backfill_chunks WHERE coin_id = $1")
            .bind(coin_id)
            .execute(&pool)
            .await
            .expect("pre-cleanup chunks");
        sqlx::query("DELETE FROM backfill_jobs WHERE coin_id = $1")
            .bind(coin_id)
            .execute(&pool)
            .await
            .expect("pre-cleanup jobs");
        sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
            .bind(coin_id)
            .execute(&pool)
            .await
            .expect("pre-cleanup tracked_coins");

        sqlx::query(
            "INSERT INTO tracked_coins (coin_id, symbol, name, status, registered_at) \
             VALUES ($1, 'TSBC', 'Test Startup Backfill Coin', 'active', now())",
        )
        .bind(coin_id)
        .execute(&pool)
        .await
        .expect("insert tracked coin");

        // First call: must enqueue exactly one job for our test coin.
        let (enqueued_1, _skipped_1) = enqueue_startup_backfills(&pool, 3650)
            .await
            .expect("first enqueue_startup_backfills");
        assert!(
            enqueued_1 >= 1,
            "first call must enqueue at least our test coin"
        );

        let job_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM backfill_jobs WHERE coin_id = $1 AND dataset = $2",
        )
        .bind(coin_id)
        .bind(STARTUP_BACKFILL_DATASET)
        .fetch_one(&pool)
        .await
        .expect("count jobs after first call");
        assert_eq!(job_count, 1, "exactly one job must exist after first call");

        // Second call (simulating a re-deploy): must skip, not duplicate/restart.
        let (_enqueued_2, skipped_2) = enqueue_startup_backfills(&pool, 3650)
            .await
            .expect("second enqueue_startup_backfills");
        assert!(
            skipped_2 >= 1,
            "second call must skip our already-enqueued test coin"
        );

        let job_count_after: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM backfill_jobs WHERE coin_id = $1 AND dataset = $2",
        )
        .bind(coin_id)
        .bind(STARTUP_BACKFILL_DATASET)
        .fetch_one(&pool)
        .await
        .expect("count jobs after second call");
        assert_eq!(
            job_count_after, 1,
            "re-invocation must not duplicate the job row"
        );

        // Cleanup.
        sqlx::query("DELETE FROM backfill_chunks WHERE coin_id = $1")
            .bind(coin_id)
            .execute(&pool)
            .await
            .expect("cleanup chunks");
        sqlx::query("DELETE FROM backfill_jobs WHERE coin_id = $1")
            .bind(coin_id)
            .execute(&pool)
            .await
            .expect("cleanup jobs");
        sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
            .bind(coin_id)
            .execute(&pool)
            .await
            .expect("cleanup tracked_coins");
    }
}
