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
//!
//! # Attempt counting & backpressure (SPEC-SCHED-002 REQ-SCHED-060/061/063)
//!
//! `attempts` is incremented at claim time, but only genuine transient failures count
//! toward `max_attempts`. A **non-failure release** — a multi-page partial release or an
//! empty-page forward-skip — routes through `RELEASE_BACKFILL_SQL`, which neutralizes the
//! claim-time increment (`attempts = GREATEST(attempts - 1, 0)`) and writes `last_error =
//! NULL`, so walking a multi-year range never exhausts the retry budget (REQ-SCHED-060.1).
//! **Pacer** `Cooldown`/`CreditExhausted` is backpressure — released without an attempt,
//! then the worker idles (REQ-SCHED-061). A **permanent** failure (coin not found, no
//! provider supports OHLC) routes through `FAIL_PERMANENT_BACKFILL_SQL` and fails on the
//! first attempt (REQ-SCHED-063.2); a **transient** failure retries with backoff.

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tracing::{error, info, warn};

use crate::collectors::collection_queue::pacer_should_skip_queue;
use crate::collectors::lease_worker::{
    run_lease_worker, HeartbeatStep, LeaseCycleOutcome, LeaseFut, LeaseItem,
};
use crate::collectors::retry::DispatchError;
use crate::pacer::acquire_slot;
use crate::providers::{Capability, MarketQuery, OhlcCandle, Provider, ProviderError};

/// Dataset tag used for the startup once-per-coin historical backfill job
/// (`enqueue_startup_backfills`). Matches the `ON CONFLICT (coin_id, dataset)`
/// idempotency key on `backfill_jobs`.
pub const STARTUP_BACKFILL_DATASET: &str = "candles";

/// Dataset tag for the deep-history **daily** backfill job (`enqueue_deep_history_backfills`).
///
/// Distinct from `STARTUP_BACKFILL_DATASET` so the two coexist under the
/// `ON CONFLICT (coin_id, dataset)` idempotency key: the startup job backfills the
/// recent window at the coin's fine interval, while this job backfills the deep pre-2017
/// window at `1d` — the only granularity a source like Bitstamp serves that far back.
pub const DEEP_HISTORY_BACKFILL_DATASET: &str = "candles_deep_1d";

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

