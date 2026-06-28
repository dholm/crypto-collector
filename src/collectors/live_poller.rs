//! Live-quote poller worker (SPEC-SCHED-001 REQ-SCHED-001..008).
//!
//! # Short-transaction discipline (REQ-SCHED-003/004)
//!
//! The claim transaction:
//! 1. SELECTs due + not-in-flight active markets with `FOR UPDATE SKIP LOCKED`.
//! 2. Sets `live_poll_claimed_until = now() + claim_ttl` (the marker, NOT `last_polled_at`).
//! 3. COMMITs — row locks released BEFORE any network call.
//!
//! After the claim tx commits, the worker iterates the returned market IDs:
//! - Acquires a pacer slot (outside any transaction).
//! - Fetches via the provider chain (outside any transaction).
//! - On success: INSERT live_quotes + UPDATE last_polled_at, clear marker.
//! - On transient failure: clear marker only (market immediately due next cycle).
//!
//! A crashed replica's marker self-expires at `claim_ttl` (REQ-SCHED-007).

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tracing::{error, info, warn};

use crate::db::upserts::upsert_live_quote;
use crate::pacer::{acquire_slot, AcquireSlotError};
use crate::providers::{Capability, MarketQuery, Provider, ProviderError};

// ── Pure scheduling functions (unit-testable, no I/O) ────────────────────────

/// Convert a duration in seconds to a Postgres INTERVAL string accepted by sqlx.
///
/// Example: `secs_to_pg_interval(30)` → `"30 seconds"`.
/// sqlx 0.9 accepts `&str` binds for INTERVAL parameters with the postgres feature.
pub fn secs_to_pg_interval(secs: i64) -> String {
    format!("{secs} seconds")
}

/// Determine whether a market is due for polling (pure, no I/O).
///
/// A market is due when:
/// 1. It is NOT in-flight (`live_poll_claimed_until` is NULL or ≤ `now`).
/// 2. It has never been polled (`last_polled_at` is NULL), OR
///    `last_polled_at + effective_interval ≤ now`.
///
/// The effective interval is `per_market_interval_secs` if present, else `global_interval_secs`.
/// This implements REQ-SCHED-002 (NULL per-market → global default) and the not-in-flight
/// check of REQ-SCHED-003 in pure application code (DB enforces it in the SQL predicate too).
pub fn is_market_due(
    last_polled_at: Option<DateTime<Utc>>,
    live_poll_claimed_until: Option<DateTime<Utc>>,
    global_interval_secs: i64,
    per_market_interval_secs: Option<i64>,
    now: DateTime<Utc>,
) -> bool {
    // Gate 1: not in-flight (REQ-SCHED-007).
    if let Some(until) = live_poll_claimed_until {
        if until > now {
            return false; // another replica owns this market
        }
    }

    // Gate 2: due by cadence (REQ-SCHED-002/003).
    let interval_secs = per_market_interval_secs.unwrap_or(global_interval_secs);
    match last_polled_at {
        None => true, // never polled
        Some(t) => t + Duration::seconds(interval_secs) <= now,
    }
}

/// Classify a pacer error: returns `true` if the worker should skip the fetch
/// (soft skip — no attempts increment) rather than fail the item permanently.
pub fn pacer_should_skip(err: &AcquireSlotError) -> bool {
    matches!(
        err,
        AcquireSlotError::Cooldown(..) | AcquireSlotError::CreditExhausted(..)
    )
}

// ── SQL constants (SQL-shape tested below) ────────────────────────────────────

