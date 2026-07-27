//! Collection-queue worker (SPEC-SCHED-001 REQ-SCHED-010..015, 030, 031, 041, 042).
//!
//! Claims `collection_queue` rows via `FOR UPDATE SKIP LOCKED` (oldest pending or
//! lease-expired first), then dispatches per-kind collectors (candles, metadata, market,
//! derivatives) through the provider chain with pacer pacing.
//!
//! # Lease + heartbeat + fencing
//!
//! All mutating UPDATEs after the claim include `AND claimed_by = $self` so that a
//! re-claimed row by another replica cannot be double-updated ("zombie fencing").
//!
//! # Attempt counting (SPEC-SCHED-002 REQ-SCHED-060/063)
//!
//! `attempts` is incremented at claim time, but only *genuine transient failures* count
//! toward `max_attempts`. A **non-failure release** (a pacer soft-skip) routes through
//! `RELEASE_QUEUE_SQL`, which resets the row to `pending`, writes `last_error = NULL`, and
//! neutralizes the claim-time increment (`attempts = GREATEST(attempts - 1, 0)`), so
//! walking the queue under backpressure never exhausts the retry budget (REQ-SCHED-060).
//! A **permanent** failure (coin not found, no provider supports the capability, unknown
//! dispatch kind) routes through `FAIL_PERMANENT_QUEUE_SQL` and is marked `'failed'`
//! immediately on the first attempt, without relying on the retry budget (REQ-SCHED-063.2).
//! A **transient** failure routes through `FAIL_OR_RETRY_QUEUE_SQL`, which marks `'failed'`
//! only once `attempts >= max_attempts` (REQ-SCHED-013/060.3).

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tracing::{error, info, warn};

use crate::collectors::lease_worker::{
    run_lease_worker, HeartbeatStep, LeaseCycleOutcome, LeaseFut, LeaseItem,
};
use crate::collectors::retry::DispatchError;
use crate::db::upserts::{
    upsert_coin_candle, upsert_coin_market_snapshot, upsert_coin_metadata, upsert_coin_quote,
};
use crate::models::quote::CoinCandle;
use crate::pacer::{acquire_slot, AcquireSlotError};
use crate::providers::{Capability, MarketQuery, OhlcCandle, Provider, ProviderError};

// ── Pure scheduling functions (unit-testable, no I/O) ────────────────────────

/// Returns `true` if the item should be retried (attempts < max), `false` if permanently failed.
pub fn should_retry(attempts: i32, max_attempts: i32) -> bool {
    attempts < max_attempts
}

/// Returns `true` if a pacer error should cause a soft skip (no attempt increment).
/// Returns `false` if the error is unexpected and the claim should be released for retry.
pub fn pacer_should_skip_queue(err: &AcquireSlotError) -> bool {
    matches!(
        err,
        AcquireSlotError::Cooldown(..) | AcquireSlotError::CreditExhausted(..)
    )
}

// ── SQL constants ─────────────────────────────────────────────────────────────

/// Claim one `collection_queue` row via `FOR UPDATE SKIP LOCKED` (REQ-SCHED-010/011/014/015).
///
/// Predicate: `status = 'pending'` OR (`status IN ('claimed','running')` AND lease expired).
/// Ordered oldest-first (`enqueued_at ASC`) for fair claiming.
/// Increments `attempts` at claim time. Per SPEC-SCHED-002 REQ-SCHED-060 the bound now
/// counts only genuine transient failures: a non-failure release (`RELEASE_QUEUE_SQL`)
/// decrements `attempts` back to neutralize this claim-time `+1`, so a soft-skip walk
/// never advances the budget; a crash re-claim keeps its `+1` (crash-loops stay bounded).
///
// @MX:ANCHOR: [AUTO] CLAIM_QUEUE_SQL — FOR UPDATE SKIP LOCKED single-owner invariant
// @MX:REASON: fan_in >= 3: claim_queue_item(), SQL-shape tests, DB integration tests.
//             REQ-SCHED-015: SKIP LOCKED + lease = at-most-one replica per row at a time.
//             REQ-SCHED-014: lease-expired predicate allows crash-recovery re-claim.
//             SPEC-SCHED-002 REQ-SCHED-060: attempts+1 here is neutralized by RELEASE_QUEUE_SQL
//             on non-failure releases, so the bound counts genuine failures, not pages walked.
// @MX:SPEC: SPEC-SCHED-001 REQ-SCHED-010 REQ-SCHED-011 REQ-SCHED-014 REQ-SCHED-015 SPEC-SCHED-002 REQ-SCHED-060
pub const CLAIM_QUEUE_SQL: &str = "\
    UPDATE collection_queue SET \
        status           = 'claimed', \
        claimed_by       = $1, \
        lease_expires_at = now() + ($2 * INTERVAL '1 second'), \
        heartbeat_at     = now(), \
        attempts         = attempts + 1, \
        updated_at       = now() \
    WHERE id = ( \
        SELECT id FROM collection_queue \
        WHERE status = 'pending' \
           OR (status IN ('claimed','running') AND lease_expires_at < now()) \
        ORDER BY enqueued_at \
        LIMIT 1 \
        FOR UPDATE SKIP LOCKED \
    ) \
    RETURNING id, target_kind, target_id, kind, status, claimed_by, \
              lease_expires_at, heartbeat_at, attempts, last_error, \
              enqueued_at, updated_at";

/// Heartbeat UPDATE: renews the lease and records the heartbeat instant (REQ-SCHED-011).
/// The `AND claimed_by = $3` guard prevents double-update if another replica stole the lease.
pub const HEARTBEAT_QUEUE_SQL: &str = "\
    UPDATE collection_queue SET \
        lease_expires_at = now() + ($1 * INTERVAL '1 second'), \
        heartbeat_at     = now(), \
        updated_at       = now() \
    WHERE id = $2 AND claimed_by = $3";

/// Success UPDATE: mark the row as done (REQ-SCHED-012).
pub const COMPLETE_QUEUE_SQL: &str = "\
    UPDATE collection_queue SET \
        status     = 'done', \
        updated_at = now() \
    WHERE id = $1 AND claimed_by = $2";

