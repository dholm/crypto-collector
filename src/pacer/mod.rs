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

    /// The gated UPDATE matched no row, but the diagnostic re-SELECT found the row present
    /// and neither the cooldown gate nor the credit gate explains the block — the block
    /// lapsed between the UPDATE and the re-SELECT (a race). Transient-by-nature: a retry
    /// would likely succeed. Reserved distinctly from `NotFound` (F-18/REQ-PROV-062, D4).
    #[error("provider '{0}' pacer row was contended (block lapsed mid-check); retry")]
    Contended(String),

    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

// ── Pure honesty cores (F-13, F-18 — testable without DB) ────────────────────

/// Former `.clamp(0, 60_000)` ceiling — now an **observability threshold**, not a
/// truncation. A computed wait beyond this is still slept in full; the threshold only
/// gates the `warn!` + backlog metric (SPEC-PROV-002 D3).
const PACER_BACKLOG_WARN_MS: i64 = 60_000;

/// Decide how long `acquire_slot` sleeps after its atomic reservation, and whether the
/// wait exceeded the observability threshold (REQ-PROV-060, pure, DB-free).
///
/// The atomic UPDATE already advanced `next_allowed_at`, so the reservation is
/// authoritative — firing before it is the defect the old `.clamp(0, 60_000)` introduced.
/// This therefore returns the **full** wait (no truncation); the boolean is set when the
/// wait exceeds `PACER_BACKLOG_WARN_MS` so the caller can `warn!` + increment a backlog
/// metric for observability WITHOUT changing how long it sleeps. A non-positive wait
/// returns `(ZERO, false)`.
pub fn sleep_plan(next_at: DateTime<Utc>, now: DateTime<Utc>) -> (StdDuration, bool) {
    let wait = next_at.signed_duration_since(now);
    let ms = wait.num_milliseconds();
    if ms <= 0 {
        return (StdDuration::ZERO, false);
    }
    // Full wait, never truncated (the reservation is authoritative).
    (
        StdDuration::from_millis(ms as u64),
        ms > PACER_BACKLOG_WARN_MS,
    )
}

/// Classify a blocked `acquire_slot` from the diagnostic re-SELECT (REQ-PROV-062, D4, pure).
///
/// The gated UPDATE returned no row; this maps the diagnostic re-SELECT `(cooldown_until,
/// credit_limit, credits_used)` to the honest error:
/// - `None` (row genuinely absent) → `NotFound`;
/// - active cooldown (`cooldown_until > now`) → `Cooldown`;
/// - credit gate reached (`credits_used >= credit_limit`) → `CreditExhausted`;
/// - row present but neither gate explains the block (a lapsed-block race) → `Contended`,
///   NOT `NotFound` (which is reserved for the genuinely-absent row).
fn classify_blocked(
    provider: &str,
    row: Option<(Option<DateTime<Utc>>, Option<i64>, i64)>,
    now: DateTime<Utc>,
) -> AcquireSlotError {
    match row {
        None => AcquireSlotError::NotFound(provider.to_string()),
        Some((cooldown_until, credit_limit, credits_used)) => {
            if let Some(until) = cooldown_until {
                if until > now {
                    return AcquireSlotError::Cooldown(provider.to_string(), until);
                }
            }
            if credit_limit.is_some_and(|lim| credits_used >= lim) {
                return AcquireSlotError::CreditExhausted(provider.to_string());
            }
            // Row exists, both gates clear — the block lapsed between the UPDATE and this
            // re-SELECT. An honest "retry would have succeeded" label, not a misleading
            // NotFound (F-18).
            AcquireSlotError::Contended(provider.to_string())
        }
    }
}

