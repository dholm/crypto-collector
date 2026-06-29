//! Per-provider, credit-aware upstream request pacer (SPEC-PROV-001).
//!
//! Generalises `ticker-collector`'s single-row `yf_request_pacer` to a
//! keyed, multi-provider table with monthly credit accounting.
//!
//! Two layers of egress control (research §3.3):
//! - **DB pacer** (`upstream_request_pacer`): fleet-wide, serialises across replicas.
//! - **Local throttle** (`LocalThrottle`): per-replica burst smoothing.
//!
//! All outbound HTTP calls MUST acquire a slot via `acquire_slot()` before
//! issuing any network request (REQ-PROV-040/045).

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};
use thiserror::Error;
use tokio::sync::Mutex;

// ── Pure decision logic (testable without DB) ───────────────────────────────

/// Outcome of pacer slot evaluation (pure, no I/O).
///
/// Used for unit testing pacer logic independently of the DB.
#[derive(Debug, PartialEq, Eq)]
pub enum PacerDecision {
    /// Slot is available; caller may proceed at or after `next_allowed_at`.
    Allow { next_allowed_at: DateTime<Utc> },
    /// Provider is in fleet-wide cooldown; no requests until `until`.
    Cooldown { until: DateTime<Utc> },
    /// Monthly credit limit reached; no requests until window resets.
    CreditExhausted,
}