/// Genuine-failure UPDATE: mark `failed` once `attempts >= $3`, else reset to `pending`
/// for retry (REQ-SCHED-013). This is the **transient-failure-only** path — non-failure
/// releases use `RELEASE_QUEUE_SQL` and permanent failures use `FAIL_PERMANENT_QUEUE_SQL`.
///
// @MX:NOTE: [AUTO] FAIL_OR_RETRY_QUEUE_SQL — genuine transient-failure-only path (SPEC-SCHED-002 REQ-SCHED-060.3/063.3)
// @MX:SPEC: SPEC-SCHED-001 REQ-SCHED-013 SPEC-SCHED-002 REQ-SCHED-060 REQ-SCHED-063
pub const FAIL_OR_RETRY_QUEUE_SQL: &str = "\
    UPDATE collection_queue SET \
        status           = CASE WHEN attempts >= $3 THEN 'failed' ELSE 'pending' END, \
        last_error       = $4, \
        lease_expires_at = NULL, \
        claimed_by       = NULL, \
        heartbeat_at     = NULL, \
        updated_at       = now() \
    WHERE id = $1 AND claimed_by = $2";

/// Administrative (non-failure) release of a claimed item — a pacer soft-skip
/// (SPEC-SCHED-002 REQ-SCHED-060.2/061/065.1). Resets to `pending`, writes a NON-error
/// marker (`last_error = NULL`, never `"pacer_skip"`), and neutralizes the claim-time
/// `attempts + 1` via `GREATEST(attempts - 1, 0)` so a soft-skip never consumes the
/// retry budget. Carries the `AND claimed_by = $2` fence like the failure paths.
///
// @MX:WARN: [AUTO] RELEASE_QUEUE_SQL — administrative release; neutralizes the claim-time attempts+1 and clears last_error
// @MX:REASON: SPEC-SCHED-002 REQ-SCHED-060/065 root-cause guard (F-01). Do NOT "restore" the old
//             FAIL_OR_RETRY reuse with i32::MAX — that reused the failure SQL for soft-skips, so
//             page-walking/backpressure re-claims counted toward max_attempts and overwrote a
//             prior genuine last_error with "pacer_skip". This path must stay attempt-neutral.
// @MX:SPEC: SPEC-SCHED-002 REQ-SCHED-060 REQ-SCHED-061 REQ-SCHED-065
pub const RELEASE_QUEUE_SQL: &str = "\
    UPDATE collection_queue SET \
        status           = 'pending', \
        last_error       = NULL, \
        attempts         = GREATEST(attempts - 1, 0), \
        lease_expires_at = NULL, \
        claimed_by       = NULL, \
        heartbeat_at     = NULL, \
        updated_at       = now() \
    WHERE id = $1 AND claimed_by = $2";

/// Permanent-failure UPDATE: mark `failed` immediately, regardless of `attempts`
/// (SPEC-SCHED-002 REQ-SCHED-063.2). A permanent failure is terminal and does not consume
/// or rely on the `max_attempts` retry budget. Carries the `AND claimed_by = $2` fence.
///
// @MX:NOTE: [AUTO] FAIL_PERMANENT_QUEUE_SQL — terminal fail-fast, retry-budget-independent (SPEC-SCHED-002 REQ-SCHED-063.2)
// @MX:SPEC: SPEC-SCHED-002 REQ-SCHED-063
pub const FAIL_PERMANENT_QUEUE_SQL: &str = "\
    UPDATE collection_queue SET \
        status           = 'failed', \
        last_error       = $3, \
        lease_expires_at = NULL, \
        claimed_by       = NULL, \
        heartbeat_at     = NULL, \
        updated_at       = now() \
    WHERE id = $1 AND claimed_by = $2";

/// Enqueue one `(target_kind, target_id, kind)` item idempotently (REQ-SCHED-030).
///
/// `ON CONFLICT DO NOTHING` absorbs re-registrations; the partial dedup index
/// `collection_queue_dedup_idx` prevents a second live row for the same triple.
///
// @MX:NOTE: [AUTO] ENQUEUE_QUEUE_SQL — ON CONFLICT DO NOTHING; partial dedup absorbs re-enqueue (REQ-SCHED-030)
// @MX:SPEC: SPEC-SCHED-001 REQ-SCHED-030
pub const ENQUEUE_QUEUE_SQL: &str = "\
    INSERT INTO collection_queue \
        (target_kind, target_id, kind, status, enqueued_at, updated_at) \
    VALUES ($1, $2, $3, 'pending', now(), now()) \
    ON CONFLICT DO NOTHING";

// ── Structs ───────────────────────────────────────────────────────────────────

/// A successfully claimed `collection_queue` row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ClaimedQueueItem {
    pub id: i64,
    pub target_kind: String,
    pub target_id: String,
    pub kind: String,
    pub status: String,
    pub claimed_by: Option<String>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub enqueued_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl LeaseItem for ClaimedQueueItem {
    fn lease_id(&self) -> i64 {
        self.id
    }
}

// ── DB functions ──────────────────────────────────────────────────────────────