/// Claim SQL for the live-quote poll loop (REQ-SCHED-003/004/007).
///
/// Uses a CTE to SELECT eligible markets (status='active', due, not-in-flight) with
/// `FOR UPDATE SKIP LOCKED`, then UPDATEs `live_poll_claimed_until` in one atomic statement.
///
/// $1 = global default cadence as `"<n> seconds"` INTERVAL string (REQ-SCHED-002).
/// $2 = claim TTL as `"<n> seconds"` INTERVAL string (REQ-SCHED-007).
///
/// INVARIANT: sets `live_poll_claimed_until` (the in-flight marker), NOT `last_polled_at`.
/// INVARIANT: this SQL runs inside a short tx that commits BEFORE any provider call.
///
// @MX:ANCHOR: [AUTO] LIVE_POLLER_CLAIM_SQL — due+not-in-flight predicate; marker-on-claim in short tx
// @MX:REASON: fan_in >= 3: claim_due_markets(), SQL-shape tests, DB integration test.
//             REQ-SCHED-003: sets live_poll_claimed_until NOT last_polled_at.
//             REQ-SCHED-004: claim tx commits/releases locks BEFORE any provider network call.
//             REQ-SCHED-007: self-expiring marker = cross-replica in-flight dedup.
// @MX:WARN: [AUTO] do NOT add any provider or network call inside the transaction that runs this SQL
// @MX:REASON: holding a row lock across network I/O serialises all replicas through one DB lock cycle
// @MX:SPEC: SPEC-SCHED-001 REQ-SCHED-003 REQ-SCHED-004 REQ-SCHED-007
pub const LIVE_POLLER_CLAIM_SQL: &str = "\
    WITH claimed AS (\
        SELECT id FROM tracked_markets \
        WHERE status = 'active' \
          AND (last_polled_at IS NULL \
               OR last_polled_at + COALESCE(live_poll_interval, $1::interval) <= now()) \
          AND (live_poll_claimed_until IS NULL OR live_poll_claimed_until <= now()) \
        FOR UPDATE SKIP LOCKED\
    ) \
    UPDATE tracked_markets \
    SET live_poll_claimed_until = now() + $2::interval \
    FROM claimed \
    WHERE tracked_markets.id = claimed.id \
    RETURNING tracked_markets.id";

/// Success UPDATE: advance `last_polled_at` and clear the marker (REQ-SCHED-005).
///
/// INVARIANT: runs OUTSIDE the claim transaction (cursor advances only on success).
///
// @MX:WARN: [AUTO] LIVE_POLLER_SUCCESS_SQL — sets last_polled_at; must run OUTSIDE claim tx
// @MX:REASON: REQ-SCHED-005: last_polled_at advances ONLY after a reached success.
//             REQ-SCHED-006: transient failure must NOT advance last_polled_at (see FAILURE_CLEAR_SQL).
// @MX:SPEC: SPEC-SCHED-001 REQ-SCHED-005
pub const LIVE_POLLER_SUCCESS_SQL: &str = "\
    UPDATE tracked_markets \
    SET last_polled_at = now(), live_poll_claimed_until = NULL \
    WHERE id = $1";

/// Transient-failure UPDATE: clear the marker only, leave `last_polled_at` unchanged (REQ-SCHED-006).
///
/// The market becomes immediately due again on the next cycle (fast retry).
///
// @MX:WARN: [AUTO] LIVE_POLLER_FAILURE_CLEAR_SQL — clears marker WITHOUT advancing last_polled_at
// @MX:REASON: REQ-SCHED-006: transient failure must NOT advance cursor. Clearing the marker
//             makes the market due again next cycle (fast retry). last_polled_at must remain
//             unchanged so the market is re-claimed rather than silently skipped.
// @MX:SPEC: SPEC-SCHED-001 REQ-SCHED-006
pub const LIVE_POLLER_FAILURE_CLEAR_SQL: &str = "\
    UPDATE tracked_markets \
    SET live_poll_claimed_until = NULL \
    WHERE id = $1";

// ── DB functions ──────────────────────────────────────────────────────────────

/// Row returned by the claim query; holds the market context needed to build a
/// `MarketQuery` for the provider call outside the claim transaction.
#[derive(Debug, sqlx::FromRow)]
pub struct ClaimedMarket {
    pub id: i64,
    pub coin_id: Option<String>,
    pub base: String,
    pub quote: String,
    pub venue: Option<String>,
}