/// Pure pacer slot decision — testable without DB I/O.
///
/// Mirrors the DB UPDATE WHERE conditions:
/// `WHERE (cooldown_until IS NULL OR cooldown_until <= now())
///    AND (credit_limit IS NULL OR credits_used < credit_limit)`
///
/// `next_allowed_at` here is the value the caller *would* compute after the gap;
/// this function only gates on cooldown and credit exhaustion, not on timing.
pub fn pacer_decision(
    cooldown_until: Option<DateTime<Utc>>,
    credit_limit: Option<i64>,
    credits_used: i64,
    next_allowed_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> PacerDecision {
    // Gate 1: fleet-wide cooldown
    if let Some(until) = cooldown_until {
        if until > now {
            return PacerDecision::Cooldown { until };
        }
    }

    // Gate 2: monthly credit budget
    if let Some(limit) = credit_limit {
        if credits_used >= limit {
            return PacerDecision::CreditExhausted;
        }
    }

    PacerDecision::Allow { next_allowed_at }
}

// ── Local replica throttle ───────────────────────────────────────────────────

/// Per-replica minimum-gap gate (mirrors ticker-collector `YfThrottle`).
///
/// Smooths intra-replica bursts. The DB pacer is the fleet-wide source of truth;
/// the local throttle only reduces DB lock contention.
pub struct LocalThrottle {
    last_request: Mutex<Option<Instant>>,
    min_gap: StdDuration,
}

impl LocalThrottle {
    pub fn new(min_gap_ms: u64) -> Self {
        Self {
            last_request: Mutex::new(None),
            min_gap: StdDuration::from_millis(min_gap_ms),
        }
    }

    /// Wait until the minimum gap since the last request has elapsed.
    ///
    /// Lock is released before sleeping so concurrent callers queue correctly
    /// (mirrors ticker-collector `YfThrottle::acquire`).
    pub async fn acquire(&self) {
        if self.min_gap.is_zero() {
            return;
        }
        let sleep_for = {
            let mut guard = self.last_request.lock().await;
            let now = Instant::now();
            let sleep_for = match *guard {
                None => StdDuration::ZERO,
                Some(prev) => self.min_gap.saturating_sub(now.duration_since(prev)),
            };
            *guard = Some(now + sleep_for);
            sleep_for
        };
        if !sleep_for.is_zero() {
            tokio::time::sleep(sleep_for).await;
        }
    }
}

impl Default for LocalThrottle {
    fn default() -> Self {
        Self::new(0)
    }
}

// ── DB pacer operations ─────────────────────────────────────────────────────

/// Error returned when a pacer slot cannot be acquired.
#[derive(Debug, Error)]
pub enum AcquireSlotError {
    #[error("provider '{0}' is in fleet-wide cooldown until {1}")]
    Cooldown(String, DateTime<Utc>),

    #[error("provider '{0}' has exhausted its monthly credit limit")]
    CreditExhausted(String),

    #[error("provider '{0}' not found in upstream_request_pacer")]
    NotFound(String),

    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// Acquire one egress slot from `upstream_request_pacer` and sleep until the allowed instant.
///
/// **This is the single fleet-wide egress governor.** Every outbound provider HTTP
/// request MUST call this before issuing the request (REQ-PROV-040/045).
///
/// Protocol:
/// 1. Reset monthly credit window if elapsed (REQ-PROV-044).
/// 2. Atomic UPDATE with cooldown + credit gates; returns `next_allowed_at`.
/// 3. Sleep OUTSIDE the transaction until `next_allowed_at` (never sleep inside the
///    lock — would serialize all replicas through one DB lock cycle).
///
/// Returns:
/// - `Ok(())` — slot acquired, caller may proceed (after sleep).
/// - `Err(AcquireSlotError::Cooldown)` — fleet-wide cooldown active.
/// - `Err(AcquireSlotError::CreditExhausted)` — monthly credit limit reached.
///
// @MX:WARN: [AUTO] acquire_slot is the single fleet-wide egress governor; every outbound HTTP call routes through it
// @MX:REASON: Bypassing acquire_slot risks 429 flood, upstream account bans, and monthly credit exhaustion.
//             REQ-PROV-040: ALL outbound calls acquire before HTTP. REQ-PROV-045: no second pacing mechanism.
//             Sleep MUST occur OUTSIDE the transaction (see ticker-collector pacer.rs @MX:WARN).
// @MX:SPEC: SPEC-PROV-001 REQ-PROV-040/041/043/044/045
pub async fn acquire_slot(pool: &PgPool, provider: &str) -> Result<(), AcquireSlotError> {
    // Step 1: reset credit window if the monthly interval has elapsed (REQ-PROV-044).
    reset_credit_window_if_needed(pool, provider).await?;

    // Step 2: atomic UPDATE with two WHERE gates:
    //   - cooldown_until IS NULL OR cooldown_until <= now()   (REQ-PROV-041)
    //   - credit_limit IS NULL OR credits_used < credit_limit (REQ-PROV-043)
    //
    // Advance next_allowed_at = GREATEST(now(), next_allowed_at) + min_gap_ms interval.
    // Increment credits_used atomically.
    // RETURNING next_allowed_at — None if either gate blocked.
    let next_allowed_at: Option<DateTime<Utc>> = sqlx::query_scalar(
        "UPDATE upstream_request_pacer \
         SET next_allowed_at = GREATEST(now(), next_allowed_at) \
                               + (min_gap_ms * INTERVAL '1 ms'), \
             credits_used = credits_used + 1, \
             updated_at   = now() \
         WHERE provider = $1 \
           AND (cooldown_until IS NULL OR cooldown_until <= now()) \
           AND (credit_limit IS NULL OR credits_used < credit_limit) \
         RETURNING next_allowed_at",
    )
    .bind(provider)
    .fetch_optional(pool)
    .await?;

    match next_allowed_at {
        Some(next_at) => {
            // Step 3: sleep OUTSIDE the transaction until the slot opens.
            let now = Utc::now();
            let wait = next_at.signed_duration_since(now);
            if wait > Duration::zero() {
                let ms = wait.num_milliseconds().clamp(0, 60_000) as u64;
                tokio::time::sleep(StdDuration::from_millis(ms)).await;
            }
            Ok(())
        }
        None => {
            // Determine why: cooldown or credit exhaustion
            let row: Option<(Option<DateTime<Utc>>, Option<i64>, i64)> = sqlx::query_as(
                "SELECT cooldown_until, credit_limit, credits_used \
                 FROM upstream_request_pacer WHERE provider = $1",
            )
            .bind(provider)
            .fetch_optional(pool)
            .await?;

            match row {
                None => Err(AcquireSlotError::NotFound(provider.to_string())),
                Some((cooldown_until, credit_limit, credits_used)) => {
                    let now = Utc::now();
                    if let Some(until) = cooldown_until {
                        if until > now {
                            return Err(AcquireSlotError::Cooldown(provider.to_string(), until));
                        }
                    }
                    if credit_limit.is_some_and(|lim| credits_used >= lim) {
                        return Err(AcquireSlotError::CreditExhausted(provider.to_string()));
                    }
                    // Fallback — pacer row exists but condition is unclear; treat as NotFound
                    Err(AcquireSlotError::NotFound(provider.to_string()))
                }
            }
        }
    }
}

/// Set a fleet-wide cooldown for a provider after an HTTP 429 or quota signal (REQ-PROV-042).
///
/// All replicas reading `upstream_request_pacer` will see `cooldown_until` and withhold
/// requests until it expires. `acquire_slot` checks this atomically.
pub async fn signal_cooldown(
    pool: &PgPool,
    provider: &str,
    cooldown_ms: u64,
) -> Result<(), sqlx::Error> {
    let cooldown_until = Utc::now() + Duration::milliseconds(cooldown_ms as i64);
    sqlx::query(
        "UPDATE upstream_request_pacer \
         SET cooldown_until = $2, updated_at = now() \
         WHERE provider = $1",
    )
    .bind(provider)
    .bind(cooldown_until)
    .execute(pool)
    .await?;
    Ok(())
}

/// Reset the monthly credit window if the 1-month interval has elapsed (REQ-PROV-044).
///
/// Called by `acquire_slot` before each slot attempt. Idempotent (UPDATE WHERE elapsed).
async fn reset_credit_window_if_needed(pool: &PgPool, provider: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE upstream_request_pacer \
         SET credits_used        = 0, \
             credit_window_start = now(), \
             updated_at          = now() \
         WHERE provider = $1 \
           AND now() - credit_window_start >= INTERVAL '1 month'",
    )
    .bind(provider)
    .execute(pool)
    .await?;
    Ok(())
}

// ── Shared Arc wrapper for use in providers ──────────────────────────────────

/// Thread-safe local throttle suitable for sharing across provider clones.
pub type SharedLocalThrottle = Arc<LocalThrottle>;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};

    fn ts(h: u32, m: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, h, m, s).unwrap()
    }

    // ── Pacer decision — pure logic (Scenario 10 / REQ-PROV-040) ────────────

    #[test]
    fn pacer_decision_allows_when_no_gates() {
        let now = ts(12, 0, 0);
        let next = now + Duration::seconds(2);
        let result = pacer_decision(None, None, 0, next, now);
        assert_eq!(
            result,
            PacerDecision::Allow {
                next_allowed_at: next
            }
        );
    }

    #[test]
    fn pacer_decision_allows_when_credit_limit_null() {
        let now = ts(12, 0, 0);
        let next = now + Duration::seconds(2);
        // credits_used = 99999, credit_limit = None → unlimited → allowed
        let result = pacer_decision(None, None, 99999, next, now);
        assert_eq!(
            result,
            PacerDecision::Allow {
                next_allowed_at: next
            }
        );
    }

    // ── Cooldown gate (Scenario 11 / REQ-PROV-041) ──────────────────────────

    #[test]
    fn pacer_decision_blocks_during_active_cooldown() {
        let now = ts(12, 0, 0);
        let until = ts(12, 1, 0); // 1 minute in future
        let next = now + Duration::seconds(2);
        let result = pacer_decision(Some(until), None, 0, next, now);
        assert_eq!(result, PacerDecision::Cooldown { until });
    }

    #[test]
    fn pacer_decision_allows_after_cooldown_expires() {
        let now = ts(12, 5, 0);
        let until = ts(12, 1, 0); // 4 minutes in the past
        let next = now + Duration::seconds(2);
        // Past cooldown → allowed
        let result = pacer_decision(Some(until), None, 0, next, now);
        assert_eq!(
            result,
            PacerDecision::Allow {
                next_allowed_at: next
            }
        );
    }

    // ── Credit exhaustion gate (Scenario 12 / REQ-PROV-043) ────────────────

    #[test]
    fn pacer_decision_blocks_when_credit_limit_reached() {
        let now = ts(12, 0, 0);
        let next = now + Duration::seconds(2);
        // credits_used == credit_limit → exhausted
        let result = pacer_decision(None, Some(10_000), 10_000, next, now);
        assert_eq!(result, PacerDecision::CreditExhausted);
    }

    #[test]
    fn pacer_decision_blocks_when_credits_exceeded() {
        let now = ts(12, 0, 0);
        let next = now + Duration::seconds(2);
        // credits_used > credit_limit (shouldn't happen normally but guard it)
        let result = pacer_decision(None, Some(10_000), 10_001, next, now);
        assert_eq!(result, PacerDecision::CreditExhausted);
    }

    #[test]
    fn pacer_decision_allows_when_credits_below_limit() {
        let now = ts(12, 0, 0);
        let next = now + Duration::seconds(2);
        let result = pacer_decision(None, Some(10_000), 9_999, next, now);
        assert_eq!(
            result,
            PacerDecision::Allow {
                next_allowed_at: next
            }
        );
    }

    // Cooldown takes priority over credit exhaustion
    #[test]
    fn pacer_decision_cooldown_takes_priority_over_credit_exhausted() {
        let now = ts(12, 0, 0);
        let until = ts(12, 1, 0);
        let next = now + Duration::seconds(2);
        let result = pacer_decision(Some(until), Some(10_000), 10_000, next, now);
        assert_eq!(result, PacerDecision::Cooldown { until });
    }

    // ── Local throttle (no DB) ───────────────────────────────────────────────

    #[tokio::test]
    async fn local_throttle_zero_gap_is_noop() {
        let t = LocalThrottle::new(0);
        t.acquire().await;
        t.acquire().await;
        // No panic, no sleep — passes
    }

    #[tokio::test]
    async fn local_throttle_spaces_calls() {
        use std::time::Instant;
        let t = LocalThrottle::new(50); // 50ms gap
        t.acquire().await; // first: no wait
        let start = Instant::now();
        t.acquire().await; // second: should wait ~50ms
        assert!(
            start.elapsed() >= StdDuration::from_millis(40),
            "local throttle must space calls by min_gap, got {:?}",
            start.elapsed()
        );
    }


    // ── DB-gated integration tests (require live DATABASE_URL) ──────────────

    async fn setup_db() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL must be set for pacer integration tests");
        let pool = crate::db::connect(&url).await.expect("db connect");
        pool
    }

    /// Scenario 10 (REQ-PROV-040): acquire_slot advances next_allowed_at by min_gap_ms
    /// and increments credits_used.
    #[tokio::test]
    #[ignore]
    async fn db_pacer_acquire_slot_advances_next_allowed_at() {
        let pool = setup_db().await;

        // Reset pacer to known state
        sqlx::query(
            "UPDATE upstream_request_pacer \
             SET next_allowed_at = now(), credits_used = 0, cooldown_until = NULL \
             WHERE provider = 'coingecko'",
        )
        .execute(&pool)
        .await
        .expect("reset");

        let before: DateTime<Utc> = sqlx::query_scalar(
            "SELECT next_allowed_at FROM upstream_request_pacer WHERE provider = 'coingecko'",
        )
        .fetch_one(&pool)
        .await
        .expect("before");

        // Acquire slot (will sleep up to min_gap_ms)
        acquire_slot(&pool, "coingecko").await.expect("acquire");

        let after: DateTime<Utc> = sqlx::query_scalar(
            "SELECT next_allowed_at FROM upstream_request_pacer WHERE provider = 'coingecko'",
        )
        .fetch_one(&pool)
        .await
        .expect("after");

        // next_allowed_at must have advanced by at least min_gap_ms (2000ms for coingecko)
        let gap = after.signed_duration_since(before);
        assert!(
            gap >= Duration::milliseconds(1900),
            "next_allowed_at must advance by min_gap_ms (~2000ms for coingecko), got {gap:?}"
        );
    }

    /// Scenario 10 (REQ-PROV-040): credits_used increments by 1 per acquisition.
    #[tokio::test]
    #[ignore]
    async fn db_pacer_acquire_slot_increments_credits_used() {
        let pool = setup_db().await;

        // Reset
        sqlx::query(
            "UPDATE upstream_request_pacer \
             SET credits_used = 100, cooldown_until = NULL, next_allowed_at = now() \
             WHERE provider = 'binance'",
        )
        .execute(&pool)
        .await
        .expect("reset");

        acquire_slot(&pool, "binance").await.expect("acquire");

        let used: i64 = sqlx::query_scalar(
            "SELECT credits_used FROM upstream_request_pacer WHERE provider = 'binance'",
        )
        .fetch_one(&pool)
        .await
        .expect("fetch");

        assert_eq!(
            used, 101,
            "credits_used must increment by 1 per acquisition"
        );
    }

    /// Scenario 11 (REQ-PROV-041/042): signal_cooldown sets cooldown_until;
    /// subsequent acquire_slot withholds.
    #[tokio::test]
    #[ignore]
    async fn db_pacer_signal_cooldown_withholds_acquire() {
        let pool = setup_db().await;

        // Set a 60-second cooldown
        signal_cooldown(&pool, "coinbase", 60_000)
            .await
            .expect("signal_cooldown");

        let result = acquire_slot(&pool, "coinbase").await;
        assert!(
            matches!(result, Err(AcquireSlotError::Cooldown(_, _))),
            "acquire_slot must be withheld during cooldown, got: {result:?}"
        );

        // Clear the cooldown
        sqlx::query(
            "UPDATE upstream_request_pacer SET cooldown_until = NULL WHERE provider = 'coinbase'",
        )
        .execute(&pool)
        .await
        .expect("clear cooldown");
    }

    /// Scenario 12 (REQ-PROV-043): credit exhaustion withholds acquire.
    #[tokio::test]
    #[ignore]
    async fn db_pacer_credit_exhaustion_withholds_acquire() {
        let pool = setup_db().await;

        // Exhaust credits on kraken (set to limit)
        sqlx::query(
            "UPDATE upstream_request_pacer \
             SET credits_used = credit_limit, cooldown_until = NULL, next_allowed_at = now() \
             WHERE provider = 'kraken' AND credit_limit IS NOT NULL",
        )
        .execute(&pool)
        .await
        .expect("exhaust credits");

        // If kraken has no credit_limit, set one first
        sqlx::query(
            "UPDATE upstream_request_pacer \
             SET credit_limit = 100, credits_used = 100, cooldown_until = NULL \
             WHERE provider = 'kraken'",
        )
        .execute(&pool)
        .await
        .expect("set limit");

        let result = acquire_slot(&pool, "kraken").await;
        assert!(
            matches!(result, Err(AcquireSlotError::CreditExhausted(_))),
            "acquire_slot must be withheld when credits exhausted, got: {result:?}"
        );

        // Restore
        sqlx::query(
            "UPDATE upstream_request_pacer SET credit_limit = NULL, credits_used = 0 WHERE provider = 'kraken'",
        )
        .execute(&pool)
        .await
        .expect("restore");
    }

    /// Scenario 12 (REQ-PROV-044): credit window resets after 1 month.
    #[tokio::test]
    #[ignore]
    async fn db_pacer_credit_window_resets_after_month() {
        let pool = setup_db().await;

        // Set credit_window_start to 2 months ago and exhaust credits
        sqlx::query(
            "UPDATE upstream_request_pacer \
             SET credits_used = 10000, credit_limit = 10000, \
                 credit_window_start = now() - INTERVAL '2 months', \
                 cooldown_until = NULL, next_allowed_at = now() \
             WHERE provider = 'coingecko'",
        )
        .execute(&pool)
        .await
        .expect("setup stale window");

        // acquire_slot triggers reset_credit_window_if_needed then grants slot
        acquire_slot(&pool, "coingecko")
            .await
            .expect("should succeed after window reset");

        let (credits_used, window_start): (i64, DateTime<Utc>) = sqlx::query_as(
            "SELECT credits_used, credit_window_start FROM upstream_request_pacer \
             WHERE provider = 'coingecko'",
        )
        .fetch_one(&pool)
        .await
        .expect("fetch after reset");

        // After reset: credits_used should be 1 (the slot we just acquired)
        assert_eq!(
            credits_used, 1,
            "credits_used must reset to 0 then increment to 1"
        );

        // credit_window_start must be recent (within last minute)
        let age = Utc::now().signed_duration_since(window_start);
        assert!(
            age < Duration::minutes(1),
            "credit_window_start must be reset to now, age={age:?}"
        );
    }

}
