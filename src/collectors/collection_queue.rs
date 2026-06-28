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
//! # Attempt counting
//!
//! `attempts` is incremented at claim time. On transient failure the row is released
//! for retry (`status = 'pending'`); on permanent failure or `attempts >= max_attempts`
//! the row is marked `'failed'` (REQ-SCHED-013).

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tracing::{error, info, warn};

use crate::db::upserts::{
    upsert_candles, upsert_coin_market_snapshot, upsert_coin_metadata, upsert_derivatives_quote,
    upsert_live_quote,
};
use crate::pacer::{acquire_slot, AcquireSlotError};
use crate::providers::{
    Capability, CoinMarket, CoinMeta, DerivTick, MarketQuery, OhlcCandle, Provider, ProviderError,
    SpotQuote,
};

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
/// Increments `attempts` at claim time to bound retries (REQ-SCHED-013).
///
// @MX:ANCHOR: [AUTO] CLAIM_QUEUE_SQL — FOR UPDATE SKIP LOCKED single-owner invariant
// @MX:REASON: fan_in >= 3: claim_queue_item(), SQL-shape tests, DB integration tests.
//             REQ-SCHED-015: SKIP LOCKED + lease = at-most-one replica per row at a time.
//             REQ-SCHED-014: lease-expired predicate allows crash-recovery re-claim.
// @MX:SPEC: SPEC-SCHED-001 REQ-SCHED-010 REQ-SCHED-011 REQ-SCHED-014 REQ-SCHED-015
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

/// Failure UPDATE: increment attempts; mark `failed` at max, else reset to `pending` (REQ-SCHED-013).
pub const FAIL_OR_RETRY_QUEUE_SQL: &str = "\
    UPDATE collection_queue SET \
        status           = CASE WHEN attempts >= $3 THEN 'failed' ELSE 'pending' END, \
        last_error       = $4, \
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

/// Market context needed to build a `MarketQuery` for provider dispatch.
#[derive(Debug)]
struct MarketInfo {
    id: i64,
    coin_id: Option<String>,
    base: String,
    quote: String,
    venue: Option<String>,
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

/// Handle a work failure: retry if under max_attempts, else permanently fail (REQ-SCHED-013).
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

// ── Market context lookup ─────────────────────────────────────────────────────

async fn fetch_market_info(pool: &PgPool, market_id: i64) -> Result<Option<MarketInfo>> {
    type MarketRow = (i64, Option<String>, String, String, Option<String>);
    let row: Option<MarketRow> =
        sqlx::query_as("SELECT id, coin_id, base, quote, venue FROM tracked_markets WHERE id = $1")
            .bind(market_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(id, coin_id, base, quote, venue)| MarketInfo {
        id,
        coin_id,
        base,
        quote,
        venue,
    }))
}

// ── Chain dispatch helpers ────────────────────────────────────────────────────

/// Try providers in order for `fetch_ohlc`; return first success.
async fn chain_fetch_ohlc_local(
    chain: &[Arc<dyn Provider>],
    market: &MarketQuery,
    days: u32,
) -> Result<Vec<OhlcCandle>, ProviderError> {
    let (result, _) = crate::providers::chain_fetch_ohlc(chain, market, days).await;
    result
}