/// Claim due, active, not-in-flight markets in a short transaction.
///
/// Sets `live_poll_claimed_until = now() + claim_ttl` (the marker) and commits.
/// Row locks are released by the commit — no connection or tx is held after return.
/// Network I/O MUST NOT be performed before calling this function's result is acted on.
///
/// Returns the market context rows needed for subsequent provider calls.
///
// @MX:ANCHOR: [AUTO] claim_due_markets — short-tx claim; returns before any network I/O
// @MX:REASON: fan_in >= 3: live poller loop, DB integration tests, SQL-shape tests.
//             REQ-SCHED-004: tx.commit() is called before returning the market list.
//             The caller is responsible for keeping all network calls outside any open tx.
// @MX:SPEC: SPEC-SCHED-001 REQ-SCHED-003 REQ-SCHED-004
pub async fn claim_due_markets(
    pool: &PgPool,
    global_interval_secs: i64,
    claim_ttl_secs: i64,
) -> Result<Vec<ClaimedMarket>, sqlx::Error> {
    let global_pg = secs_to_pg_interval(global_interval_secs);
    let ttl_pg = secs_to_pg_interval(claim_ttl_secs);

    // Short transaction: SELECT + UPDATE + COMMIT before returning.
    let mut tx = pool.begin().await?;
    // The full claim SQL sets the marker; we also need market context for the provider call.
    // We run the claim to get IDs, then fetch market context in a separate query.
    let ids: Vec<(i64,)> = sqlx::query_as(LIVE_POLLER_CLAIM_SQL)
        .bind(&global_pg) // $1: global interval
        .bind(&ttl_pg) // $2: claim TTL
        .fetch_all(&mut *tx)
        .await?;
    // COMMIT — row locks released BEFORE any provider call (REQ-SCHED-004).
    tx.commit().await?;

    if ids.is_empty() {
        return Ok(vec![]);
    }

    // Fetch market context outside the claim tx (REQ-SCHED-004: no tx held).
    let market_ids: Vec<i64> = ids.into_iter().map(|(id,)| id).collect();
    // Use ANY($1) to look up all claimed markets in one query.
    let markets: Vec<ClaimedMarket> = sqlx::query_as(
        "SELECT id, coin_id, base, quote, venue \
         FROM tracked_markets \
         WHERE id = ANY($1)",
    )
    .bind(&market_ids)
    .fetch_all(pool)
    .await?;

    Ok(markets)
}