/// Emit the backlog observability signal (`warn!` + counter) when a pacer wait exceeds the
/// threshold. The sleep itself is NOT affected — this is observability only (REQ-PROV-060,
/// D2). Split out so the metric increment is unit-observable with a local recorder.
fn record_backlog_wait(provider: &str, wait: StdDuration) {
    tracing::warn!(
        provider = provider,
        wait_secs = wait.as_secs(),
        threshold_ms = PACER_BACKLOG_WARN_MS,
        "pacer acquire_slot wait exceeds the backlog threshold; sleeping the full reserved wait (no truncation)"
    );
    metrics::counter!(
        "pacer_backlog_wait_exceeded_total",
        "provider" => provider.to_string(),
    )
    .increment(1);
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
// @MX:WARN: [AUTO] acquire_slot is the single fleet-wide egress governor; the standard
//           enforcement point is providers::transport::paced, which wraps this call.
// @MX:REASON: Bypassing acquire_slot risks 429 flood, upstream account bans, and monthly credit exhaustion.
//             REQ-PROV-040: ALL outbound calls acquire before HTTP. REQ-PROV-045: no second pacing mechanism.
//             Sleep MUST occur OUTSIDE the transaction (see ticker-collector pacer.rs @MX:WARN).
//             The reservation advances next_allowed_at atomically, so the sleep honours the
//             FULL computed wait (no 60 s truncation) — firing before the reserved slot is
//             exactly the burst the pacer prevents (F-13). Backlog beyond the former ceiling
//             is surfaced via warn! + pacer_backlog_wait_exceeded_total, NOT truncated.
// @MX:SPEC: SPEC-PROV-001 REQ-PROV-040/041/043/044/045 SPEC-PROV-002 REQ-PROV-060 REQ-PROV-062
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
            // Step 3: sleep OUTSIDE the transaction until the slot opens. The reservation
            // already advanced next_allowed_at, so sleep the FULL computed wait — never
            // truncate (F-13). Backlog beyond the former ceiling is surfaced for
            // observability but does not change how long we sleep (D3).
            let (dur, backlog_exceeded) = sleep_plan(next_at, Utc::now());
            if backlog_exceeded {
                record_backlog_wait(provider, dur);
            }
            if !dur.is_zero() {
                tokio::time::sleep(dur).await;
            }
            Ok(())
        }
        None => {
            // Diagnose why the gated UPDATE matched no row. A genuinely-absent row is
            // NotFound; a present row whose block lapsed mid-check is Contended, not a
            // misleading NotFound (F-18/REQ-PROV-062).
            let row: Option<(Option<DateTime<Utc>>, Option<i64>, i64)> = sqlx::query_as(
                "SELECT cooldown_until, credit_limit, credits_used \
                 FROM upstream_request_pacer WHERE provider = $1",
            )
            .bind(provider)
            .fetch_optional(pool)
            .await?;

            Err(classify_blocked(provider, row, Utc::now()))
        }
    }
}

/// Set a fleet-wide cooldown for a provider after an HTTP 429 or quota signal (REQ-PROV-042).
///
/// All replicas reading `upstream_request_pacer` will see `cooldown_until` and withhold
/// requests until it expires. `acquire_slot` checks this atomically.
///
/// The cooldown is **monotonic** — `GREATEST(COALESCE(cooldown_until,'epoch'), $2)` — so a
/// later, shorter signal (multi-replica races, operator-set cooldowns) can never truncate
/// an earlier, longer one (F-17/REQ-PROV-061). A NULL existing cooldown coalesces to epoch
/// so the first signal always applies.
///
// @MX:WARN: [AUTO] signal_cooldown is monotonic — GREATEST never shortens an existing cooldown
// @MX:REASON: A shortened cooldown reopens the very 429 window the cooldown exists to close;
//             a later, shorter signal (a stale replica, or an operator-set longer cooldown)
//             must NOT truncate a longer one. GREATEST(COALESCE(cooldown_until,'epoch'),$2)
//             enforces this at the SQL layer for every replica (F-17).
// @MX:SPEC: SPEC-PROV-002 REQ-PROV-061
pub async fn signal_cooldown(
    pool: &PgPool,
    provider: &str,
    cooldown_ms: u64,
) -> Result<(), sqlx::Error> {
    let cooldown_until = Utc::now() + Duration::milliseconds(cooldown_ms as i64);
    sqlx::query(
        "UPDATE upstream_request_pacer \
         SET cooldown_until = GREATEST(COALESCE(cooldown_until, 'epoch'::timestamptz), $2), \
             updated_at = now() \
         WHERE provider = $1",
    )
    .bind(provider)
    .bind(cooldown_until)
    .execute(pool)
    .await?;
    Ok(())
}