/// Resolve the candle granularity (seconds) for a claimed chunk.
///
/// An explicit `chunk_interval` (canonical string, e.g. `"1d"`) takes precedence — this
/// is how a deep-history job pins the daily granularity that deep sources serve. When the
/// chunk leaves it `NULL` (legacy / startup chunks), fall back to the per-coin poll
/// interval, then the global default (`effective_candle_interval_secs`).
pub fn resolve_interval_secs(
    chunk_interval: Option<&str>,
    live_poll_interval: Option<&str>,
    global_secs: i64,
) -> i64 {
    chunk_interval
        .and_then(crate::api::candles_agg::interval_to_seconds)
        .unwrap_or_else(|| {
            crate::config::effective_candle_interval_secs(live_poll_interval, global_secs)
        })
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
/// Increments `attempts` at claim time. Per SPEC-SCHED-002 REQ-SCHED-060 the bound counts
/// only genuine transient failures: a non-failure release (`RELEASE_BACKFILL_SQL`) decrements
/// `attempts` to neutralize this `+1`, so multi-page partial releases / forward-skips (F-01)
/// no longer count pages toward the bound; a crash re-claim keeps its `+1` (crash-loops stay
/// bounded, and the un-indexed backfill reclaim path is unaffected).
///
// @MX:ANCHOR: [AUTO] CLAIM_BACKFILL_SQL — FOR UPDATE SKIP LOCKED; at-most-one-replica per chunk
// @MX:REASON: fan_in >= 3: claim_backfill_chunk(), SQL-shape tests, DB integration tests.
//             REQ-SCHED-022: lease-expired re-claim enables crash recovery without orphaning chunks.
//             REQ-SCHED-027: attempts incremented at claim time for bound retry accounting.
//             SPEC-SCHED-002 REQ-SCHED-060: RELEASE_BACKFILL_SQL neutralizes this +1 on non-failure
//             releases, so the bound counts genuine failures, not pages walked (F-01 root-cause fix).
// @MX:SPEC: SPEC-SCHED-001 REQ-SCHED-021 REQ-SCHED-022 REQ-SCHED-027 SPEC-SCHED-002 REQ-SCHED-060
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

/// Genuine transient-failure UPDATE: reset to `pending` if under max_attempts, else mark
/// `failed` (REQ-SCHED-027). This is the **transient-failure-only** path — non-failure
/// releases use `RELEASE_BACKFILL_SQL` and permanent failures use `FAIL_PERMANENT_BACKFILL_SQL`.
///
// @MX:NOTE: [AUTO] FAIL_OR_RETRY_BACKFILL_SQL — genuine transient-failure-only path (SPEC-SCHED-002 REQ-SCHED-060.3/063.3)
// @MX:SPEC: SPEC-SCHED-001 REQ-SCHED-027 SPEC-SCHED-002 REQ-SCHED-060 REQ-SCHED-063
pub const FAIL_OR_RETRY_BACKFILL_SQL: &str = "\
    UPDATE backfill_chunks SET \
        status           = CASE WHEN attempts >= $3 THEN 'failed' ELSE 'pending' END, \
        last_error       = $4, \
        claimed_by       = NULL, \
        lease_expires_at = NULL, \
        heartbeat_at     = NULL, \
        updated_at       = now() \
    WHERE id = $1 AND claimed_by = $2";

/// Administrative (non-failure) release of a claimed chunk — a multi-page partial release
/// or an empty-page forward-skip (SPEC-SCHED-002 REQ-SCHED-060.1/065.1). Resets to `pending`,
/// writes a NON-error marker (`last_error = NULL`, never `"partial"`), and neutralizes the
/// claim-time `attempts + 1` via `GREATEST(attempts - 1, 0)` so walking pages never consumes
/// the retry budget. Carries the `AND claimed_by = $2` fence like the failure paths.
///
// @MX:WARN: [AUTO] RELEASE_BACKFILL_SQL — administrative release; neutralizes the claim-time attempts+1 and clears last_error
// @MX:REASON: SPEC-SCHED-002 REQ-SCHED-060/065 root-cause guard (F-01). Do NOT "restore" the old
//             FAIL_OR_RETRY reuse with i32::MAX — that made multi-page partial re-claims count pages
//             toward max_attempts and overwrote a prior genuine last_error with "partial". This
//             path must stay attempt-neutral. Also do NOT park chunks in claimed/running via
//             lease_expires_at for cooldown deferral: that collides with the backfill-stalled alarm
//             and the un-indexed reclaim path (see plan.md § Schema Investigation, R3).
// @MX:SPEC: SPEC-SCHED-002 REQ-SCHED-060 REQ-SCHED-065
pub const RELEASE_BACKFILL_SQL: &str = "\
    UPDATE backfill_chunks SET \
        status           = 'pending', \
        last_error       = NULL, \
        attempts         = GREATEST(attempts - 1, 0), \
        claimed_by       = NULL, \
        lease_expires_at = NULL, \
        heartbeat_at     = NULL, \
        updated_at       = now() \
    WHERE id = $1 AND claimed_by = $2";

/// Permanent-failure UPDATE: mark `failed` immediately, regardless of `attempts`
/// (SPEC-SCHED-002 REQ-SCHED-063.2). A permanent failure is terminal and does not consume
/// or rely on the `max_attempts` retry budget. Carries the `AND claimed_by = $2` fence.
///
// @MX:NOTE: [AUTO] FAIL_PERMANENT_BACKFILL_SQL — terminal fail-fast, retry-budget-independent (SPEC-SCHED-002 REQ-SCHED-063.2)
// @MX:SPEC: SPEC-SCHED-002 REQ-SCHED-063
pub const FAIL_PERMANENT_BACKFILL_SQL: &str = "\
    UPDATE backfill_chunks SET \
        status           = 'failed', \
        last_error       = $3, \
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

impl LeaseItem for ClaimedChunk {
    fn lease_id(&self) -> i64 {
        self.id
    }
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

/// Fail or retry a chunk on a genuine transient failure (REQ-SCHED-027). Non-failure
/// releases use [`release_backfill_chunk`]; permanent failures use
/// [`fail_permanent_backfill_chunk`].
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

/// Administratively release a claimed chunk for a non-failure reason (a multi-page partial
/// release or an empty-page forward-skip) without consuming the retry budget
/// (SPEC-SCHED-002 REQ-SCHED-060.1/061/065.1).
pub async fn release_backfill_chunk(
    pool: &PgPool,
    id: i64,
    claimed_by: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(RELEASE_BACKFILL_SQL)
        .bind(id)
        .bind(claimed_by)
        .execute(pool)
        .await?;
    Ok(())
}

/// Mark a chunk permanently `failed` on a permanent dispatch error, immediately and
/// independent of the retry budget (SPEC-SCHED-002 REQ-SCHED-063.2).
pub async fn fail_permanent_backfill_chunk(
    pool: &PgPool,
    id: i64,
    claimed_by: &str,
    error: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(FAIL_PERMANENT_BACKFILL_SQL)
        .bind(id)
        .bind(claimed_by)
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

/// Enqueue a deep-history **daily** backfill job for each of `coin_ids`, once per coin,
/// idempotently (startup hook — see `main.rs`).
///
/// Covers the pre-regular-lookback window `[start, end)` at the `1d` interval, sourced
/// (via the provider chain's continue-on-empty fallthrough) from whichever provider has
/// data that far back — for BTC/USD that is Bitstamp, whose daily candles reach 2011-08.
/// Only coins that are actually tracked get a job (an untracked coin would produce chunks
/// that always fail the `tracked_coins` lookup in `process_chunk`).
///
/// Idempotent via `ON CONFLICT (coin_id, dataset) DO NOTHING` on
/// `DEEP_HISTORY_BACKFILL_DATASET`, so re-deploys never duplicate or restart it. Returns
/// `(enqueued, skipped)`. Does not fail the caller's startup sequence.
///
// @MX:ANCHOR: [AUTO] enqueue_deep_history_backfills — once-per-coin idempotent deep 1d backfill
// @MX:REASON: fan_in >= 2: main.rs startup hook, DB integration tests. Idempotency invariant:
//             ON CONFLICT (coin_id, DEEP_HISTORY_BACKFILL_DATASET) DO NOTHING means re-deploys
//             never duplicate/restart. Interval is pinned to '1d' (the deep-history granularity).
// @MX:SPEC: SPEC-PROV-001 SPEC-SCHED-001
pub async fn enqueue_deep_history_backfills(
    pool: &PgPool,
    coin_ids: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<(u64, u64), sqlx::Error> {
    let mut enqueued = 0u64;
    let mut skipped = 0u64;

    for coin_id in coin_ids {
        // Skip untracked coins — process_chunk requires a tracked_coins row.
        // EXISTS returns a non-null bool (BOOL), avoiding the INT4/INT8 decode mismatch
        // that a bare `SELECT 1` (typed INT4) would cause against a Rust integer.
        let tracked: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tracked_coins WHERE coin_id = $1)")
                .bind(coin_id)
                .fetch_one(pool)
                .await?;
        if !tracked {
            skipped += 1;
            continue;
        }

        let created = enqueue_backfill_job(
            pool,
            coin_id,
            DEEP_HISTORY_BACKFILL_DATASET,
            Some("1d"),
            Some(start),
            Some(end),
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
    registry: Option<&crate::alarm::HealthRegistry>,
) -> Result<Vec<OhlcCandle>, ProviderError> {
    let (result, _) =
        crate::providers::chain_fetch_ohlc(chain, market, days, interval_secs, registry).await;
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
    registry: Option<&crate::alarm::HealthRegistry>,
) -> Result<Vec<OhlcCandle>, ProviderError> {
    let (result, _) = crate::providers::chain_fetch_ohlc_range(
        chain,
        market,
        start,
        end,
        interval_secs,
        registry,
    )
    .await;
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

/// Outcome of processing one backfill chunk page (SPEC-SCHED-002 REQ-SCHED-060/061/063).
///
/// The `Err` channel ([`DispatchError`]) carries genuine failures classified
/// transient-vs-permanent; pacer backpressure is the `SoftSkip` non-failure variant here,
/// so it never touches the retry budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkOutcome {
    /// A page was processed. `max_ts` is the max candle timestamp persisted this page
    /// (`None` when the page was empty); `interval_secs` is the candle granularity used,
    /// needed to compute the empty-page forward-skip span (see [`next_cursor_for_page`]).
    Progress {
        max_ts: Option<DateTime<Utc>>,
        interval_secs: i64,
    },
    /// Pacer backpressure (cooldown / credit exhaustion) — release without an attempt,
    /// then idle (REQ-SCHED-061).
    SoftSkip,
}

/// Process one claimed backfill chunk page.
///
/// Returns `Ok(ChunkOutcome::Progress { .. })` after persisting a page (possibly empty),
/// `Ok(ChunkOutcome::SoftSkip)` on pacer backpressure (no attempt consumed),
/// `Err(DispatchError::Transient)` on a retryable failure, and `Err(DispatchError::Permanent)`
/// on a terminal failure (coin not found, no provider supports OHLC — fail-fast, REQ-SCHED-063).
async fn process_chunk(
    pool: &PgPool,
    chain: &[Arc<dyn Provider>],
    chunk: &ClaimedChunk,
    registry: Option<&crate::alarm::HealthRegistry>,
) -> Result<ChunkOutcome, DispatchError> {
    // Look up coin's trading symbol and per-coin poll interval from tracked_coins.
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT symbol, live_poll_interval::TEXT FROM tracked_coins WHERE coin_id = $1",
    )
    .bind(&chunk.coin_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| DispatchError::Transient(e.to_string()))?;
    let (symbol, live_poll_interval) =
        row.ok_or_else(|| DispatchError::Permanent(format!("coin {} not found", chunk.coin_id)))?;

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

    // Candle granularity: an explicit chunk `interval` wins (a deep-history job pins
    // `1d` — the only granularity Bitstamp serves before ~2013), otherwise fall back to
    // the per-coin poll interval (or global default). Legacy/startup chunks leave
    // `interval` NULL and keep the poll-interval behaviour.
    let global_interval = crate::config::live_quote_poll_interval_secs();
    let interval_secs = resolve_interval_secs(
        chunk.interval.as_deref(),
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
    .ok_or_else(|| DispatchError::Permanent("no provider supports OHLC".to_string()))?;

    // Classify the pacer outcome: cooldown / credit exhaustion is backpressure (soft-skip,
    // no attempt), mirroring the collection-queue worker's `pacer_should_skip_queue`
    // classification (REQ-SCHED-061); any other pacer error is a genuine transient failure.
    match acquire_slot(pool, &provider_name).await {
        Ok(()) => {}
        Err(ref e) if pacer_should_skip_queue(e) => {
            warn!("backfill_worker: pacer skip for chunk {}: {e}", chunk.id);
            return Ok(ChunkOutcome::SoftSkip);
        }
        Err(e) => return Err(DispatchError::Transient(format!("pacer: {e}"))),
    }

    let candles = if use_range_path {
        let range_start = start.expect("checked by use_range_path");
        let range_end = chunk.range_end.expect("checked by use_range_path");
        chain_fetch_ohlc_range_for_chunk(
            chain,
            &mq,
            range_start,
            range_end,
            interval_secs,
            registry,
        )
        .await
        .map_err(|e| DispatchError::Transient(e.to_string()))?
    } else {
        let days = range_to_days(start, chunk.range_end, 90); // fallback: recent-window path
        chain_fetch_ohlc_for_chunk(chain, &mq, days, interval_secs, registry)
            .await
            .map_err(|e| DispatchError::Transient(e.to_string()))?
    };

    if candles.is_empty() {
        return Ok(ChunkOutcome::Progress {
            max_ts: None,
            interval_secs,
        });
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

    // Idempotent upsert into coin_candles (REQ-SCHED-040). SPEC-REFACTOR-001 M4 (F-52): route the
    // page through the shared batched UNNEST upsert instead of a per-row loop.
    // [INTENDED CHANGE (b)] Backfill writes historical rows, so this path is SILENT — NO
    // pg_notify (CandleNotifyPolicy::Silent, REQ-REFACTOR-042), so backfilled history never floods
    // the WebSocket broadcast. Native provider rows use the unconditional DO UPDATE
    // (CandleConflictPolicy::NativeOverwrite, D1 native path — no rollup:% guard).
    let batch: Vec<crate::models::quote::CoinCandle> = filtered
        .iter()
        .map(|c| crate::models::quote::CoinCandle {
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
        })
        .collect();
    match crate::db::batched_upsert_coin_candles(
        pool,
        &batch,
        crate::db::CandleConflictPolicy::NativeOverwrite,
        crate::db::CandleNotifyPolicy::Silent,
    )
    .await
    {
        Ok(()) => {
            if !batch.is_empty() {
                if let Some(reg) = registry {
                    reg.record_upsert_success();
                }
            }
        }
        Err(e) => {
            // REQ-ALARM-042: O(1) in-memory registry poke only, never a network
            // call — the reconciler derives db-upsert-failures.
            if let Some(reg) = registry {
                reg.record_upsert_failure();
            }
            return Err(DispatchError::Transient(e.to_string()));
        }
    }

    // Return the max timestamp for cursor advancement.
    let max_ts = filtered.iter().map(|c| c.ts).max();
    Ok(ChunkOutcome::Progress {
        max_ts,
        interval_secs,
    })
}

/// Run the backfill worker loop (REQ-SCHED-020/050/051).
///
/// The claim / heartbeat / complete / release lifecycle is the shared
/// [`run_lease_worker`](crate::collectors::lease_worker::run_lease_worker) scaffold
/// (SPEC-REFACTOR-001 M3, F-53b); this function supplies only the backfill specifics — the
/// chunk claim, the heartbeat SQL, the page-processing step, and the cursor-advance +
/// complete-or-release terminal transition.
#[allow(clippy::too_many_arguments)]
pub async fn run_backfill_worker(
    pool: PgPool,
    chain: Arc<Vec<Arc<dyn Provider>>>,
    claimed_by: String,
    lease_secs: i64,
    heartbeat_interval_secs: u64,
    max_attempts: i32,
    idle_sleep: StdDuration,
    shutdown: tokio::sync::watch::Receiver<bool>,
    registry: Option<Arc<crate::alarm::HealthRegistry>>,
) -> Result<()> {
    // Claim one backfill chunk (REQ-SCHED-021/022).
    let claim = {
        let pool = pool.clone();
        let claimed_by = claimed_by.clone();
        move || -> LeaseFut<Result<Option<ClaimedChunk>, sqlx::Error>> {
            let pool = pool.clone();
            let claimed_by = claimed_by.clone();
            Box::pin(async move { claim_backfill_chunk(&pool, &claimed_by, lease_secs).await })
        }
    };

    // Heartbeat one owned chunk (REQ-SCHED-022). Log wording preserved verbatim.
    let beat = {
        let pool = pool.clone();
        let claimed_by = claimed_by.clone();
        move |id: i64| -> LeaseFut<HeartbeatStep> {
            let pool = pool.clone();
            let claimed_by = claimed_by.clone();
            Box::pin(async move {
                match heartbeat_backfill_chunk(&pool, id, &claimed_by, lease_secs).await {
                    Ok(true) => HeartbeatStep::Renewed,
                    Ok(false) => {
                        warn!("backfill_worker: heartbeat fencing fired for chunk {id}");
                        HeartbeatStep::FencedOut
                    }
                    Err(e) => {
                        error!("backfill_worker: heartbeat error for chunk {id}: {e}");
                        HeartbeatStep::Errored
                    }
                }
            })
        }
    };

    // Process one chunk page.
    let work = {
        let pool = pool.clone();
        let chain = chain.clone();
        let registry = registry.clone();
        move |chunk: ClaimedChunk| -> LeaseFut<(ClaimedChunk, Result<ChunkOutcome, DispatchError>)> {
            let pool = pool.clone();
            let chain = chain.clone();
            let registry = registry.clone();
            Box::pin(async move {
                let result = process_chunk(&pool, &chain, &chunk, registry.as_deref()).await;
                (chunk, result)
            })
        }
    };

    // Terminal transition: cursor advance + complete-or-release / fail
    // (classification preserved, REQ-REFACTOR-032).
    let finalize = {
        let pool = pool.clone();
        let claimed_by = claimed_by.clone();
        move |chunk: ClaimedChunk,
              result: Result<ChunkOutcome, DispatchError>|
              -> LeaseFut<LeaseCycleOutcome> {
            let pool = pool.clone();
            let claimed_by = claimed_by.clone();
            Box::pin(async move {
                match result {
                    Ok(ChunkOutcome::Progress {
                        max_ts,
                        interval_secs,
                    }) => {
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
                            if let Err(e) =
                                advance_cursor(&pool, chunk.id, &claimed_by, cursor).await
                            {
                                error!(
                                    "backfill_worker: cursor advance error for chunk {}: {e}",
                                    chunk.id
                                );
                            }
                        }

                        if done {
                            if let Err(e) =
                                complete_backfill_chunk(&pool, chunk.id, &claimed_by).await
                            {
                                error!(
                                    "backfill_worker: complete error for chunk {}: {e}",
                                    chunk.id
                                );
                            }
                            info!("backfill_worker: chunk {} done", chunk.id);
                        } else {
                            // More data in range (or an empty page was skipped forward): this is a
                            // NON-failure release. Route through RELEASE_BACKFILL_SQL so the
                            // claim-time attempts+1 is neutralized and a prior genuine last_error is
                            // not overwritten — a multi-page walk never consumes the retry budget
                            // (REQ-SCHED-060.1, F-01 root-cause fix). No pause: this is forward progress.
                            if let Err(e) =
                                release_backfill_chunk(&pool, chunk.id, &claimed_by).await
                            {
                                error!(
                                    "backfill_worker: partial-release error for chunk {}: {e}",
                                    chunk.id
                                );
                            }
                        }
                        LeaseCycleOutcome::Continue
                    }
                    Ok(ChunkOutcome::SoftSkip) => {
                        // Pacer backpressure: administratively release WITHOUT consuming the retry
                        // budget, then idle (REQ-SCHED-061). Mirrors the collection-queue soft-skip.
                        if let Err(e) = release_backfill_chunk(&pool, chunk.id, &claimed_by).await {
                            error!(
                                "backfill_worker: soft-skip release error for chunk {}: {e}",
                                chunk.id
                            );
                        }
                        LeaseCycleOutcome::PauseBeforeNextClaim
                    }
                    Err(DispatchError::Permanent(msg)) => {
                        // Terminal: fail immediately on the first attempt, independent of the retry
                        // budget (REQ-SCHED-063.2). No pause — the next claim is a different chunk.
                        warn!(
                            "backfill_worker: chunk {} permanently failed: {msg}",
                            chunk.id
                        );
                        if let Err(db_err) =
                            fail_permanent_backfill_chunk(&pool, chunk.id, &claimed_by, &msg).await
                        {
                            error!(
                                "backfill_worker: permanent-fail update error for chunk {}: {db_err}",
                                chunk.id
                            );
                        }
                        LeaseCycleOutcome::Continue
                    }
                    Err(DispatchError::Transient(msg)) => {
                        // Retryable: retry or fail at max_attempts (REQ-SCHED-027/063.3), then pause.
                        warn!(
                            "backfill_worker: chunk {} failed (attempts={}/{}): {msg}",
                            chunk.id, chunk.attempts, max_attempts
                        );
                        if let Err(db_err) = fail_or_retry_backfill_chunk(
                            &pool,
                            chunk.id,
                            &claimed_by,
                            max_attempts,
                            &msg,
                        )
                        .await
                        {
                            error!(
                                "backfill_worker: fail update error for chunk {}: {db_err}",
                                chunk.id
                            );
                        }
                        LeaseCycleOutcome::PauseBeforeNextClaim
                    }
                }
            })
        }
    };

    run_lease_worker(
        "backfill_worker",
        claimed_by,
        heartbeat_interval_secs,
        idle_sleep,
        shutdown,
        claim,
        beat,
        work,
        finalize,
    )
    .await
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

    // ── resolve_interval_secs: chunk.interval wins, else poll-interval fallback ──

    #[test]
    fn resolve_interval_prefers_explicit_chunk_interval() {
        // Deep-history job pins "1d" → 86400, regardless of poll-interval.
        assert_eq!(
            resolve_interval_secs(Some("1d"), Some("00:05:00"), 60),
            86_400
        );
        assert_eq!(resolve_interval_secs(Some("5m"), None, 60), 300);
    }

    #[test]
    fn resolve_interval_falls_back_to_poll_interval_when_chunk_null() {
        // Legacy/startup chunk (interval NULL): per-coin poll interval (5m) wins.
        assert_eq!(resolve_interval_secs(None, Some("00:05:00"), 60), 300);
    }

    #[test]
    fn resolve_interval_falls_back_to_global_when_both_absent() {
        assert_eq!(resolve_interval_secs(None, None, 60), 60);
    }

    #[test]
    fn resolve_interval_unparseable_chunk_interval_falls_back() {
        // A non-canonical interval string is ignored, not fatal.
        assert_eq!(resolve_interval_secs(Some("bogus"), None, 60), 60);
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

    // ── SPEC-SCHED-002: administrative-release + permanent-fail SQL shape ─────

    #[test]
    fn release_backfill_sql_neutralizes_claim_increment() {
        assert!(
            RELEASE_BACKFILL_SQL.contains("attempts         = GREATEST(attempts - 1, 0)"),
            "release SQL must neutralize the claim-time attempts+1 (REQ-SCHED-060.1)"
        );
    }

    #[test]
    fn release_backfill_sql_resets_pending_and_clears_last_error() {
        assert!(
            RELEASE_BACKFILL_SQL.contains("status           = 'pending'"),
            "release SQL must reset the chunk to pending"
        );
        assert!(
            RELEASE_BACKFILL_SQL.contains("last_error       = NULL"),
            "release SQL must write NULL (not 'partial') to last_error (REQ-SCHED-065.1)"
        );
    }

    #[test]
    fn release_backfill_sql_uses_fencing_guard() {
        assert!(
            RELEASE_BACKFILL_SQL.contains("AND claimed_by = $2"),
            "release SQL must preserve the claimed_by fence"
        );
    }

    #[test]
    fn permanent_fail_backfill_sql_is_unconditional_failed() {
        assert!(
            FAIL_PERMANENT_BACKFILL_SQL.contains("status           = 'failed'"),
            "permanent-fail SQL must set status = 'failed' unconditionally (REQ-SCHED-063.2)"
        );
        assert!(
            !FAIL_PERMANENT_BACKFILL_SQL.contains("attempts"),
            "permanent-fail SQL must not depend on the attempts budget (REQ-SCHED-063.2)"
        );
        assert!(
            FAIL_PERMANENT_BACKFILL_SQL.contains("AND claimed_by = $2"),
            "permanent-fail SQL must preserve the claimed_by fence"
        );
    }

    // ── AC-SCHED-061: backfill classifies pacer cooldown as soft-skip ────────
    // Mirrors the collection_queue soft-skip classification tests — the backfill worker
    // calls the SAME `pacer_should_skip_queue` predicate, so cooldown/credit-exhaustion is
    // backpressure (released without an attempt) and NotFound is a genuine error.

    #[test]
    fn backfill_classifies_cooldown_as_soft_skip() {
        let cooldown =
            crate::pacer::AcquireSlotError::Cooldown("coingecko".to_string(), chrono::Utc::now());
        assert!(
            pacer_should_skip_queue(&cooldown),
            "backfill must treat pacer cooldown as backpressure (REQ-SCHED-061)"
        );
    }

    #[test]
    fn backfill_classifies_credit_exhausted_as_soft_skip() {
        let exhausted = crate::pacer::AcquireSlotError::CreditExhausted("coingecko".to_string());
        assert!(
            pacer_should_skip_queue(&exhausted),
            "backfill must treat credit exhaustion as backpressure (REQ-SCHED-061)"
        );
    }

    #[test]
    fn backfill_does_not_soft_skip_not_found() {
        let not_found = crate::pacer::AcquireSlotError::NotFound("coingecko".to_string());
        assert!(
            !pacer_should_skip_queue(&not_found),
            "pacer NotFound is a genuine error, not backpressure (acceptance.md edge case)"
        );
    }

    // ── AC-REFACTOR-030a / AC-SCHED-065c: delegation to the shared lease-queue scaffold ──
    // After the M3 extraction (F-53b) the claim/heartbeat/complete/release loop — including the
    // guarded shutdown select! arms (REQ-SCHED-065.3) and the watch-based heartbeat stop
    // (REQ-REFACTOR-031) — lives in `lease_worker::run_lease_worker`. The guard invariant is
    // now behavior-verified there; this worker must merely delegate to it.

    #[test]
    fn worker_delegates_to_shared_lease_scaffold() {
        let src = std::fs::read_to_string("src/collectors/backfill.rs").expect("read backfill.rs");
        // Scan only the production code (before the test module) so this scan does not match
        // its own assertion-message string literals.
        let code = src.split("#[cfg(test)]").next().unwrap_or(&src);
        let call = format!("{}(", "run_lease_worker");
        assert!(
            code.contains(&call),
            "worker must delegate its claim/heartbeat/complete/release loop to the shared \
             lease-queue scaffold (REQ-REFACTOR-030)"
        );
        // Watch-based heartbeat stop (REQ-REFACTOR-031): no local heartbeat abort remains.
        let abort = format!(".{}()", "abort");
        assert!(
            !code.contains(&abort),
            "heartbeat must stop via the shared watch signal, not abort (REQ-REFACTOR-031)"
        );
        // No un-guarded shutdown arm remains here — the guarded arms live in the scaffold.
        let unguarded = format!("_ = shutdown{}", ".changed()");
        assert!(
            !code.contains(&unguarded),
            "no un-captured shutdown.changed() arm may remain in the worker (REQ-SCHED-065.3)"
        );
    }

    #[test]
    fn enqueue_job_sql_uses_on_conflict_do_nothing() {
        assert!(
            ENQUEUE_BACKFILL_JOB_SQL.contains("ON CONFLICT (coin_id, dataset) DO NOTHING"),
            "enqueue job SQL must be idempotent via ON CONFLICT (REQ-SCHED-028)"
        );
    }

    #[test]
    fn deep_history_dataset_distinct_from_startup_dataset() {
        // The two coexist under the ON CONFLICT (coin_id, dataset) key; a shared tag
        // would make them clobber each other.
        assert_ne!(DEEP_HISTORY_BACKFILL_DATASET, STARTUP_BACKFILL_DATASET);
        assert_eq!(DEEP_HISTORY_BACKFILL_DATASET, "candles_deep_1d");
    }

    // ── DB-gated integration tests ─────────────────────────────────────────────
    // These MUST run with `--test-threads=1`. `claim_backfill_chunk` selects the
    // globally-oldest pending chunk (`ORDER BY created_at LIMIT 1 FOR UPDATE SKIP
    // LOCKED`), so tests running concurrently against the shared DB would steal each
    // other's pending rows and flake. See CLAUDE.md § Integration Tests.

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

    /// `enqueue_deep_history_backfills`: creates a distinct `1d` deep job for a tracked
    /// coin, is idempotent on re-invocation, and skips untracked coins.
    #[tokio::test]
    #[ignore]
    async fn db_enqueue_deep_history_backfills_idempotent_and_skips_untracked() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
        let pool = crate::db::connect(&url).await.expect("connect");

        let coin_id = "test-deep-history-coin";
        let untracked = "test-deep-history-untracked";
        for c in [coin_id, untracked] {
            sqlx::query("DELETE FROM backfill_chunks WHERE coin_id = $1")
                .bind(c)
                .execute(&pool)
                .await
                .expect("pre-cleanup chunks");
            sqlx::query("DELETE FROM backfill_jobs WHERE coin_id = $1")
                .bind(c)
                .execute(&pool)
                .await
                .expect("pre-cleanup jobs");
        }
        sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
            .bind(coin_id)
            .execute(&pool)
            .await
            .expect("pre-cleanup tracked");
        sqlx::query(
            "INSERT INTO tracked_coins (coin_id, symbol, name, status, registered_at) \
             VALUES ($1, 'TDHC', 'Test Deep History Coin', 'active', now())",
        )
        .bind(coin_id)
        .execute(&pool)
        .await
        .expect("insert tracked coin");

        let start = chrono::Utc.with_ymd_and_hms(2011, 8, 18, 0, 0, 0).unwrap();
        let end = chrono::Utc.with_ymd_and_hms(2016, 7, 1, 0, 0, 0).unwrap();

        // First call: tracked coin enqueued, untracked skipped.
        let (enqueued, skipped) = enqueue_deep_history_backfills(
            &pool,
            &[coin_id.to_string(), untracked.to_string()],
            start,
            end,
        )
        .await
        .expect("first deep enqueue");
        assert_eq!(enqueued, 1, "only the tracked coin is enqueued");
        assert_eq!(skipped, 1, "the untracked coin is skipped");

        // The chunk must carry interval='1d' and the requested range.
        let (interval, range_start): (Option<String>, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT c.interval, c.range_start FROM backfill_chunks c \
             JOIN backfill_jobs j ON j.id = c.job_id \
             WHERE c.coin_id = $1 AND j.dataset = $2",
        )
        .bind(coin_id)
        .bind(DEEP_HISTORY_BACKFILL_DATASET)
        .fetch_one(&pool)
        .await
        .expect("fetch deep chunk");
        assert_eq!(interval.as_deref(), Some("1d"), "deep chunk pins 1d");
        assert_eq!(range_start, Some(start));

        // Second call: idempotent — the tracked coin is now skipped too.
        let (enqueued_2, _skipped_2) =
            enqueue_deep_history_backfills(&pool, &[coin_id.to_string()], start, end)
                .await
                .expect("second deep enqueue");
        assert_eq!(
            enqueued_2, 0,
            "re-invocation must not duplicate the deep job"
        );

        let job_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM backfill_jobs WHERE coin_id = $1 AND dataset = $2",
        )
        .bind(coin_id)
        .bind(DEEP_HISTORY_BACKFILL_DATASET)
        .fetch_one(&pool)
        .await
        .expect("count deep jobs");
        assert_eq!(job_count, 1, "exactly one deep job after two calls");

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

    /// AC-SCHED-060a / REQ-SCHED-060.1: a chunk that is claimed and partial-released more
    /// than `max_attempts` times (walking pages) and then hits exactly ONE genuine transient
    /// failure must remain `pending` (still retryable), NOT `failed` — page count must never
    /// fail a chunk (F-01 root-cause regression test).
    #[tokio::test]
    #[ignore]
    async fn db_partial_release_page_walk_does_not_fail_chunk() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
        let pool = crate::db::connect(&url).await.expect("connect");
        let max_attempts = 5;
        let coin_id = "test-f01-page-walk";

        // Fresh tracked_coin + job + chunk. `backfill_jobs.coin_id` has an FK to
        // `tracked_coins(coin_id)`, so the parent row MUST exist before the job INSERT even
        // though this test drives the claim/release SQL wrappers directly (not process_chunk).
        // FK-safe order — delete: chunks → jobs → tracked_coins; insert: tracked_coins → job → chunk.
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
             VALUES ($1, 'TF01W', 'Test F01 Page Walk', 'active', now())",
        )
        .bind(coin_id)
        .execute(&pool)
        .await
        .expect("insert tracked coin");
        let job_id: i64 = sqlx::query_scalar(
            "INSERT INTO backfill_jobs (coin_id, dataset, status, requested_at, updated_at) \
             VALUES ($1, 'ohlc_1d', 'pending', now(), now()) RETURNING id",
        )
        .bind(coin_id)
        .fetch_one(&pool)
        .await
        .expect("insert job");
        let chunk_id: i64 = sqlx::query_scalar(
            "INSERT INTO backfill_chunks \
             (job_id, coin_id, dataset, interval, range_start, range_end, status, created_at, updated_at) \
             VALUES ($1, $2, 'ohlc_1d', '1d', now() - INTERVAL '30 days', now(), 'pending', now(), now()) \
             RETURNING id",
        )
        .bind(job_id)
        .bind(coin_id)
        .fetch_one(&pool)
        .await
        .expect("insert chunk");

        // Walk pages: claim (attempts += 1) then partial-release (attempts -= 1), more than
        // max_attempts times.
        for _ in 0..(max_attempts + 3) {
            let claimed = claim_backfill_chunk(&pool, "test-replica", 300)
                .await
                .expect("claim")
                .expect("should re-claim the released chunk");
            assert_eq!(claimed.id, chunk_id);
            release_backfill_chunk(&pool, chunk_id, "test-replica")
                .await
                .expect("partial release");
        }

        let attempts_after_walk: i32 =
            sqlx::query_scalar("SELECT attempts FROM backfill_chunks WHERE id = $1")
                .bind(chunk_id)
                .fetch_one(&pool)
                .await
                .expect("fetch attempts");
        assert_eq!(
            attempts_after_walk, 0,
            "page-walking must leave the effective attempt count unchanged (REQ-SCHED-060.1)"
        );

        // One genuine transient failure: claim (attempts→1) then fail_or_retry.
        let claimed = claim_backfill_chunk(&pool, "test-replica", 300)
            .await
            .expect("claim")
            .expect("claim for genuine failure");
        assert_eq!(claimed.attempts, 1);
        fail_or_retry_backfill_chunk(&pool, chunk_id, "test-replica", max_attempts, "boom")
            .await
            .expect("genuine transient failure");

        let status: String = sqlx::query_scalar("SELECT status FROM backfill_chunks WHERE id = $1")
            .bind(chunk_id)
            .fetch_one(&pool)
            .await
            .expect("fetch status");
        assert_eq!(
            status, "pending",
            "a page-count of releases must not fail a chunk (F-01, AC-SCHED-060a)"
        );

        // Cleanup — FK-safe order: chunks → jobs → tracked_coins.
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

    /// AC-SCHED-060c / REQ-SCHED-060.3: genuine failures still bound retries — after
    /// `max_attempts` genuine transient failures the chunk is marked `failed`.
    #[tokio::test]
    #[ignore]
    async fn db_genuine_failures_still_bound_retries() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
        let pool = crate::db::connect(&url).await.expect("connect");
        let max_attempts = 3;
        let coin_id = "test-f01-bound";

        // `backfill_jobs.coin_id` has an FK to `tracked_coins(coin_id)`, so the parent row MUST
        // exist before the job INSERT even though this test drives the claim/fail SQL wrappers
        // directly. FK-safe order — delete: chunks → jobs → tracked_coins; insert: tracked_coins → job → chunk.
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
             VALUES ($1, 'TF01B', 'Test F01 Bound Retries', 'active', now())",
        )
        .bind(coin_id)
        .execute(&pool)
        .await
        .expect("insert tracked coin");
        let job_id: i64 = sqlx::query_scalar(
            "INSERT INTO backfill_jobs (coin_id, dataset, status, requested_at, updated_at) \
             VALUES ($1, 'ohlc_1d', 'pending', now(), now()) RETURNING id",
        )
        .bind(coin_id)
        .fetch_one(&pool)
        .await
        .expect("insert job");
        let chunk_id: i64 = sqlx::query_scalar(
            "INSERT INTO backfill_chunks \
             (job_id, coin_id, dataset, interval, range_start, range_end, status, created_at, updated_at) \
             VALUES ($1, $2, 'ohlc_1d', '1d', now() - INTERVAL '30 days', now(), 'pending', now(), now()) \
             RETURNING id",
        )
        .bind(job_id)
        .bind(coin_id)
        .fetch_one(&pool)
        .await
        .expect("insert chunk");

        // max_attempts genuine transient failures: each claims (attempts += 1) then fails.
        let mut last_status = String::new();
        for _ in 0..max_attempts {
            let claimed = claim_backfill_chunk(&pool, "test-replica", 300)
                .await
                .expect("claim")
                .expect("claim");
            fail_or_retry_backfill_chunk(&pool, chunk_id, "test-replica", max_attempts, "boom")
                .await
                .expect("genuine failure");
            last_status = sqlx::query_scalar("SELECT status FROM backfill_chunks WHERE id = $1")
                .bind(claimed.id)
                .fetch_one(&pool)
                .await
                .expect("fetch status");
        }
        assert_eq!(
            last_status, "failed",
            "max_attempts genuine failures must mark the chunk failed (REQ-SCHED-060.3)"
        );

        // Cleanup — FK-safe order: chunks → jobs → tracked_coins.
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