/// Mark a successful poll: advance `last_polled_at`, clear the in-flight marker.
pub async fn mark_poll_success(pool: &PgPool, market_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(LIVE_POLLER_SUCCESS_SQL)
        .bind(market_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Clear the in-flight marker after a transient failure (leaves `last_polled_at` unchanged).
pub async fn clear_poll_marker(pool: &PgPool, market_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(LIVE_POLLER_FAILURE_CLEAR_SQL)
        .bind(market_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ── Worker loop ───────────────────────────────────────────────────────────────

/// Run the live-quote poll loop until `shutdown` is signalled (REQ-SCHED-001/008/050).
///
/// No calendar, market-hours, or phase gate — collection is continuous (REQ-SCHED-008).
/// Each tick claims due markets in a short tx, then paces + fetches outside the tx.
pub async fn run_live_poller(
    pool: PgPool,
    chain: Arc<Vec<Arc<dyn Provider>>>,
    global_interval_secs: i64,
    claim_ttl_secs: i64,
    tick_interval: StdDuration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    info!(
        "live_poller: started (interval={}s, claim_ttl={}s)",
        global_interval_secs, claim_ttl_secs
    );

    let mut ticker = tokio::time::interval(tick_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("live_poller: shutdown signal received");
                    break;
                }
            }
            _ = ticker.tick() => {
                if let Err(e) = poll_cycle(&pool, &chain, global_interval_secs, claim_ttl_secs).await {
                    error!("live_poller: cycle error: {e}");
                }
            }
        }
    }

    info!("live_poller: stopped");
    Ok(())
}

/// Execute one poll cycle: claim due markets, then fetch+persist outside the tx.
async fn poll_cycle(
    pool: &PgPool,
    chain: &[Arc<dyn Provider>],
    global_interval_secs: i64,
    claim_ttl_secs: i64,
) -> Result<()> {
    let markets = match claim_due_markets(pool, global_interval_secs, claim_ttl_secs).await {
        Ok(ms) => ms,
        Err(e) => {
            error!("live_poller: claim error: {e}");
            return Ok(()); // non-fatal: next cycle will retry
        }
    };

    for market in markets {
        let mq = MarketQuery {
            market_id: market.id,
            coin_id: market.coin_id.clone(),
            base: market.base.clone(),
            quote: market.quote.clone(),
            venue: market.venue.clone(),
            vs_currency: market.quote.to_lowercase(),
        };

        // Find the first provider supporting Spot for pacing (REQ-SCHED-041).
        let provider_name = match chain.iter().find(|p| p.supports(Capability::Spot)) {
            Some(p) => p.name().to_string(),
            None => {
                warn!(
                    "live_poller: no provider supports Spot for market {}",
                    market.id
                );
                let _ = clear_poll_marker(pool, market.id).await;
                continue;
            }
        };

        // Acquire pacer slot OUTSIDE any transaction (REQ-SCHED-041).
        match acquire_slot(pool, &provider_name).await {
            Ok(()) => {}
            Err(ref e) if pacer_should_skip(e) => {
                // Cooldown or credit exhaustion — release the marker, skip for now.
                warn!("live_poller: pacer skip for market {}: {e}", market.id);
                let _ = clear_poll_marker(pool, market.id).await;
                continue;
            }
            Err(e) => {
                error!("live_poller: pacer error for market {}: {e}", market.id);
                let _ = clear_poll_marker(pool, market.id).await;
                continue;
            }
        }

        // Fetch via chain (outside any transaction, REQ-SCHED-004).
        let fetch_result = chain_fetch_spot(chain, &mq).await;

        match fetch_result {
            Ok(quote) => {
                // Persist (idempotent upsert, REQ-SCHED-040).
                if let Err(e) = upsert_live_quote(pool, &quote).await {
                    error!("live_poller: upsert error for market {}: {e}", market.id);
                    let _ = clear_poll_marker(pool, market.id).await;
                    continue;
                }
                // Advance cursor and clear marker (REQ-SCHED-005).
                if let Err(e) = mark_poll_success(pool, market.id).await {
                    error!(
                        "live_poller: success mark error for market {}: {e}",
                        market.id
                    );
                }
            }
            Err(e) if is_transient_provider_error(&e) => {
                // Transient failure: clear marker only, market stays due (REQ-SCHED-006).
                warn!("live_poller: transient error for market {}: {e}", market.id);
                let _ = clear_poll_marker(pool, market.id).await;
            }
            Err(e) => {
                error!("live_poller: permanent error for market {}: {e}", market.id);
                let _ = clear_poll_marker(pool, market.id).await;
            }
        }
    }

    Ok(())
}

/// Try providers in order for `fetch_spot`; return the first success.
async fn chain_fetch_spot(
    chain: &[Arc<dyn Provider>],
    market: &MarketQuery,
) -> Result<crate::providers::SpotQuote, ProviderError> {
    let mut last_err = ProviderError::Other(anyhow::anyhow!("empty provider chain"));
    for provider in chain {
        if !provider.supports(Capability::Spot) {
            continue;
        }
        match provider.fetch_spot(market).await {
            Ok(q) => return Ok(q),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

fn is_transient_provider_error(e: &ProviderError) -> bool {
    e.is_transient()
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(h: u32, m: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, h, m, s).unwrap()
    }

    // ── Scenario 3 / REQ-SCHED-002: NULL per-market interval → global default ─

    #[test]
    fn null_last_polled_is_always_due() {
        let now = ts(12, 0, 0);
        assert!(
            is_market_due(None, None, 60, None, now),
            "market never polled must always be due"
        );
    }

    #[test]
    fn null_interval_uses_global_default() {
        let now = ts(12, 1, 0);
        // last_polled 30s ago, global = 60s → NOT due
        let last = ts(12, 0, 30);
        assert!(
            !is_market_due(Some(last), None, 60, None, now),
            "30s ago with 60s global default must not be due"
        );
    }

    #[test]
    fn null_interval_due_after_global_default_elapsed() {
        let now = ts(12, 1, 30);
        // last_polled 90s ago, global = 60s → due (90 >= 60)
        let last = ts(12, 0, 0);
        assert!(
            is_market_due(Some(last), None, 60, None, now),
            "90s ago with 60s global default must be due"
        );
    }

    #[test]
    fn per_market_interval_overrides_global() {
        let now = ts(12, 0, 50);
        // last_polled 50s ago, global = 60s (would NOT be due), per-market = 30s (IS due)
        let last = ts(12, 0, 0);
        assert!(
            is_market_due(Some(last), None, 60, Some(30), now),
            "per-market interval 30s must override global 60s"
        );
    }

    #[test]
    fn per_market_interval_longer_than_global_delays_poll() {
        let now = ts(12, 1, 5);
        // last_polled 65s ago, global = 60s (would be due), per-market = 120s (NOT due)
        let last = ts(12, 0, 0);
        assert!(
            !is_market_due(Some(last), None, 60, Some(120), now),
            "per-market interval 120s must delay poll past global 60s"
        );
    }

    // ── Scenario 5 / REQ-SCHED-007: in-flight marker prevents double-poll ─────

    #[test]
    fn in_flight_marker_prevents_claim() {
        let now = ts(12, 0, 0);
        let future_marker = ts(12, 1, 0); // marker expires in 60s
        assert!(
            !is_market_due(None, Some(future_marker), 60, None, now),
            "market with active in-flight marker must not be due"
        );
    }

    #[test]
    fn expired_marker_makes_market_reclaimable() {
        let now = ts(12, 2, 0);
        let past_marker = ts(12, 1, 0); // marker expired 60s ago
        assert!(
            is_market_due(None, Some(past_marker), 60, None, now),
            "market with expired marker must be reclaimable"
        );
    }

    // ── Scenario 2 / REQ-SCHED-003/004: claim SQL shape ──────────────────────

    #[test]
    fn claim_sql_contains_for_update_skip_locked() {
        assert!(
            LIVE_POLLER_CLAIM_SQL.contains("FOR UPDATE SKIP LOCKED"),
            "claim SQL must use FOR UPDATE SKIP LOCKED for multi-replica safety"
        );
    }

    #[test]
    fn claim_sql_filters_status_active() {
        assert!(
            LIVE_POLLER_CLAIM_SQL.contains("status = 'active'"),
            "claim SQL must filter status='active' (REQ-SCHED-003)"
        );
    }

    #[test]
    fn claim_sql_has_due_predicate() {
        // Due predicate: last_polled_at IS NULL OR last_polled_at + COALESCE(...) <= now()
        assert!(
            LIVE_POLLER_CLAIM_SQL.contains("last_polled_at IS NULL"),
            "claim SQL must include NULL last_polled_at check"
        );
        assert!(
            LIVE_POLLER_CLAIM_SQL.contains("COALESCE(live_poll_interval"),
            "claim SQL must use COALESCE(live_poll_interval, ...) for per-market override"
        );
    }

    #[test]
    fn claim_sql_has_not_in_flight_predicate() {
        assert!(
            LIVE_POLLER_CLAIM_SQL
                .contains("live_poll_claimed_until IS NULL OR live_poll_claimed_until <= now()"),
            "claim SQL must include not-in-flight predicate (REQ-SCHED-003/007)"
        );
    }

    #[test]
    fn claim_sql_sets_marker_not_cursor() {
        // Must SET live_poll_claimed_until, must NOT SET last_polled_at.
        assert!(
            LIVE_POLLER_CLAIM_SQL.contains("live_poll_claimed_until = now()"),
            "claim SQL must set live_poll_claimed_until (the marker)"
        );
        assert!(
            !LIVE_POLLER_CLAIM_SQL.contains("last_polled_at = now()"),
            "claim SQL must NOT advance last_polled_at (cursor advances only on success)"
        );
    }

    // ── Scenario 4 / REQ-SCHED-005/006: success vs failure SQL shape ──────────

    #[test]
    fn success_sql_advances_last_polled_at_and_clears_marker() {
        assert!(
            LIVE_POLLER_SUCCESS_SQL.contains("last_polled_at = now()"),
            "success SQL must advance last_polled_at (REQ-SCHED-005)"
        );
        assert!(
            LIVE_POLLER_SUCCESS_SQL.contains("live_poll_claimed_until = NULL"),
            "success SQL must clear the in-flight marker (REQ-SCHED-005)"
        );
    }

    #[test]
    fn failure_clear_sql_does_not_advance_last_polled_at() {
        assert!(
            LIVE_POLLER_FAILURE_CLEAR_SQL.contains("live_poll_claimed_until = NULL"),
            "failure-clear SQL must clear the marker (REQ-SCHED-006)"
        );
        assert!(
            !LIVE_POLLER_FAILURE_CLEAR_SQL.contains("last_polled_at"),
            "failure-clear SQL must NOT touch last_polled_at (REQ-SCHED-006)"
        );
    }

    // ── Pacer skip classification ──────────────────────────────────────────────

    #[test]
    fn cooldown_triggers_skip() {
        let err = AcquireSlotError::Cooldown("coingecko".to_string(), Utc::now());
        assert!(pacer_should_skip(&err), "cooldown must trigger skip action");
    }

    #[test]
    fn credit_exhausted_triggers_skip() {
        let err = AcquireSlotError::CreditExhausted("coingecko".to_string());
        assert!(
            pacer_should_skip(&err),
            "credit exhaustion must trigger skip action"
        );
    }

    #[test]
    fn db_error_does_not_trigger_skip() {
        let err = AcquireSlotError::NotFound("coingecko".to_string());
        assert!(!pacer_should_skip(&err), "DB error must not trigger skip");
    }

    // ── secs_to_pg_interval ────────────────────────────────────────────────────

    #[test]
    fn secs_to_pg_interval_formats_correctly() {
        assert_eq!(secs_to_pg_interval(30), "30 seconds");
        assert_eq!(secs_to_pg_interval(120), "120 seconds");
        assert_eq!(secs_to_pg_interval(0), "0 seconds");
    }

    // ── DB-gated integration tests (require live DATABASE_URL) ────────────────

    /// Scenario 5 / REQ-SCHED-007: two concurrent claim calls produce disjoint market sets.
    #[tokio::test]
    #[ignore]
    async fn db_concurrent_claims_produce_disjoint_sets() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
        let pool = crate::db::connect(&url).await.expect("connect");

        // Reset: clear all markers and set some markets as due.
        sqlx::query(
            "UPDATE tracked_markets \
             SET live_poll_claimed_until = NULL, last_polled_at = NULL \
             WHERE status = 'active'",
        )
        .execute(&pool)
        .await
        .expect("reset markers");

        // Run two claims concurrently.
        let pool2 = pool.clone();
        let (r1, r2) = tokio::join!(
            claim_due_markets(&pool, 60, 120),
            claim_due_markets(&pool2, 60, 120),
        );

        let ids1: std::collections::HashSet<i64> =
            r1.expect("claim 1").iter().map(|m| m.id).collect();
        let ids2: std::collections::HashSet<i64> =
            r2.expect("claim 2").iter().map(|m| m.id).collect();

        // No market should be in both sets (SKIP LOCKED guarantees disjoint claims).
        let intersection: std::collections::HashSet<_> = ids1.intersection(&ids2).collect();
        assert!(
            intersection.is_empty(),
            "concurrent claims must produce disjoint market sets; overlap: {intersection:?}"
        );
    }
}