/// Return the chain members that have **no** `upstream_request_pacer` row (REQ-PROV-063).
///
/// `SELECT provider FROM upstream_request_pacer WHERE provider = ANY($1)` returns the
/// present members; the missing set is the requested names minus the present ones. Called
/// once at startup (`main.rs` Step 8) AFTER migrations succeed — a non-empty result fails
/// readiness naming the missing member (REQ-PROV-064). A missing row otherwise surfaces
/// only as an error on every fetch at runtime (F-15).
pub async fn missing_pacer_rows(
    pool: &PgPool,
    providers: &[String],
) -> Result<Vec<String>, sqlx::Error> {
    let present: Vec<String> =
        sqlx::query_scalar("SELECT provider FROM upstream_request_pacer WHERE provider = ANY($1)")
            .bind(providers)
            .fetch_all(pool)
            .await?;
    Ok(providers
        .iter()
        .filter(|p| !present.contains(p))
        .cloned()
        .collect())
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

    // ── sleep_plan pure core (Scenario 5a / REQ-PROV-060) ───────────────────

    #[test]
    fn sleep_plan_wait_beyond_ceiling_is_full_not_truncated() {
        let now = ts(12, 0, 0);
        // 120 s wait — twice the former 60 s clamp ceiling.
        let next = now + Duration::seconds(120);
        let (dur, exceeded) = sleep_plan(next, now);
        // FULL wait (no truncation to 60_000 ms) — FAILS against the pre-fix clamp. RED-first.
        assert_eq!(dur, StdDuration::from_millis(120_000));
        assert!(
            exceeded,
            "a wait beyond the ceiling must set backlog_exceeded"
        );
    }

    #[test]
    fn sleep_plan_wait_within_ceiling_is_wait_and_not_exceeded() {
        let now = ts(12, 0, 0);
        let next = now + Duration::seconds(30);
        let (dur, exceeded) = sleep_plan(next, now);
        assert_eq!(dur, StdDuration::from_millis(30_000));
        assert!(!exceeded);
    }

    #[test]
    fn sleep_plan_exactly_ceiling_is_not_exceeded() {
        let now = ts(12, 0, 0);
        let next = now + Duration::milliseconds(60_000);
        let (dur, exceeded) = sleep_plan(next, now);
        assert_eq!(dur, StdDuration::from_millis(60_000));
        // Exactly at the ceiling is NOT "exceeded" (strict `>`), so no spurious warn/metric.
        assert!(!exceeded);
    }

    #[test]
    fn sleep_plan_nonpositive_wait_is_zero() {
        let now = ts(12, 0, 0);
        let next = now - Duration::seconds(5); // slot already open
        let (dur, exceeded) = sleep_plan(next, now);
        assert_eq!(dur, StdDuration::ZERO);
        assert!(!exceeded);
    }

    // ── classify_blocked pure core (D1 / Scenario 5c / REQ-PROV-062) ────────

    #[test]
    fn classify_blocked_absent_row_is_not_found() {
        let e = classify_blocked("coingecko", None, ts(12, 0, 0));
        assert!(
            matches!(e, AcquireSlotError::NotFound(ref p) if p == "coingecko"),
            "a genuinely-absent row must be NotFound, got {e:?}"
        );
    }

    #[test]
    fn classify_blocked_lapsed_block_row_is_contended() {
        let now = ts(12, 0, 0);
        // Row present; cooldown expired (past); credits below limit → neither gate explains
        // the block → the block lapsed mid-check → Contended (NOT NotFound).
        let past = ts(11, 0, 0);
        let e = classify_blocked("coingecko", Some((Some(past), Some(100), 5)), now);
        assert!(
            matches!(e, AcquireSlotError::Contended(ref p) if p == "coingecko"),
            "a present-but-lapsed-block row must be Contended, got {e:?}"
        );
    }

    #[test]
    fn classify_blocked_active_cooldown_is_cooldown() {
        let now = ts(12, 0, 0);
        let future = ts(12, 5, 0);
        let e = classify_blocked("binance", Some((Some(future), None, 0)), now);
        assert!(matches!(e, AcquireSlotError::Cooldown(_, _)), "got {e:?}");
    }

    #[test]
    fn classify_blocked_credit_exhausted() {
        let now = ts(12, 0, 0);
        let e = classify_blocked("kraken", Some((None, Some(100), 100)), now);
        assert!(
            matches!(e, AcquireSlotError::CreditExhausted(_)),
            "got {e:?}"
        );
    }

    // ── record_backlog_wait metric increment (D2 / REQ-PROV-060) ────────────

    /// The backlog observability signal increments `pacer_backlog_wait_exceeded_total`
    /// labelled by provider. Uses a test-local recorder (no global install), mirroring the
    /// `metrics` module's test harness.
    #[test]
    fn record_backlog_wait_increments_provider_labelled_counter() {
        use metrics_exporter_prometheus::PrometheusBuilder;
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record_backlog_wait("coingecko", StdDuration::from_secs(90));
        });
        let rendered = handle.render();
        assert!(
            rendered.contains("pacer_backlog_wait_exceeded_total"),
            "backlog metric must be emitted, got:\n{rendered}"
        );
        assert!(
            rendered.contains(r#"provider="coingecko""#),
            "backlog metric must carry the provider label, got:\n{rendered}"
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

    /// Scenario 5b (REQ-PROV-061): a later, shorter cooldown must NOT shorten an existing
    /// longer one (GREATEST); a first signal against a NULL cooldown always applies.
    #[tokio::test]
    #[ignore]
    async fn db_signal_cooldown_is_monotonic_never_shortens() {
        let pool = setup_db().await;

        // Start from NULL so the first signal always applies (COALESCE(..,'epoch')).
        sqlx::query(
            "UPDATE upstream_request_pacer SET cooldown_until = NULL WHERE provider = 'coinbase'",
        )
        .execute(&pool)
        .await
        .expect("clear");

        // First signal: a long (1 h) cooldown applies against the NULL baseline.
        signal_cooldown(&pool, "coinbase", 3_600_000)
            .await
            .expect("long signal");
        let long: Option<DateTime<Utc>> = sqlx::query_scalar(
            "SELECT cooldown_until FROM upstream_request_pacer WHERE provider = 'coinbase'",
        )
        .fetch_one(&pool)
        .await
        .expect("read long");
        assert!(long.is_some(), "first signal against NULL must apply");

        // Second signal: a SHORT (1 s) cooldown must NOT truncate the existing long one.
        signal_cooldown(&pool, "coinbase", 1_000)
            .await
            .expect("short signal");
        let after: Option<DateTime<Utc>> = sqlx::query_scalar(
            "SELECT cooldown_until FROM upstream_request_pacer WHERE provider = 'coinbase'",
        )
        .fetch_one(&pool)
        .await
        .expect("read after");

        assert_eq!(
            after, long,
            "a later, shorter cooldown must leave the existing longer cooldown in place (GREATEST)"
        );

        // Restore.
        sqlx::query(
            "UPDATE upstream_request_pacer SET cooldown_until = NULL WHERE provider = 'coinbase'",
        )
        .execute(&pool)
        .await
        .expect("restore");
    }

    /// Scenario 5c (REQ-PROV-062, DB half): a genuinely-absent provider returns NotFound;
    /// a present, unblocked row returns Ok (NotFound is reserved for the absent case). The
    /// Contended positive path is race-timing-sensitive and is covered deterministically by
    /// the pure `classify_blocked_lapsed_block_row_is_contended` test above.
    #[tokio::test]
    #[ignore]
    async fn db_acquire_slot_absent_is_not_found_present_is_ok() {
        let pool = setup_db().await;

        // Absent provider name → NotFound.
        let absent = acquire_slot(&pool, "definitely_absent_provider_zzz").await;
        assert!(
            matches!(absent, Err(AcquireSlotError::NotFound(_))),
            "an absent pacer row must be NotFound, got {absent:?}"
        );

        // Present, unblocked row → Ok (never NotFound / Contended).
        sqlx::query(
            "UPDATE upstream_request_pacer \
             SET cooldown_until = NULL, credits_used = 0, next_allowed_at = now() \
             WHERE provider = 'coingecko'",
        )
        .execute(&pool)
        .await
        .expect("reset coingecko");
        let present = acquire_slot(&pool, "coingecko").await;
        assert!(
            present.is_ok(),
            "a present, unblocked row must acquire Ok (NotFound is reserved for absent rows), got {present:?}"
        );
    }

    /// Scenario 6 (REQ-PROV-063/064): a chain member without a pacer row is reported in the
    /// missing set; a chain whose members all have rows returns empty.
    #[tokio::test]
    #[ignore]
    async fn db_missing_pacer_rows_detects_absent_member() {
        let pool = setup_db().await;

        let names = vec![
            "coingecko".to_string(),
            "definitely_absent_provider_zzz".to_string(),
        ];
        let missing = missing_pacer_rows(&pool, &names)
            .await
            .expect("missing_pacer_rows");
        assert_eq!(
            missing,
            vec!["definitely_absent_provider_zzz".to_string()],
            "the absent member must be reported in the missing set"
        );

        let present_only = vec!["coingecko".to_string()];
        let none_missing = missing_pacer_rows(&pool, &present_only)
            .await
            .expect("missing_pacer_rows");
        assert!(
            none_missing.is_empty(),
            "a chain whose members all have pacer rows returns an empty missing set"
        );
    }
}