async fn chain_fetch_spot_local(
    chain: &[Arc<dyn Provider>],
    market: &MarketQuery,
) -> Result<SpotQuote, ProviderError> {
    let mut last_err = ProviderError::Other(anyhow::anyhow!("empty chain"));
    for p in chain {
        if !p.supports(Capability::Spot) {
            continue;
        }
        match p.fetch_spot(market).await {
            Ok(q) => return Ok(q),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

async fn chain_fetch_coin_metadata(
    chain: &[Arc<dyn Provider>],
    coin_id: &str,
) -> Result<CoinMeta, ProviderError> {
    let mut last_err = ProviderError::Other(anyhow::anyhow!("empty chain"));
    for p in chain {
        if !p.supports(Capability::CoinMetadata) {
            continue;
        }
        match p.fetch_coin_metadata(coin_id).await {
            Ok(m) => return Ok(m),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

async fn chain_fetch_coin_market(
    chain: &[Arc<dyn Provider>],
    coin_id: &str,
    vs_currency: &str,
) -> Result<CoinMarket, ProviderError> {
    let mut last_err = ProviderError::Other(anyhow::anyhow!("empty chain"));
    for p in chain {
        if !p.supports(Capability::CoinMarket) {
            continue;
        }
        match p.fetch_coin_market(coin_id, vs_currency).await {
            Ok(m) => return Ok(m),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

async fn chain_fetch_derivatives(
    chain: &[Arc<dyn Provider>],
    market: &MarketQuery,
) -> Result<DerivTick, ProviderError> {
    let mut last_err = ProviderError::Other(anyhow::anyhow!("empty chain"));
    for p in chain {
        if !p.supports(Capability::Derivatives) {
            continue;
        }
        match p.fetch_derivatives(market).await {
            Ok(d) => return Ok(d),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

/// Find the first provider supporting `cap`; return its name for pacer pacing.
fn first_provider_for_cap(chain: &[Arc<dyn Provider>], cap: Capability) -> Option<String> {
    chain
        .iter()
        .find(|p| p.supports(cap))
        .map(|p| p.name().to_string())
}

// ── Worker dispatch ───────────────────────────────────────────────────────────

/// Dispatch one claimed queue item to its collector and upsert the result.
///
/// Returns `Ok(true)` on success, `Ok(false)` on transient failure (pacer skip or soft error),
/// `Err(e)` on a hard dispatch error that should increment attempts.
async fn dispatch_item(
    pool: &PgPool,
    chain: &[Arc<dyn Provider>],
    item: &ClaimedQueueItem,
) -> Result<bool, String> {
    match (item.target_kind.as_str(), item.kind.as_str()) {
        ("market", "candles") => {
            let market_id: i64 = item
                .target_id
                .parse()
                .map_err(|_| format!("invalid market_id: {}", item.target_id))?;

            let info = fetch_market_info(pool, market_id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("market {market_id} not found"))?;

            let cap = Capability::Ohlc;
            let provider_name = match first_provider_for_cap(chain, cap) {
                Some(n) => n,
                None => return Err("no provider supports OHLC".to_string()),
            };

            match acquire_slot(pool, &provider_name).await {
                Err(ref e) if pacer_should_skip_queue(e) => {
                    warn!("queue_worker: pacer skip for item {}: {e}", item.id);
                    return Ok(false); // soft skip, no attempt increment
                }
                Err(e) => return Err(format!("pacer: {e}")),
                Ok(()) => {}
            }

            let mq = MarketQuery {
                market_id: info.id,
                coin_id: info.coin_id,
                base: info.base.clone(),
                quote: info.quote.clone(),
                venue: info.venue,
                vs_currency: info.quote.to_lowercase(),
            };

            let candles = chain_fetch_ohlc_local(chain, &mq, 7)
                .await
                .map_err(|e| e.to_string())?;

            upsert_candles(pool, &candles)
                .await
                .map_err(|e| e.to_string())?;

            Ok(true)
        }

        ("market", "spot") => {
            let market_id: i64 = item
                .target_id
                .parse()
                .map_err(|_| format!("invalid market_id: {}", item.target_id))?;

            let info = fetch_market_info(pool, market_id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("market {market_id} not found"))?;

            let cap = Capability::Spot;
            let provider_name = match first_provider_for_cap(chain, cap) {
                Some(n) => n,
                None => return Err("no provider supports Spot".to_string()),
            };

            match acquire_slot(pool, &provider_name).await {
                Err(ref e) if pacer_should_skip_queue(e) => {
                    warn!("queue_worker: pacer skip for item {}: {e}", item.id);
                    return Ok(false);
                }
                Err(e) => return Err(format!("pacer: {e}")),
                Ok(()) => {}
            }

            let mq = MarketQuery {
                market_id: info.id,
                coin_id: info.coin_id,
                base: info.base.clone(),
                quote: info.quote.clone(),
                venue: info.venue,
                vs_currency: info.quote.to_lowercase(),
            };

            let quote = chain_fetch_spot_local(chain, &mq)
                .await
                .map_err(|e| e.to_string())?;

            upsert_live_quote(pool, &quote)
                .await
                .map_err(|e| e.to_string())?;

            Ok(true)
        }

        ("market", "derivatives") => {
            let market_id: i64 = item
                .target_id
                .parse()
                .map_err(|_| format!("invalid market_id: {}", item.target_id))?;

            let info = fetch_market_info(pool, market_id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("market {market_id} not found"))?;

            let cap = Capability::Derivatives;
            let provider_name = match first_provider_for_cap(chain, cap) {
                Some(n) => n,
                None => return Err("no provider supports Derivatives".to_string()),
            };

            match acquire_slot(pool, &provider_name).await {
                Err(ref e) if pacer_should_skip_queue(e) => {
                    warn!("queue_worker: pacer skip for item {}: {e}", item.id);
                    return Ok(false);
                }
                Err(e) => return Err(format!("pacer: {e}")),
                Ok(()) => {}
            }

            let mq = MarketQuery {
                market_id: info.id,
                coin_id: info.coin_id,
                base: info.base.clone(),
                quote: info.quote.clone(),
                venue: info.venue,
                vs_currency: info.quote.to_lowercase(),
            };

            let tick = chain_fetch_derivatives(chain, &mq)
                .await
                .map_err(|e| e.to_string())?;

            upsert_derivatives_quote(pool, &tick)
                .await
                .map_err(|e| e.to_string())?;

            Ok(true)
        }

        ("coin", "metadata") => {
            let coin_id = &item.target_id;

            let cap = Capability::CoinMetadata;
            let provider_name = match first_provider_for_cap(chain, cap) {
                Some(n) => n,
                None => return Err("no provider supports CoinMetadata".to_string()),
            };

            match acquire_slot(pool, &provider_name).await {
                Err(ref e) if pacer_should_skip_queue(e) => {
                    warn!("queue_worker: pacer skip for item {}: {e}", item.id);
                    return Ok(false);
                }
                Err(e) => return Err(format!("pacer: {e}")),
                Ok(()) => {}
            }

            let meta = chain_fetch_coin_metadata(chain, coin_id)
                .await
                .map_err(|e| e.to_string())?;

            // Revision upsert (REQ-SCHED-042): new revision only if values changed.
            upsert_coin_metadata(pool, &meta)
                .await
                .map_err(|e| e.to_string())?;

            Ok(true)
        }

        ("coin", "market") => {
            let coin_id = &item.target_id;

            let cap = Capability::CoinMarket;
            let provider_name = match first_provider_for_cap(chain, cap) {
                Some(n) => n,
                None => return Err("no provider supports CoinMarket".to_string()),
            };

            match acquire_slot(pool, &provider_name).await {
                Err(ref e) if pacer_should_skip_queue(e) => {
                    warn!("queue_worker: pacer skip for item {}: {e}", item.id);
                    return Ok(false);
                }
                Err(e) => return Err(format!("pacer: {e}")),
                Ok(()) => {}
            }

            let snapshot = chain_fetch_coin_market(chain, coin_id, "usd")
                .await
                .map_err(|e| e.to_string())?;

            upsert_coin_market_snapshot(pool, &snapshot)
                .await
                .map_err(|e| e.to_string())?;

            Ok(true)
        }

        (target_kind, kind) => Err(format!(
            "unknown dispatch: target_kind={target_kind:?} kind={kind:?}"
        )),
    }
}

// ── Worker loop ───────────────────────────────────────────────────────────────

/// Run the collection-queue worker loop (REQ-SCHED-010/051/050).
#[allow(clippy::too_many_arguments)]
pub async fn run_collection_queue_worker(
    pool: PgPool,
    chain: Arc<Vec<Arc<dyn Provider>>>,
    claimed_by: String,
    lease_secs: i64,
    heartbeat_interval_secs: u64,
    max_attempts: i32,
    idle_sleep: StdDuration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    info!("collection_queue_worker: started (replica={claimed_by})");

    loop {
        if *shutdown.borrow() {
            break;
        }

        let item = match claim_queue_item(&pool, &claimed_by, lease_secs).await {
            Ok(Some(i)) => i,
            Ok(None) => {
                // Queue empty: idle until next check or shutdown signal.
                tokio::select! {
                    _ = shutdown.changed() => {}
                    _ = tokio::time::sleep(idle_sleep) => {}
                }
                continue;
            }
            Err(e) => {
                error!("collection_queue_worker: claim error: {e}");
                tokio::time::sleep(StdDuration::from_secs(1)).await;
                continue;
            }
        };

        let item_id = item.id;
        let claimed_by_clone = claimed_by.clone();
        let pool_hb = pool.clone();
        let lease_clone = lease_secs;

        // Heartbeat task: renews lease periodically while work runs (REQ-SCHED-011).
        let hb_handle = tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(StdDuration::from_secs(heartbeat_interval_secs));
            loop {
                interval.tick().await;
                match heartbeat_queue_item(&pool_hb, item_id, &claimed_by_clone, lease_clone).await
                {
                    Ok(true) => {} // renewed
                    Ok(false) => {
                        warn!(
                            "collection_queue_worker: heartbeat fencing fired for item {item_id}"
                        );
                        break;
                    }
                    Err(e) => {
                        error!("collection_queue_worker: heartbeat error for item {item_id}: {e}");
                    }
                }
            }
        });

        // Dispatch the work (REQ-SCHED-041: all upstream calls acquire pacer OUTSIDE tx).
        let dispatch_result = dispatch_item(&pool, &chain, &item).await;

        hb_handle.abort();

        match dispatch_result {
            Ok(true) => {
                // Success: mark done (REQ-SCHED-012).
                if let Err(e) = complete_queue_item(&pool, item.id, &claimed_by).await {
                    error!(
                        "collection_queue_worker: complete error for item {}: {e}",
                        item.id
                    );
                }
                info!("collection_queue_worker: item {} done", item.id);
            }
            Ok(false) => {
                // Soft skip (pacer): release for retry without incrementing attempts further.
                // The item was claimed (attempts already incremented at claim time).
                // Reset to pending so it gets picked up next cycle.
                if let Err(e) = fail_or_retry_queue_item(
                    &pool,
                    item.id,
                    &claimed_by,
                    i32::MAX, // never mark failed on a skip
                    "pacer_skip",
                )
                .await
                {
                    error!(
                        "collection_queue_worker: skip-release error for item {}: {e}",
                        item.id
                    );
                }
            }
            Err(e) => {
                // Hard error: increment attempts, retry or fail (REQ-SCHED-013).
                warn!(
                    "collection_queue_worker: item {} failed (attempts={}/{}): {e}",
                    item.id, item.attempts, max_attempts
                );
                if let Err(db_err) =
                    fail_or_retry_queue_item(&pool, item.id, &claimed_by, max_attempts, &e).await
                {
                    error!(
                        "collection_queue_worker: fail update error for item {}: {db_err}",
                        item.id
                    );
                }
            }
        }
    }

    info!("collection_queue_worker: stopped");
    Ok(())
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

    #[test]
    fn enqueue_sql_uses_on_conflict_do_nothing() {
        assert!(
            ENQUEUE_QUEUE_SQL.contains("ON CONFLICT DO NOTHING"),
            "enqueue SQL must use ON CONFLICT DO NOTHING for idempotency (REQ-SCHED-030)"
        );
    }

    // ── DB-gated integration tests (require live DATABASE_URL) ────────────────

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
}