/// Claim one `collection_queue` item via `FOR UPDATE SKIP LOCKED`.
///
/// Returns `None` when no claimable row exists.
pub async fn claim_queue_item(
    pool: &PgPool,
    claimed_by: &str,
    lease_secs: i64,
) -> Result<Option<ClaimedQueueItem>, sqlx::Error> {
    let row: Option<ClaimedQueueItem> = sqlx::query_as(CLAIM_QUEUE_SQL)
        .bind(claimed_by)
        .bind(lease_secs)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Renew the lease on an owned item. Returns `false` if the fencing guard fired.
pub async fn heartbeat_queue_item(
    pool: &PgPool,
    id: i64,
    claimed_by: &str,
    lease_secs: i64,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(HEARTBEAT_QUEUE_SQL)
        .bind(lease_secs)
        .bind(id)
        .bind(claimed_by)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Mark a queue item as done (REQ-SCHED-012).
pub async fn complete_queue_item(
    pool: &PgPool,
    id: i64,
    claimed_by: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(COMPLETE_QUEUE_SQL)
        .bind(id)
        .bind(claimed_by)
        .execute(pool)
        .await?;
    Ok(())
}

/// Handle a genuine transient failure: retry if under max_attempts, else permanently
/// fail (REQ-SCHED-013). Non-failure releases use [`release_queue_item`]; permanent
/// failures use [`fail_permanent_queue_item`].
pub async fn fail_or_retry_queue_item(
    pool: &PgPool,
    id: i64,
    claimed_by: &str,
    max_attempts: i32,
    error: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(FAIL_OR_RETRY_QUEUE_SQL)
        .bind(id)
        .bind(claimed_by)
        .bind(max_attempts)
        .bind(error)
        .execute(pool)
        .await?;
    Ok(())
}

/// Administratively release a claimed item for a non-failure reason (a pacer soft-skip)
/// without consuming the retry budget (SPEC-SCHED-002 REQ-SCHED-060.2/061/065.1).
pub async fn release_queue_item(
    pool: &PgPool,
    id: i64,
    claimed_by: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(RELEASE_QUEUE_SQL)
        .bind(id)
        .bind(claimed_by)
        .execute(pool)
        .await?;
    Ok(())
}

/// Mark an item permanently `failed` on a permanent dispatch error, immediately and
/// independent of the retry budget (SPEC-SCHED-002 REQ-SCHED-063.2).
pub async fn fail_permanent_queue_item(
    pool: &PgPool,
    id: i64,
    claimed_by: &str,
    error: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(FAIL_PERMANENT_QUEUE_SQL)
        .bind(id)
        .bind(claimed_by)
        .bind(error)
        .execute(pool)
        .await?;
    Ok(())
}

/// Enqueue a work item idempotently (REQ-SCHED-030).
///
/// Returns `true` if a new row was inserted, `false` if the dedup index absorbed it.
pub async fn enqueue_queue_item(
    pool: &PgPool,
    target_kind: &str,
    target_id: &str,
    kind: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(ENQUEUE_QUEUE_SQL)
        .bind(target_kind)
        .bind(target_id)
        .bind(kind)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

// ── Coin context lookup ───────────────────────────────────────────────────────

/// Fetch the trading symbol and optional per-coin poll interval for a tracked coin.
///
/// `live_poll_interval` is returned as a PG INTERVAL cast to TEXT (e.g. `"00:05:00"`).
/// Returns `None` when the coin is not found.
async fn fetch_coin_context(
    pool: &PgPool,
    coin_id: &str,
) -> Result<Option<(String, Option<String>)>> {
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT symbol, live_poll_interval::TEXT FROM tracked_coins WHERE coin_id = $1",
    )
    .bind(coin_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

// ── Chain dispatch helpers ────────────────────────────────────────────────────

/// Try providers in order for `fetch_ohlc`; return first success.
///
/// `registry`, when present, feeds SPEC-ALARM-001's Tier 1 desired-state derivation
/// (provider-unreachable/all-providers-down, REQ-ALARM-020/022); `None` is a no-op.
async fn chain_fetch_ohlc_local(
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

/// Find the first provider supporting `cap`; return its name for pacer pacing.
fn first_provider_for_cap(chain: &[Arc<dyn Provider>], cap: Capability) -> Option<String> {
    chain
        .iter()
        .find(|p| p.supports(cap))
        .map(|p| p.name().to_string())
}

// ── Worker dispatch ───────────────────────────────────────────────────────────

/// Outcome of dispatching one claimed queue item (SPEC-SCHED-002 REQ-SCHED-060/061/063).
///
/// The `Err` channel ([`DispatchError`]) carries genuine failures classified
/// transient-vs-permanent; backpressure is the `SoftSkip` non-failure variant here, so it
/// never touches the retry budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// Work completed successfully — mark the item `done`.
    Done,
    /// Pacer backpressure (cooldown / credit exhaustion) — release without an attempt,
    /// then idle (REQ-SCHED-060.2/061).
    SoftSkip,
}

/// Dispatch one claimed queue item to its collector and upsert the result.
///
/// Returns `Ok(DispatchOutcome::Done)` on success, `Ok(DispatchOutcome::SoftSkip)` on pacer
/// backpressure (no attempt consumed), `Err(DispatchError::Transient)` on a retryable
/// failure, and `Err(DispatchError::Permanent)` on a terminal failure (coin not found, no
/// provider supports the capability, unknown dispatch kind — fail-fast, REQ-SCHED-063).
async fn dispatch_item(
    pool: &PgPool,
    chain: &[Arc<dyn Provider>],
    item: &ClaimedQueueItem,
    registry: Option<&crate::alarm::HealthRegistry>,
) -> Result<DispatchOutcome, DispatchError> {
    match (item.target_kind.as_str(), item.kind.as_str()) {
        ("coin", "candles") => {
            let coin_id = &item.target_id;

            let (symbol, live_poll_interval) = fetch_coin_context(pool, coin_id)
                .await
                .map_err(|e| DispatchError::Transient(e.to_string()))?
                .ok_or_else(|| DispatchError::Permanent(format!("coin {coin_id} not found")))?;

            let cap = Capability::Ohlc;
            let provider_name = match first_provider_for_cap(chain, cap) {
                Some(n) => n,
                None => {
                    return Err(DispatchError::Permanent(
                        "no provider supports OHLC".to_string(),
                    ))
                }
            };

            match acquire_slot(pool, &provider_name).await {
                Err(ref e) if pacer_should_skip_queue(e) => {
                    warn!("queue_worker: pacer skip for item {}: {e}", item.id);
                    return Ok(DispatchOutcome::SoftSkip); // no attempt increment
                }
                Err(e) => return Err(DispatchError::Transient(format!("pacer: {e}"))),
                Ok(()) => {}
            }

            let mq = MarketQuery {
                market_id: 0, // dummy; coin-keyed dispatch does not use market_id
                coin_id: Some(coin_id.clone()),
                base: symbol,
                quote: "USDT".to_string(),
                venue: None,
                vs_currency: "usd".to_string(),
            };

            // Candle granularity = per-coin poll interval (or global default).
            let global_interval = crate::config::live_quote_poll_interval_secs();
            let interval_secs = crate::config::effective_candle_interval_secs(
                live_poll_interval.as_deref(),
                global_interval,
            );

            // REQ-OBS-012/015: instrument provider call with counter + duration histogram.
            let fetch_start = std::time::Instant::now();
            let candles_result =
                chain_fetch_ohlc_local(chain, &mq, 7, interval_secs, registry).await;
            let fetch_dur = fetch_start.elapsed().as_secs_f64();
            let outcome = if candles_result.is_ok() {
                "success"
            } else {
                "error"
            };
            metrics::counter!(
                "collection_requests_total",
                "provider" => provider_name.clone(),
                "capability" => "ohlc",
                "outcome" => outcome,
            )
            .increment(1);
            metrics::histogram!(
                "collection_request_duration_seconds",
                "provider" => provider_name,
                "capability" => "ohlc",
            )
            .record(fetch_dur);
            let candles = candles_result.map_err(|e| DispatchError::Transient(e.to_string()))?;

            for c in &candles {
                let candle = CoinCandle {
                    coin_id: coin_id.clone(),
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
                match upsert_coin_candle(pool, &candle).await {
                    Ok(()) => {
                        if let Some(reg) = registry {
                            reg.record_upsert_success();
                        }
                    }
                    Err(e) => {
                        // REQ-ALARM-042: O(1) in-memory registry poke only, never a
                        // network call — the reconciler derives db-upsert-failures.
                        if let Some(reg) = registry {
                            reg.record_upsert_failure();
                        }
                        return Err(DispatchError::Transient(e.to_string()));
                    }
                }
            }

            // SPEC-CANDLE-001 REQ-CANDLE-020: enqueue a network-free rollup recompute after
            // every candle refresh so materialized 1d/1w rows stay current. Duplicate items
            // are dedup-absorbed by enqueue_queue_item's ON CONFLICT DO NOTHING.
            if let Err(e) = enqueue_queue_item(pool, "coin", coin_id, "rollup").await {
                warn!("queue_worker: rollup enqueue failed for coin {coin_id}: {e}");
            }

            Ok(DispatchOutcome::Done)
        }

        ("coin", "spot") => {
            let coin_id = &item.target_id;

            let (symbol, _) = fetch_coin_context(pool, coin_id)
                .await
                .map_err(|e| DispatchError::Transient(e.to_string()))?
                .ok_or_else(|| DispatchError::Permanent(format!("coin {coin_id} not found")))?;

            let cap = Capability::Spot;
            let provider_name = match first_provider_for_cap(chain, cap) {
                Some(n) => n,
                None => {
                    return Err(DispatchError::Permanent(
                        "no provider supports Spot".to_string(),
                    ))
                }
            };

            let mq = MarketQuery {
                market_id: 0, // dummy; coin-keyed dispatch does not use market_id
                coin_id: Some(coin_id.clone()),
                base: symbol,
                quote: "USDT".to_string(),
                venue: None,
                vs_currency: "usd".to_string(),
            };

            // REQ-OBS-012/015: instrument provider call. Pacer acquire moves INTO chain_try,
            // charged per-attempted-provider (F-16, REQ-REFACTOR-021); first_provider_for_cap
            // above is retained only for the permanent-no-capable check and the metric label
            // (behavior-preserving, REQ-REFACTOR-080).
            let fetch_start = std::time::Instant::now();
            let quote_result = crate::providers::chain_try(
                chain,
                cap,
                registry,
                |name| Box::pin(acquire_slot(pool, name)),
                |p| Box::pin(p.fetch_spot(&mq)),
            )
            .await;

            // A pacer soft-skip releases WITHOUT an attempt and emits NO metric (backpressure is
            // not a served request); a non-skip pacer error is transient — both bail before the
            // metric block, exactly as the prior pre-fetch acquire did (REQ-SCHED-060.2/061).
            if let Err(ProviderError::Pacer(ref e)) = quote_result {
                if pacer_should_skip_queue(e) {
                    warn!("queue_worker: pacer skip for item {}: {e}", item.id);
                    return Ok(DispatchOutcome::SoftSkip);
                }
                return Err(DispatchError::Transient(format!("pacer: {e}")));
            }

            let fetch_dur = fetch_start.elapsed().as_secs_f64();
            let outcome = if quote_result.is_ok() {
                "success"
            } else {
                "error"
            };
            metrics::counter!(
                "collection_requests_total",
                "provider" => provider_name.clone(),
                "capability" => "spot",
                "outcome" => outcome,
            )
            .increment(1);
            metrics::histogram!(
                "collection_request_duration_seconds",
                "provider" => provider_name,
                "capability" => "spot",
            )
            .record(fetch_dur);
            let quote = quote_result.map_err(|e| DispatchError::Transient(e.to_string()))?;

            match upsert_coin_quote(pool, coin_id, &quote).await {
                Ok(()) => {
                    if let Some(reg) = registry {
                        reg.record_upsert_success();
                    }
                }
                Err(e) => {
                    if let Some(reg) = registry {
                        reg.record_upsert_failure();
                    }
                    return Err(DispatchError::Transient(e.to_string()));
                }
            }

            Ok(DispatchOutcome::Done)
        }

        ("coin", "metadata") => {
            let coin_id = &item.target_id;

            let cap = Capability::CoinMetadata;
            let provider_name = match first_provider_for_cap(chain, cap) {
                Some(n) => n,
                None => {
                    return Err(DispatchError::Permanent(
                        "no provider supports CoinMetadata".to_string(),
                    ))
                }
            };

            // REQ-OBS-012/015: instrument provider call. Pacer acquire moves INTO chain_try,
            // charged per-attempted-provider (F-16); first_provider_for_cap is retained only for
            // the permanent-no-capable check and the metric label (behavior-preserving).
            let fetch_start = std::time::Instant::now();
            let meta_result = crate::providers::chain_try(
                chain,
                cap,
                registry,
                |name| Box::pin(acquire_slot(pool, name)),
                |p| Box::pin(p.fetch_coin_metadata(coin_id)),
            )
            .await;

            if let Err(ProviderError::Pacer(ref e)) = meta_result {
                if pacer_should_skip_queue(e) {
                    warn!("queue_worker: pacer skip for item {}: {e}", item.id);
                    return Ok(DispatchOutcome::SoftSkip);
                }
                return Err(DispatchError::Transient(format!("pacer: {e}")));
            }

            let fetch_dur = fetch_start.elapsed().as_secs_f64();
            let outcome = if meta_result.is_ok() {
                "success"
            } else {
                "error"
            };
            metrics::counter!(
                "collection_requests_total",
                "provider" => provider_name.clone(),
                "capability" => "coin_metadata",
                "outcome" => outcome,
            )
            .increment(1);
            metrics::histogram!(
                "collection_request_duration_seconds",
                "provider" => provider_name,
                "capability" => "coin_metadata",
            )
            .record(fetch_dur);
            let meta = meta_result.map_err(|e| DispatchError::Transient(e.to_string()))?;

            // Revision upsert (REQ-SCHED-042): new revision only if values changed.
            match upsert_coin_metadata(pool, &meta).await {
                Ok(()) => {
                    if let Some(reg) = registry {
                        reg.record_upsert_success();
                    }
                }
                Err(e) => {
                    if let Some(reg) = registry {
                        reg.record_upsert_failure();
                    }
                    return Err(DispatchError::Transient(e.to_string()));
                }
            }

            Ok(DispatchOutcome::Done)
        }

        ("coin", "market") => {
            let coin_id = &item.target_id;

            let cap = Capability::CoinMarket;
            let provider_name = match first_provider_for_cap(chain, cap) {
                Some(n) => n,
                None => {
                    return Err(DispatchError::Permanent(
                        "no provider supports CoinMarket".to_string(),
                    ))
                }
            };

            // REQ-OBS-012/015: instrument provider call. Pacer acquire moves INTO chain_try,
            // charged per-attempted-provider (F-16); first_provider_for_cap is retained only for
            // the permanent-no-capable check and the metric label (behavior-preserving).
            let fetch_start = std::time::Instant::now();
            let snapshot_result = crate::providers::chain_try(
                chain,
                cap,
                registry,
                |name| Box::pin(acquire_slot(pool, name)),
                |p| Box::pin(p.fetch_coin_market(coin_id, "usd")),
            )
            .await;

            if let Err(ProviderError::Pacer(ref e)) = snapshot_result {
                if pacer_should_skip_queue(e) {
                    warn!("queue_worker: pacer skip for item {}: {e}", item.id);
                    return Ok(DispatchOutcome::SoftSkip);
                }
                return Err(DispatchError::Transient(format!("pacer: {e}")));
            }

            let fetch_dur = fetch_start.elapsed().as_secs_f64();
            let outcome = if snapshot_result.is_ok() {
                "success"
            } else {
                "error"
            };
            metrics::counter!(
                "collection_requests_total",
                "provider" => provider_name.clone(),
                "capability" => "coin_market",
                "outcome" => outcome,
            )
            .increment(1);
            metrics::histogram!(
                "collection_request_duration_seconds",
                "provider" => provider_name,
                "capability" => "coin_market",
            )
            .record(fetch_dur);
            let snapshot = snapshot_result.map_err(|e| DispatchError::Transient(e.to_string()))?;

            match upsert_coin_market_snapshot(pool, &snapshot).await {
                Ok(()) => {
                    if let Some(reg) = registry {
                        reg.record_upsert_success();
                    }
                }
                Err(e) => {
                    if let Some(reg) = registry {
                        reg.record_upsert_failure();
                    }
                    return Err(DispatchError::Transient(e.to_string()));
                }
            }

            Ok(DispatchOutcome::Done)
        }

        ("coin", "cycle_overlay") => {
            // SPEC-CYCLE-001 REQ-CYCLE-041/042: full idempotent derived rebuild from
            // coin_candles. No provider/pacer call — this kind never touches the network.
            let coin_id = &item.target_id;
            let vs_currency = crate::config::cycle_overlay_vs_currency();
            crate::collectors::cycle_overlay::recompute_cycle_overlay(pool, coin_id, &vs_currency)
                .await
                .map_err(|e| DispatchError::Transient(e.to_string()))?;
            Ok(DispatchOutcome::Done)
        }

        ("coin", "rollup") => {
            // SPEC-CANDLE-001 REQ-CANDLE-024: materialize native 1d/1w OHLCV rollups from
            // coin_candles. No provider/pacer call — this kind never touches the network,
            // mirroring the ("coin","cycle_overlay") arm above.
            let coin_id = &item.target_id;
            crate::collectors::rollup::run_rollup(
                pool,
                coin_id,
                crate::collectors::rollup::ROLLUP_VS_CURRENCY,
                Utc::now(),
            )
            .await
            .map_err(|e| DispatchError::Transient(e.to_string()))?;
            Ok(DispatchOutcome::Done)
        }

        // An unknown (target_kind, kind) pair is a permanent misconfiguration: no amount
        // of retrying will teach the worker a dispatch it does not implement (REQ-SCHED-063.2).
        (target_kind, kind) => Err(DispatchError::Permanent(format!(
            "unknown dispatch: target_kind={target_kind:?} kind={kind:?}"
        ))),
    }
}

// ── Worker loop ───────────────────────────────────────────────────────────────

/// Run the collection-queue worker loop (REQ-SCHED-010/051/050).
///
/// The claim / heartbeat / complete / release lifecycle is the shared
/// [`run_lease_worker`](crate::collectors::lease_worker::run_lease_worker) scaffold
/// (SPEC-REFACTOR-001 M3, F-53b); this function supplies only the collection-queue specifics —
/// the claim query, the heartbeat SQL, the dispatch step, and the terminal transition.
#[allow(clippy::too_many_arguments)]
pub async fn run_collection_queue_worker(
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
    // Claim one collection_queue row (REQ-SCHED-010/014/015).
    let claim = {
        let pool = pool.clone();
        let claimed_by = claimed_by.clone();
        move || -> LeaseFut<Result<Option<ClaimedQueueItem>, sqlx::Error>> {
            let pool = pool.clone();
            let claimed_by = claimed_by.clone();
            Box::pin(async move { claim_queue_item(&pool, &claimed_by, lease_secs).await })
        }
    };

    // Heartbeat one owned item: renews the lease, warns + fences out on a stolen lease
    // (REQ-SCHED-011). Log wording preserved verbatim from the pre-refactor loop.
    let beat = {
        let pool = pool.clone();
        let claimed_by = claimed_by.clone();
        move |id: i64| -> LeaseFut<HeartbeatStep> {
            let pool = pool.clone();
            let claimed_by = claimed_by.clone();
            Box::pin(async move {
                match heartbeat_queue_item(&pool, id, &claimed_by, lease_secs).await {
                    Ok(true) => HeartbeatStep::Renewed,
                    Ok(false) => {
                        warn!("collection_queue_worker: heartbeat fencing fired for item {id}");
                        HeartbeatStep::FencedOut
                    }
                    Err(e) => {
                        error!("collection_queue_worker: heartbeat error for item {id}: {e}");
                        HeartbeatStep::Errored
                    }
                }
            })
        }
    };

    // Dispatch the work (REQ-SCHED-041: all upstream calls acquire pacer OUTSIDE tx).
    let work = {
        let pool = pool.clone();
        let chain = chain.clone();
        let registry = registry.clone();
        move |item: ClaimedQueueItem| -> LeaseFut<(
            ClaimedQueueItem,
            Result<DispatchOutcome, DispatchError>,
        )> {
            let pool = pool.clone();
            let chain = chain.clone();
            let registry = registry.clone();
            Box::pin(async move {
                let result = dispatch_item(&pool, &chain, &item, registry.as_deref()).await;
                (item, result)
            })
        }
    };

    // Terminal transition: complete / release / fail (classification preserved, REQ-REFACTOR-032).
    let finalize = {
        let pool = pool.clone();
        let claimed_by = claimed_by.clone();
        move |item: ClaimedQueueItem,
              result: Result<DispatchOutcome, DispatchError>|
              -> LeaseFut<LeaseCycleOutcome> {
            let pool = pool.clone();
            let claimed_by = claimed_by.clone();
            Box::pin(async move {
                match result {
                    Ok(DispatchOutcome::Done) => {
                        // Success: mark done (REQ-SCHED-012).
                        if let Err(e) = complete_queue_item(&pool, item.id, &claimed_by).await {
                            error!(
                                "collection_queue_worker: complete error for item {}: {e}",
                                item.id
                            );
                        }
                        info!("collection_queue_worker: item {} done", item.id);
                        LeaseCycleOutcome::Continue
                    }
                    Ok(DispatchOutcome::SoftSkip) => {
                        // Pacer backpressure: administratively release WITHOUT consuming the retry
                        // budget and WITHOUT overwriting a prior genuine last_error (REQ-SCHED-060.2/061/065.1).
                        if let Err(e) = release_queue_item(&pool, item.id, &claimed_by).await {
                            error!(
                                "collection_queue_worker: skip-release error for item {}: {e}",
                                item.id
                            );
                        }
                        LeaseCycleOutcome::PauseBeforeNextClaim
                    }
                    Err(DispatchError::Permanent(msg)) => {
                        // Terminal: fail immediately on the first attempt, independent of the retry
                        // budget (REQ-SCHED-063.2). No pause — the next claim is a different item.
                        warn!(
                            "collection_queue_worker: item {} permanently failed: {msg}",
                            item.id
                        );
                        if let Err(db_err) =
                            fail_permanent_queue_item(&pool, item.id, &claimed_by, &msg).await
                        {
                            error!(
                                "collection_queue_worker: permanent-fail update error for item {}: {db_err}",
                                item.id
                            );
                        }
                        LeaseCycleOutcome::Continue
                    }
                    Err(DispatchError::Transient(msg)) => {
                        // Retryable: increment counts (attempts already +1 at claim), retry or fail
                        // at max_attempts (REQ-SCHED-013/063.3), then pause (REQ-SCHED-062).
                        warn!(
                            "collection_queue_worker: item {} failed (attempts={}/{}): {msg}",
                            item.id, item.attempts, max_attempts
                        );
                        if let Err(db_err) = fail_or_retry_queue_item(
                            &pool,
                            item.id,
                            &claimed_by,
                            max_attempts,
                            &msg,
                        )
                        .await
                        {
                            error!(
                                "collection_queue_worker: fail update error for item {}: {db_err}",
                                item.id
                            );
                        }
                        LeaseCycleOutcome::PauseBeforeNextClaim
                    }
                }
            })
        }
    };

    run_lease_worker(
        "collection_queue_worker",
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

    // ── Scenario 7 / REQ-SCHED-013: retry vs permanent-fail logic ────────────

    #[test]
    fn should_retry_when_below_max_attempts() {
        assert!(should_retry(0, 5));
        assert!(should_retry(1, 5));
        assert!(should_retry(4, 5));
    }

    #[test]
    fn should_not_retry_when_at_max_attempts() {
        assert!(!should_retry(5, 5));
        assert!(!should_retry(6, 5));
    }

    #[test]
    fn should_not_retry_at_max_attempts_of_one() {
        assert!(!should_retry(1, 1));
    }

    // ── Pacer skip classification (REQ-SCHED-041) ─────────────────────────────

    #[test]
    fn cooldown_triggers_queue_skip() {
        let err = AcquireSlotError::Cooldown("coingecko".to_string(), chrono::Utc::now());
        assert!(
            pacer_should_skip_queue(&err),
            "cooldown must trigger soft skip in queue worker"
        );
    }

    #[test]
    fn credit_exhausted_triggers_queue_skip() {
        let err = AcquireSlotError::CreditExhausted("coingecko".to_string());
        assert!(
            pacer_should_skip_queue(&err),
            "credit exhaustion must trigger soft skip in queue worker"
        );
    }

    #[test]
    fn not_found_does_not_trigger_skip() {
        let err = AcquireSlotError::NotFound("unknown".to_string());
        assert!(
            !pacer_should_skip_queue(&err),
            "NotFound must not trigger skip"
        );
    }

    // ── SQL-shape assertions ──────────────────────────────────────────────────

    #[test]
    fn claim_sql_uses_skip_locked() {
        assert!(
            CLAIM_QUEUE_SQL.contains("FOR UPDATE SKIP LOCKED"),
            "claim SQL must use FOR UPDATE SKIP LOCKED (REQ-SCHED-015)"
        );
    }

    #[test]
    fn claim_sql_includes_lease_expired_predicate() {
        assert!(
            CLAIM_QUEUE_SQL.contains("lease_expires_at < now()"),
            "claim SQL must include lease-expired predicate (REQ-SCHED-014)"
        );
    }

    #[test]
    fn claim_sql_includes_pending_predicate() {
        assert!(
            CLAIM_QUEUE_SQL.contains("status = 'pending'"),
            "claim SQL must include pending status predicate"
        );
    }

    #[test]
    fn claim_sql_orders_oldest_first() {
        assert!(
            CLAIM_QUEUE_SQL.contains("ORDER BY enqueued_at"),
            "claim SQL must order by enqueued_at for oldest-first fairness"
        );
    }

    #[test]
    fn claim_sql_increments_attempts() {
        assert!(
            CLAIM_QUEUE_SQL.contains("attempts + 1"),
            "claim SQL must increment attempts at claim time"
        );
    }

    #[test]
    fn claim_sql_limits_one() {
        assert!(
            CLAIM_QUEUE_SQL.contains("LIMIT 1"),
            "claim SQL must LIMIT 1 to claim exactly one item"
        );
    }

    #[test]
    fn claim_sql_sets_claimed_by_and_lease() {
        assert!(CLAIM_QUEUE_SQL.contains("claimed_by"));
        assert!(CLAIM_QUEUE_SQL.contains("lease_expires_at"));
    }

    #[test]
    fn heartbeat_sql_uses_fencing_guard() {
        assert!(
            HEARTBEAT_QUEUE_SQL.contains("AND claimed_by = $3"),
            "heartbeat SQL must use claimed_by fencing guard"
        );
    }

    #[test]
    fn complete_sql_uses_fencing_guard() {
        assert!(
            COMPLETE_QUEUE_SQL.contains("AND claimed_by = $2"),
            "complete SQL must use claimed_by fencing guard"
        );
    }

    #[test]
    fn fail_or_retry_sql_uses_conditional_status() {
        assert!(
            FAIL_OR_RETRY_QUEUE_SQL
                .contains("CASE WHEN attempts >= $3 THEN 'failed' ELSE 'pending' END"),
            "fail-or-retry SQL must conditionally set failed vs pending (REQ-SCHED-013)"
        );
    }

    // ── SPEC-SCHED-002: administrative-release + permanent-fail SQL shape ─────

    #[test]
    fn release_sql_neutralizes_claim_increment() {
        // The non-failure release must decrement attempts to cancel the claim-time +1
        // (F-01 root-cause fix, REQ-SCHED-060).
        assert!(
            RELEASE_QUEUE_SQL.contains("attempts         = GREATEST(attempts - 1, 0)"),
            "release SQL must neutralize the claim-time attempts+1 (REQ-SCHED-060)"
        );
    }

    #[test]
    fn release_sql_resets_pending_and_clears_last_error() {
        assert!(
            RELEASE_QUEUE_SQL.contains("status           = 'pending'"),
            "release SQL must reset the row to pending"
        );
        assert!(
            RELEASE_QUEUE_SQL.contains("last_error       = NULL"),
            "release SQL must write NULL (not 'pacer_skip') to last_error (REQ-SCHED-065.1)"
        );
    }

    #[test]
    fn release_sql_uses_fencing_guard() {
        assert!(
            RELEASE_QUEUE_SQL.contains("AND claimed_by = $2"),
            "release SQL must preserve the claimed_by fence"
        );
    }

    #[test]
    fn permanent_fail_sql_is_unconditional_failed() {
        // A permanent failure is terminal regardless of attempts (REQ-SCHED-063.2): no
        // CASE/attempts comparison, an unconditional status = 'failed'.
        assert!(
            FAIL_PERMANENT_QUEUE_SQL.contains("status           = 'failed'"),
            "permanent-fail SQL must set status = 'failed' unconditionally (REQ-SCHED-063.2)"
        );
        assert!(
            !FAIL_PERMANENT_QUEUE_SQL.contains("attempts"),
            "permanent-fail SQL must not depend on the attempts budget (REQ-SCHED-063.2)"
        );
        assert!(
            FAIL_PERMANENT_QUEUE_SQL.contains("AND claimed_by = $2"),
            "permanent-fail SQL must preserve the claimed_by fence"
        );
    }

    // ── AC-REFACTOR-030a / AC-SCHED-065c: delegation to the shared lease-queue scaffold ──
    // After the M3 extraction (F-53b) the claim/heartbeat/complete/release loop — including the
    // guarded shutdown select! arms (REQ-SCHED-065.3) and the watch-based heartbeat stop
    // (REQ-REFACTOR-031) — lives in `lease_worker::run_lease_worker`. The guard invariant is
    // now behavior-verified there (lease_worker::tests::scaffold_guards_dropped_sender +
    // heartbeat_stops_via_watch_signal_not_abort); this worker must merely delegate to it.

    #[test]
    fn worker_delegates_to_shared_lease_scaffold() {
        let src = std::fs::read_to_string("src/collectors/collection_queue.rs")
            .expect("read collection_queue.rs");
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

    // ── DispatchOutcome / DispatchError classification (in-process) ───────────

    #[test]
    fn dispatch_outcome_variants_are_distinct() {
        assert_ne!(DispatchOutcome::Done, DispatchOutcome::SoftSkip);
    }

    #[test]
    fn enqueue_sql_uses_on_conflict_do_nothing() {
        assert!(
            ENQUEUE_QUEUE_SQL.contains("ON CONFLICT DO NOTHING"),
            "enqueue SQL must use ON CONFLICT DO NOTHING for idempotency (REQ-SCHED-030)"
        );
    }

    // ── DB-gated integration tests (require live DATABASE_URL) ────────────────
    // These MUST run with `--test-threads=1`. `claim_queue_item` selects the
    // globally-oldest pending item (`ORDER BY enqueued_at LIMIT 1 FOR UPDATE SKIP
    // LOCKED`), so tests running concurrently against the shared DB would steal each
    // other's pending rows and flake. See CLAUDE.md § Integration Tests.

    /// Scenario 6/7 / REQ-SCHED-010/011/012/013: claim, heartbeat, complete cycle.
    #[tokio::test]
    #[ignore]
    async fn db_claim_heartbeat_complete_cycle() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
        let pool = crate::db::connect(&url).await.expect("connect");

        // Insert a test pending item.
        sqlx::query(
            "INSERT INTO collection_queue \
             (target_kind, target_id, kind, status, enqueued_at, updated_at) \
             VALUES ('coin', 'test-btc', 'metadata', 'pending', now(), now())",
        )
        .execute(&pool)
        .await
        .expect("insert test item");

        // Claim it.
        let item = claim_queue_item(&pool, "test-replica-1", 120)
            .await
            .expect("claim")
            .expect("should find item");

        assert_eq!(item.target_id, "test-btc");
        assert_eq!(item.kind, "metadata");
        assert_eq!(item.status, "claimed");
        assert_eq!(item.attempts, 1);

        // Heartbeat.
        let renewed = heartbeat_queue_item(&pool, item.id, "test-replica-1", 120)
            .await
            .expect("heartbeat");
        assert!(renewed, "heartbeat must succeed");

        // Complete.
        complete_queue_item(&pool, item.id, "test-replica-1")
            .await
            .expect("complete");

        let status: String =
            sqlx::query_scalar("SELECT status FROM collection_queue WHERE id = $1")
                .bind(item.id)
                .fetch_one(&pool)
                .await
                .expect("fetch status");

        assert_eq!(status, "done");

        // Cleanup.
        sqlx::query("DELETE FROM collection_queue WHERE id = $1")
            .bind(item.id)
            .execute(&pool)
            .await
            .expect("cleanup");
    }

    /// Scenario 6 / REQ-SCHED-014: lease-expired row is re-claimable.
    #[tokio::test]
    #[ignore]
    async fn db_lease_expired_row_is_reclaimable() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
        let pool = crate::db::connect(&url).await.expect("connect");

        // Insert an already-claimed item with an expired lease.
        let item_id: i64 = sqlx::query_scalar(
            "INSERT INTO collection_queue \
             (target_kind, target_id, kind, status, claimed_by, \
              lease_expires_at, attempts, enqueued_at, updated_at) \
             VALUES ('coin', 'test-eth', 'market', 'claimed', 'dead-replica', \
                     now() - INTERVAL '5 minutes', 1, now(), now()) \
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("insert stale item");

        // A new replica should be able to claim it.
        let item = claim_queue_item(&pool, "test-replica-2", 120)
            .await
            .expect("claim")
            .expect("should find lease-expired item");

        assert_eq!(item.id, item_id);
        assert_eq!(
            item.claimed_by.as_deref(),
            Some("test-replica-2"),
            "new replica must own the re-claimed item"
        );

        // Cleanup.
        sqlx::query("DELETE FROM collection_queue WHERE id = $1")
            .bind(item_id)
            .execute(&pool)
            .await
            .expect("cleanup");
    }

    /// AC-SCHED-060a / REQ-SCHED-060.2: soft-skip releases (walking the queue under pacer
    /// backpressure) more than `max_attempts` times must NOT consume the retry budget — one
    /// subsequent genuine transient failure leaves the item `pending`, not `failed`.
    #[tokio::test]
    #[ignore]
    async fn db_soft_skip_release_does_not_consume_retry_budget() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
        let pool = crate::db::connect(&url).await.expect("connect");
        let max_attempts = 5;

        let item_id: i64 = sqlx::query_scalar(
            "INSERT INTO collection_queue \
             (target_kind, target_id, kind, status, enqueued_at, updated_at) \
             VALUES ('coin', 'test-f01-soft-skip', 'metadata', 'pending', now(), now()) \
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("insert item");

        // Walk the queue under backpressure > max_attempts times: each iteration claims
        // (attempts += 1) then administratively releases (attempts -= 1, net zero).
        for _ in 0..(max_attempts + 3) {
            let claimed = claim_queue_item(&pool, "test-replica", 120)
                .await
                .expect("claim")
                .expect("should re-claim the released item");
            assert_eq!(claimed.id, item_id);
            release_queue_item(&pool, item_id, "test-replica")
                .await
                .expect("soft-skip release");
        }

        // Effective attempts must be back at 0 after all the neutralized releases.
        let attempts_after_walk: i32 =
            sqlx::query_scalar("SELECT attempts FROM collection_queue WHERE id = $1")
                .bind(item_id)
                .fetch_one(&pool)
                .await
                .expect("fetch attempts");
        assert_eq!(
            attempts_after_walk, 0,
            "soft-skip walk must leave the effective attempt count unchanged (REQ-SCHED-060)"
        );

        // Now exactly ONE genuine transient failure: claim (attempts→1) then fail_or_retry.
        let claimed = claim_queue_item(&pool, "test-replica", 120)
            .await
            .expect("claim")
            .expect("claim for genuine failure");
        assert_eq!(claimed.attempts, 1, "first genuine attempt is attempt #1");
        fail_or_retry_queue_item(&pool, item_id, "test-replica", max_attempts, "boom")
            .await
            .expect("genuine transient failure");

        let status: String =
            sqlx::query_scalar("SELECT status FROM collection_queue WHERE id = $1")
                .bind(item_id)
                .fetch_one(&pool)
                .await
                .expect("fetch status");
        assert_eq!(
            status, "pending",
            "one genuine failure after many soft-skips must stay pending, NOT failed (F-01)"
        );

        sqlx::query("DELETE FROM collection_queue WHERE id = $1")
            .bind(item_id)
            .execute(&pool)
            .await
            .expect("cleanup");
    }

    /// AC-SCHED-063a / REQ-SCHED-063.2: a permanent dispatch error (unknown coin) fails the
    /// item on the FIRST attempt with a descriptive `last_error`, never exhausting retries.
    #[tokio::test]
    #[ignore]
    async fn db_permanent_dispatch_fails_fast() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
        let pool = crate::db::connect(&url).await.expect("connect");

        // A coin_id guaranteed absent from tracked_coins → fetch_coin_context returns None.
        let coin = "test-f04-unknown-coin";
        sqlx::query("DELETE FROM tracked_coins WHERE coin_id = $1")
            .bind(coin)
            .execute(&pool)
            .await
            .expect("ensure coin absent");

        let item_id: i64 = sqlx::query_scalar(
            "INSERT INTO collection_queue \
             (target_kind, target_id, kind, status, enqueued_at, updated_at) \
             VALUES ('coin', $1, 'candles', 'pending', now(), now()) RETURNING id",
        )
        .bind(coin)
        .fetch_one(&pool)
        .await
        .expect("insert item");

        let item = claim_queue_item(&pool, "test-replica", 120)
            .await
            .expect("claim")
            .expect("should claim");
        assert_eq!(
            item.attempts, 1,
            "permanent failure occurs on the first attempt"
        );

        // Dispatch with an empty chain: the candles arm looks up the coin FIRST, so it
        // classifies as Permanent before ever needing a provider or the pacer.
        let empty_chain: Vec<Arc<dyn Provider>> = vec![];
        let result = dispatch_item(&pool, &empty_chain, &item, None).await;
        let msg = match result {
            Err(DispatchError::Permanent(m)) => m,
            other => panic!("expected Permanent, got {other:?}"),
        };
        assert!(
            msg.contains("not found"),
            "descriptive last_error expected: {msg}"
        );

        fail_permanent_queue_item(&pool, item.id, "test-replica", &msg)
            .await
            .expect("permanent fail");

        let (status, attempts, last_error): (String, i32, Option<String>) = sqlx::query_as(
            "SELECT status, attempts, last_error FROM collection_queue WHERE id = $1",
        )
        .bind(item_id)
        .fetch_one(&pool)
        .await
        .expect("fetch row");
        assert_eq!(
            status, "failed",
            "permanent error fails on the first attempt"
        );
        assert_eq!(
            attempts, 1,
            "retries were NOT exhausted (attempts stays at 1)"
        );
        assert!(
            last_error.as_deref().unwrap_or("").contains("not found"),
            "last_error must be descriptive (REQ-SCHED-063.2)"
        );

        sqlx::query("DELETE FROM collection_queue WHERE id = $1")
            .bind(item_id)
            .execute(&pool)
            .await
            .expect("cleanup");
    }

    /// AC-SCHED-063b / REQ-SCHED-063.3: a single transient failure resets the item to
    /// `pending` for retry — it is not failed on the first transient error.
    #[tokio::test]
    #[ignore]
    async fn db_transient_failure_retries_not_fails() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
        let pool = crate::db::connect(&url).await.expect("connect");

        let item_id: i64 = sqlx::query_scalar(
            "INSERT INTO collection_queue \
             (target_kind, target_id, kind, status, enqueued_at, updated_at) \
             VALUES ('coin', 'test-f04-transient', 'metadata', 'pending', now(), now()) \
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("insert item");

        let item = claim_queue_item(&pool, "test-replica", 120)
            .await
            .expect("claim")
            .expect("claim");
        assert_eq!(item.attempts, 1);

        fail_or_retry_queue_item(&pool, item_id, "test-replica", 5, "network blip")
            .await
            .expect("transient failure");

        let status: String =
            sqlx::query_scalar("SELECT status FROM collection_queue WHERE id = $1")
                .bind(item_id)
                .fetch_one(&pool)
                .await
                .expect("fetch status");
        assert_eq!(
            status, "pending",
            "a single transient failure must reset to pending, not fail (REQ-SCHED-063.3)"
        );

        sqlx::query("DELETE FROM collection_queue WHERE id = $1")
            .bind(item_id)
            .execute(&pool)
            .await
            .expect("cleanup");
    }
}
