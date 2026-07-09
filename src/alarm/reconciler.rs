//! Reconciler worker: the near-stateless desired-state sweep loop (SPEC-ALARM-001
//! Milestones 4–7).
//!
//! Each sweep: compute the CURRENT set of active alarm conditions from observable
//! state, `raise()` every active condition with `timeoutSeconds = ALARM_TTL_SECS`, and
//! simply stop refreshing every condition that is no longer active so the Alarm Center
//! auto-clears it once the TTL lapses (REQ-ALARM-011..018). For Critical/Error
//! conditions observed transitioning active→inactive, an optional immediate fast-clear
//! is fired as a latency optimisation (REQ-ALARM-014); Warning conditions rely on TTL
//! expiry alone. Batch 2 wired Tier 1 (registry-derived provider-unreachable/
//! all-providers-down + pacer-derived rate-limited/credit-exhausted). This batch
//! (Batch 3) adds Tier 2 (db-unreachable, missing-pacer-row, windowed
//! collection-queue-failures/backfill-failed, backfill-stalled, worker-crash-looping)
//! and Tier 3 (coins-stalled, db-pool-exhausted, upsert-failure-streak).
//!
//! The pure core — the registry-derived active-set computation and the
//! active→inactive transition detection that drives the optional fast-clear — is
//! factored out as plain functions so it is unit-testable without a DB or HTTP,
//! mirroring `pacer::pacer_decision`'s pure, separately-tested core.
//!
//! @MX:ANCHOR: [AUTO] the reconciler sweep — the load-bearing "raised but never
//! cleared is structurally impossible" guarantee.
//! @MX:REASON: every active condition MUST be re-raised each sweep with
//! `timeoutSeconds = ALARM_TTL_SECS`, and a recovered condition MUST simply stop being
//! refreshed so the server auto-clears it. This depends on no in-memory reported-set,
//! so it survives restarts with no startup seeding (REQ-ALARM-011..018).
//! @MX:WARN: [AUTO] the sweep loop holds an `Arc<Reconciler>` shared across the
//! supervised task; `previously_active` is a `Mutex<HashMap>` mutated every sweep.
//! @MX:REASON: losing/poisoning this map does not affect correctness (server TTL still
//! clears everything) — it only degrades the optional fast-clear latency optimisation.

use crate::alarm::catalog::{self, Condition, Severity};
use crate::alarm::registry::{HealthRegistry, ProviderHealth};
use crate::alarm::AlarmClient;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tracing::{error, info};

// ── Pure core ──────────────────────────────────────────────────────────────────

/// Is a single provider's Tier 1 `provider-unreachable` condition active (REQ-ALARM-020)?
///
/// Active when the provider has recorded at least one consecutive network failure AND
/// either it has never succeeded, or its last success is at least `threshold` old.
pub fn provider_unreachable_active(
    snapshot: &ProviderHealth,
    now: Instant,
    threshold: Duration,
) -> bool {
    if snapshot.consecutive_network_failures == 0 {
        return false;
    }
    match snapshot.last_success_at {
        None => true,
        Some(last) => now.saturating_duration_since(last) >= threshold,
    }
}

/// Pure: the Tier 1 desired-active-set derivable from the in-memory registry alone
/// (provider-unreachable + all-providers-down). Pacer-derived Tier 1 conditions
/// (rate-limited, credit-exhausted) require a DB read; see [`pacer_rows_to_conditions`].
pub fn registry_desired_conditions(
    registry: &HealthRegistry,
    now: Instant,
    provider_unreachable_threshold: Duration,
) -> Vec<Condition> {
    let mut conditions = Vec::new();
    for provider in registry.tracked_providers() {
        let snap = registry.provider_snapshot(&provider);
        if provider_unreachable_active(&snap, now, provider_unreachable_threshold) {
            conditions.push(Condition::ProviderUnreachable { provider });
        }
    }
    if registry.all_providers_down() {
        conditions.push(Condition::AllProvidersDown);
    }
    conditions
}

/// A row read from `upstream_request_pacer` (Tier 1 pacer-derived conditions).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PacerRow {
    pub provider: String,
    pub cooldown_until: Option<DateTime<Utc>>,
    pub credit_limit: Option<i64>,
    pub credits_used: i64,
}

/// Pure: derive `provider-rate-limited` (REQ-ALARM-021) and
/// `provider-credit-exhausted` (REQ-ALARM-023) conditions from a snapshot of
/// `upstream_request_pacer` rows. Testable without a DB by constructing `PacerRow`
/// values directly.
pub fn pacer_rows_to_conditions(rows: &[PacerRow], now: DateTime<Utc>) -> Vec<Condition> {
    let mut conditions = Vec::new();
    for row in rows {
        if let Some(until) = row.cooldown_until {
            if until > now {
                conditions.push(Condition::ProviderRateLimited {
                    provider: row.provider.clone(),
                });
            }
        }
        if let Some(limit) = row.credit_limit {
            if row.credits_used >= limit {
                conditions.push(Condition::ProviderCreditExhausted {
                    provider: row.provider.clone(),
                });
            }
        }
    }
    conditions
}

// ── Tier 2/3 pure core (Batch 3) ─────────────────────────────────────────────────

/// Pure sustained-timer update: given whether the raw condition holds right now and the
/// previous "since" marker (`None` = not currently ongoing), returns the updated marker.
/// A `false` reading always resets to `None`; a `true` reading starts the marker on the
/// first `true` observation and holds it steady on subsequent `true` observations.
/// Shared by `db-unreachable` (REQ-ALARM-030) and `db-pool-exhausted` (REQ-ALARM-041),
/// which both need a "how long has this been continuously true" signal that cannot be
/// derived from a single point-in-time DB/pool read.
pub fn sustained_state_update(
    condition_now: bool,
    since: Option<Instant>,
    now: Instant,
) -> Option<Instant> {
    if condition_now {
        Some(since.unwrap_or(now))
    } else {
        None
    }
}

/// Pure: is a sustained-timer marker active (i.e. has held continuously for at least
/// `threshold`)? `None` (not currently ongoing) is never active.
pub fn sustained_active(since: Option<Instant>, now: Instant, threshold: Duration) -> bool {
    match since {
        Some(t) => now.saturating_duration_since(t) >= threshold,
        None => false,
    }
}

/// Pure: derive `missing-pacer-row` conditions (REQ-ALARM-031) by comparing the
/// configured provider names against the providers present in a snapshot of
/// `upstream_request_pacer` rows.
pub fn missing_pacer_conditions(
    configured: &[String],
    present_rows: &[PacerRow],
) -> Vec<Condition> {
    let present: std::collections::HashSet<&str> =
        present_rows.iter().map(|r| r.provider.as_str()).collect();
    configured
        .iter()
        .filter(|name| !present.contains(name.as_str()))
        .map(|name| Condition::MissingPacerRow {
            provider: name.clone(),
        })
        .collect()
}

/// Pure: derive `worker-crash-looping` conditions (REQ-ALARM-034) from each tracked
/// worker's in-window restart-event count.
pub fn worker_crashloop_conditions(
    registry: &HealthRegistry,
    now: Instant,
    window: Duration,
    threshold: u32,
) -> Vec<Condition> {
    registry
        .tracked_workers()
        .into_iter()
        .filter(|worker| registry.worker_restart_count_in_window(worker, now, window) >= threshold)
        .map(|worker| Condition::WorkerCrashLooping { worker })
        .collect()
}

/// Pure: is the windowed collection-queue failure-rate signal active (REQ-ALARM-032)?
pub fn queue_failures_active(windowed_count: i64, threshold: u32) -> bool {
    windowed_count >= threshold as i64
}

/// Pure: is the windowed backfill-chunk failure-rate signal active (REQ-ALARM-033 row 8a)?
pub fn backfill_failed_active(windowed_count: i64, threshold: u32) -> bool {
    windowed_count >= threshold as i64
}

/// Pure: is the aggregated `coins-stalled` signal active (REQ-ALARM-040)?
pub fn coins_stalled_active(stale_count: i64, threshold: u32) -> bool {
    stale_count >= threshold as i64
}

/// Pure: is the DB pool currently saturated (no idle connections at full size),
/// independent of how long it has been so (REQ-ALARM-041 raw signal; the sustained
/// timer is layered on top via [`sustained_state_update`]/[`sustained_active`]).
pub fn pool_saturated(size: u32, num_idle: usize, max_connections: u32) -> bool {
    num_idle == 0 && size >= max_connections
}

/// Pure: is the `db-upsert-failures` signal active (REQ-ALARM-042)?
pub fn upsert_failures_active(streak: u32, threshold: u32) -> bool {
    streak >= threshold
}

/// The reconciler's per-sweep decision.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SweepActions {
    /// Fingerprints that are active this sweep (raised/heartbeated).
    pub to_raise: Vec<String>,
    /// Fingerprints that dropped out of the active set AND are Critical/Error
    /// severity — eligible for the optional immediate fast-clear (REQ-ALARM-014).
    pub fast_clear: Vec<String>,
}

/// Pure: given this sweep's desired-active `(fingerprint, severity)` pairs and the
/// previous sweep's active `(fingerprint, severity)` map, compute this sweep's
/// actions. Every desired fingerprint is (re-)raised (REQ-ALARM-013/015 — dedup
/// heartbeat is server-side, so an unchanged sweep produces no duplicates,
/// REQ-ALARM-017). A fingerprint present in `previously_active` but absent from
/// `desired` has recovered; if its previous severity was Critical or Error it
/// qualifies for the optional fast-clear (REQ-ALARM-014) — Warning conditions rely on
/// TTL expiry alone and are never fast-cleared.
pub fn compute_sweep_actions(
    desired: &[(String, Severity)],
    previously_active: &HashMap<String, Severity>,
) -> SweepActions {
    let desired_map: HashMap<&str, Severity> = desired
        .iter()
        .map(|(fp, sev)| (fp.as_str(), *sev))
        .collect();

    let mut fast_clear: Vec<String> = previously_active
        .iter()
        .filter(|(fp, _)| !desired_map.contains_key(fp.as_str()))
        .filter(|(_, sev)| matches!(sev, Severity::Critical | Severity::Error))
        .map(|(fp, _)| fp.clone())
        .collect();
    fast_clear.sort();

    SweepActions {
        to_raise: desired.iter().map(|(fp, _)| fp.clone()).collect(),
        fast_clear,
    }
}

// ── Async sweep runner ────────────────────────────────────────────────────────

/// Thresholds read once per sweep from `crate::config` (env vars).
struct Thresholds {
    provider_unreachable: Duration,
    db_unreachable: Duration,
    queue_failed_threshold: u32,
    queue_failed_window_secs: i64,
    backfill_failed_threshold: u32,
    backfill_failed_window_secs: i64,
    backfill_stall_secs: i64,
    worker_crashloop_threshold: u32,
    worker_crashloop_window: Duration,
    coin_staleness_secs: i64,
    coins_stalled_threshold: u32,
    db_pool_saturation: Duration,
    upsert_failure_streak: u32,
}

impl Thresholds {
    fn from_config() -> Self {
        Self {
            provider_unreachable: Duration::from_secs(
                crate::config::alarm_provider_unreachable_secs(),
            ),
            db_unreachable: Duration::from_secs(crate::config::alarm_db_unreachable_secs()),
            queue_failed_threshold: crate::config::alarm_queue_failed_threshold(),
            queue_failed_window_secs: crate::config::alarm_queue_failed_window_secs() as i64,
            backfill_failed_threshold: crate::config::alarm_backfill_failed_threshold(),
            backfill_failed_window_secs: crate::config::alarm_backfill_failed_window_secs() as i64,
            backfill_stall_secs: crate::config::alarm_backfill_stall_secs() as i64,
            worker_crashloop_threshold: crate::config::alarm_worker_crashloop_threshold(),
            worker_crashloop_window: Duration::from_secs(
                crate::config::alarm_worker_crashloop_window_secs(),
            ),
            coin_staleness_secs: crate::config::alarm_coin_staleness_secs() as i64,
            coins_stalled_threshold: crate::config::alarm_coins_stalled_threshold(),
            db_pool_saturation: Duration::from_secs(crate::config::alarm_db_pool_saturation_secs()),
            upsert_failure_streak: crate::config::alarm_upsert_failure_streak(),
        }
    }
}

/// The near-stateless reconciler (REQ-ALARM-012): the Alarm Center (via TTL) is the
/// source of truth for what is currently active, so this struct needs no
/// correctness-critical record of what it raised. `previously_active` is kept ONLY to
/// detect active→inactive transitions for the optional Critical/Error fast-clear
/// (REQ-ALARM-014); losing it (e.g. on restart) does not affect correctness.
///
/// `db_unreachable_since`/`pool_saturation_since` are sustained-timer markers for the
/// two conditions whose active signal needs "how long has this held continuously" —
/// they are NOT correctness-critical either: losing them on restart merely resets the
/// sustained clock (worst case, one extra reconcile interval before the alarm fires).
pub struct Reconciler {
    client: Arc<AlarmClient>,
    registry: Arc<HealthRegistry>,
    pool: PgPool,
    interval: Duration,
    previously_active: Mutex<HashMap<String, Severity>>,
    db_unreachable_since: Mutex<Option<Instant>>,
    pool_saturation_since: Mutex<Option<Instant>>,
    first_sweep_done: std::sync::atomic::AtomicBool,
}

impl Reconciler {
    pub fn new(
        client: Arc<AlarmClient>,
        registry: Arc<HealthRegistry>,
        pool: PgPool,
        interval: Duration,
    ) -> Self {
        Self {
            client,
            registry,
            pool,
            interval,
            previously_active: Mutex::new(HashMap::new()),
            db_unreachable_since: Mutex::new(None),
            pool_saturation_since: Mutex::new(None),
            first_sweep_done: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Fetch a snapshot of `upstream_request_pacer` rows once per sweep, shared by both
    /// the Tier 1 pacer-derived conditions (rate-limited/credit-exhausted) and the Tier 2
    /// `missing-pacer-row` comparison (REQ-ALARM-021/023/031).
    async fn pacer_rows(&self) -> Option<Vec<PacerRow>> {
        match sqlx::query_as::<_, PacerRow>(
            "SELECT provider, cooldown_until, credit_limit, credits_used \
             FROM upstream_request_pacer",
        )
        .fetch_all(&self.pool)
        .await
        {
            Ok(rows) => Some(rows),
            Err(e) => {
                error!(
                    error = %e,
                    "reconciler: failed to read upstream_request_pacer; skipping \
                     pacer-derived conditions this sweep"
                );
                None
            }
        }
    }

    /// Readiness-style DB ping (REQ-ALARM-030 active signal). `true` = reachable.
    async fn db_ping_ok(&self) -> bool {
        sqlx::query("SELECT 1").fetch_one(&self.pool).await.is_ok()
    }

    /// Windowed count of `collection_queue` rows with `status='failed'` whose
    /// `updated_at` falls inside the configured window (REQ-ALARM-032). `None` on query
    /// error (skip this condition this sweep rather than propagate).
    async fn queue_failed_windowed_count(&self, window_secs: i64) -> Option<i64> {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM collection_queue \
             WHERE status = 'failed' AND updated_at > now() - ($1 * INTERVAL '1 second')",
        )
        .bind(window_secs)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            error!(error = %e, "reconciler: failed to count collection_queue failures");
        })
        .ok()
    }

    /// Windowed count of `backfill_chunks` rows with `status='failed'` whose
    /// `updated_at` falls inside the configured window (REQ-ALARM-033 row 8a).
    async fn backfill_failed_windowed_count(&self, window_secs: i64) -> Option<i64> {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM backfill_chunks \
             WHERE status = 'failed' AND updated_at > now() - ($1 * INTERVAL '1 second')",
        )
        .bind(window_secs)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            error!(error = %e, "reconciler: failed to count backfill_chunks failures");
        })
        .ok()
    }

    /// Is `backfill-stalled` active (REQ-ALARM-033 row 8b)? Active when pending/claimed/
    /// running chunks exist whose `updated_at` has not moved for at least `stall_secs` —
    /// every successful progress path (`ADVANCE_CURSOR_SQL`, `COMPLETE_BACKFILL_SQL`,
    /// `FAIL_OR_RETRY_BACKFILL_SQL`) touches `updated_at`, so a stale `updated_at` on a
    /// still-open chunk means no progress has been made in that time.
    async fn backfill_stalled_active(&self, stall_secs: i64) -> Option<bool> {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM backfill_chunks \
             WHERE status IN ('pending', 'claimed', 'running') \
               AND updated_at < now() - ($1 * INTERVAL '1 second')",
        )
        .bind(stall_secs)
        .fetch_one(&self.pool)
        .await
        .map(|count| count > 0)
        .map_err(|e| {
            error!(error = %e, "reconciler: failed to check backfill stall");
        })
        .ok()
    }

    /// Count of `tracked_coins` (active) whose `last_polled_at`/`last_collected_at` is
    /// stale beyond `staleness_secs` (REQ-ALARM-040). A coin with neither timestamp set
    /// (never collected) counts as stale.
    async fn coins_stalled_count(&self, staleness_secs: i64) -> Option<i64> {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM tracked_coins \
             WHERE status = 'active' \
               AND COALESCE(last_polled_at, last_collected_at, 'epoch'::timestamptz) \
                   < now() - ($1 * INTERVAL '1 second')",
        )
        .bind(staleness_secs)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            error!(error = %e, "reconciler: failed to count stalled tracked_coins");
        })
        .ok()
    }

    /// Run exactly one sweep (REQ-ALARM-011): compute the full Tier 1/2/3 desired state,
    /// raise every active condition with the TTL, and fire the optional Critical/Error
    /// fast-clear for any fingerprint that dropped out of the active set since the last
    /// sweep.
    pub async fn sweep_once(&self) {
        let thresholds = Thresholds::from_config();
        let now = Instant::now();

        // On the first-ever sweep, best-effort fast-clear the fatal startup-config
        // alarm (if `main` raised it before this reconciler existed): a healthy start
        // proves config parsed. This is optional (REQ-ALARM-035/D2a) — the alarm
        // auto-expires via TTL regardless if this is dropped or skipped.
        if !self
            .first_sweep_done
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.client
                .clear(&catalog::fingerprint(&Condition::StartupConfigError))
                .await;
        }

        let mut conditions =
            registry_desired_conditions(&self.registry, now, thresholds.provider_unreachable);

        // Pacer-derived: Tier 1 (rate-limited/credit-exhausted) + Tier 2 (missing-pacer-row).
        if let Some(rows) = self.pacer_rows().await {
            conditions.extend(pacer_rows_to_conditions(&rows, Utc::now()));
            conditions.extend(missing_pacer_conditions(
                &crate::config::provider_names(),
                &rows,
            ));
        }

        // db-unreachable (REQ-ALARM-030): sustained-timer over the readiness ping.
        let db_ok = self.db_ping_ok().await;
        {
            let mut since = self
                .db_unreachable_since
                .lock()
                .expect("db_unreachable_since lock poisoned");
            *since = sustained_state_update(!db_ok, *since, now);
            if sustained_active(*since, now, thresholds.db_unreachable) {
                conditions.push(Condition::DbUnreachable);
            }
        }

        // The remaining SQL-derived conditions all need the DB; skip them gracefully
        // (matching db-unreachable's own tolerance) if it is currently unreachable —
        // each helper already logs and returns `None` on query failure.
        if let Some(count) = self
            .queue_failed_windowed_count(thresholds.queue_failed_window_secs)
            .await
        {
            if queue_failures_active(count, thresholds.queue_failed_threshold) {
                conditions.push(Condition::CollectionQueueFailures);
            }
        }

        if let Some(count) = self
            .backfill_failed_windowed_count(thresholds.backfill_failed_window_secs)
            .await
        {
            if backfill_failed_active(count, thresholds.backfill_failed_threshold) {
                conditions.push(Condition::BackfillFailed);
            }
        }

        if let Some(true) = self
            .backfill_stalled_active(thresholds.backfill_stall_secs)
            .await
        {
            conditions.push(Condition::BackfillStalled);
        }

        // worker-crash-looping (REQ-ALARM-034): registry-derived, no DB needed.
        conditions.extend(worker_crashloop_conditions(
            &self.registry,
            now,
            thresholds.worker_crashloop_window,
            thresholds.worker_crashloop_threshold,
        ));

        // coins-stalled (REQ-ALARM-040): single aggregated alarm; the stalled count is
        // attached to the raised spec's `details` below (catalogue requirement).
        let mut coins_stalled_count_value: Option<i64> = None;
        if let Some(count) = self
            .coins_stalled_count(thresholds.coin_staleness_secs)
            .await
        {
            if coins_stalled_active(count, thresholds.coins_stalled_threshold) {
                conditions.push(Condition::CoinsStalled);
                coins_stalled_count_value = Some(count);
            }
        }

        // db-pool-exhausted (REQ-ALARM-041): sync pool sampling, no I/O, sustained timer.
        {
            let saturated = pool_saturated(
                self.pool.size(),
                self.pool.num_idle(),
                crate::db::max_connections(),
            );
            let mut since = self
                .pool_saturation_since
                .lock()
                .expect("pool_saturation_since lock poisoned");
            *since = sustained_state_update(saturated, *since, now);
            if sustained_active(*since, now, thresholds.db_pool_saturation) {
                conditions.push(Condition::DbPoolExhausted);
            }
        }

        // db-upsert-failures (REQ-ALARM-042): registry-derived, no DB needed.
        if upsert_failures_active(
            self.registry.upsert_failure_streak(),
            thresholds.upsert_failure_streak,
        ) {
            conditions.push(Condition::DbUpsertFailures);
        }

        let desired: Vec<(String, Severity)> = conditions
            .iter()
            .map(|c| (catalog::fingerprint(c), catalog::severity(c)))
            .collect();

        let actions = {
            let previously_active = self
                .previously_active
                .lock()
                .expect("reconciler previously_active lock poisoned");
            compute_sweep_actions(&desired, &previously_active)
        };

        // Raise/heartbeat every active condition (REQ-ALARM-013/015).
        for condition in &conditions {
            let mut spec = catalog::to_alarm_spec(condition);
            // REQ-ALARM-040: coins-stalled carries the stalled-coin count in `details`
            // (a documented exception to the per-target fingerprint default, OR-ALARM-3).
            if matches!(condition, Condition::CoinsStalled) {
                if let Some(count) = coins_stalled_count_value {
                    spec.details
                        .insert("stalled_count".to_string(), count.to_string());
                }
            }
            self.client.raise(&spec).await;
        }

        // Optional Critical/Error fast-clear on observed active→inactive transitions
        // (REQ-ALARM-014). A dropped fast-clear falls back to TTL expiry
        // (REQ-ALARM-016) — no compensating action is taken here.
        for fingerprint in &actions.fast_clear {
            self.client.clear(fingerprint).await;
        }

        let mut previously_active = self
            .previously_active
            .lock()
            .expect("reconciler previously_active lock poisoned");
        *previously_active = desired.into_iter().collect();
    }
}

/// Run the reconciler sweep loop on `reconciler.interval()` until shutdown
/// (REQ-ALARM-010/011). On shutdown it simply stops between sweeps — no mass-clear,
/// since every alarm it raised carries a TTL and auto-expires unless refreshed
/// (REQ-ALARM-018).
pub async fn run_reconciler(reconciler: Arc<Reconciler>, mut shutdown: watch::Receiver<bool>) {
    let mut ticker = tokio::time::interval(reconciler.interval());
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                reconciler.sweep_once().await;
            }
            _ = shutdown.changed() => {
                info!("reconciler: shutdown signal received; stopping (no mass-clear, REQ-ALARM-018)");
                break;
            }
        }
        if *shutdown.borrow() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── provider_unreachable_active (REQ-ALARM-020) ────────────────────────────

    #[test]
    fn no_failures_recorded_is_never_active() {
        let snap = ProviderHealth {
            last_success_at: None,
            consecutive_network_failures: 0,
        };
        assert!(!provider_unreachable_active(
            &snap,
            Instant::now(),
            Duration::from_secs(300)
        ));
    }

    #[test]
    fn never_succeeded_with_failures_is_active() {
        let snap = ProviderHealth {
            last_success_at: None,
            consecutive_network_failures: 3,
        };
        assert!(provider_unreachable_active(
            &snap,
            Instant::now(),
            Duration::from_secs(300)
        ));
    }

    #[test]
    fn recent_success_within_threshold_is_not_active() {
        let now = Instant::now();
        let snap = ProviderHealth {
            last_success_at: Some(now - Duration::from_secs(10)),
            consecutive_network_failures: 5,
        };
        assert!(!provider_unreachable_active(
            &snap,
            now,
            Duration::from_secs(300)
        ));
    }

    #[test]
    fn stale_success_beyond_threshold_is_active() {
        let now = Instant::now();
        let snap = ProviderHealth {
            last_success_at: Some(now - Duration::from_secs(400)),
            consecutive_network_failures: 5,
        };
        assert!(provider_unreachable_active(
            &snap,
            now,
            Duration::from_secs(300)
        ));
    }

    // ── registry_desired_conditions ─────────────────────────────────────────────

    #[test]
    fn registry_desired_conditions_raises_all_with_ttl_semantics() {
        // "raises all with TTL" here means: every currently-active condition appears
        // in the desired set (the caller re-raises each with timeoutSeconds every
        // sweep — see Reconciler::sweep_once).
        let reg = HealthRegistry::new();
        reg.record_provider_network_failure("binance");
        // No success recorded → active regardless of threshold.
        let conditions =
            registry_desired_conditions(&reg, Instant::now(), Duration::from_secs(300));
        assert_eq!(
            conditions,
            vec![Condition::ProviderUnreachable {
                provider: "binance".to_string()
            }]
        );
    }

    #[test]
    fn registry_desired_conditions_drop_out_when_recovered() {
        let reg = HealthRegistry::new();
        reg.record_provider_network_failure("binance");
        reg.record_provider_success("binance");
        let conditions =
            registry_desired_conditions(&reg, Instant::now(), Duration::from_secs(300));
        assert!(
            conditions.is_empty(),
            "recovered provider must not be raised"
        );
    }

    #[test]
    fn registry_desired_conditions_includes_all_providers_down() {
        let reg = HealthRegistry::new();
        reg.record_chain_all_failed();
        let conditions =
            registry_desired_conditions(&reg, Instant::now(), Duration::from_secs(300));
        assert!(conditions.contains(&Condition::AllProvidersDown));
    }

    #[test]
    fn registry_desired_conditions_unchanged_sweep_is_stable() {
        // Two consecutive computations over unchanged registry state produce the same
        // set — no duplicate intent (REQ-ALARM-017 is enforced server-side, but the
        // desired-set computation itself must be deterministic).
        let reg = HealthRegistry::new();
        reg.record_provider_network_failure("binance");
        let now = Instant::now();
        let first = registry_desired_conditions(&reg, now, Duration::from_secs(300));
        let second = registry_desired_conditions(&reg, now, Duration::from_secs(300));
        assert_eq!(first, second);
    }

    // ── pacer_rows_to_conditions (REQ-ALARM-021/023) ────────────────────────────

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    #[test]
    fn pacer_row_with_future_cooldown_is_rate_limited() {
        let rows = vec![PacerRow {
            provider: "binance".to_string(),
            cooldown_until: Some(now() + chrono::Duration::seconds(60)),
            credit_limit: None,
            credits_used: 0,
        }];
        let conditions = pacer_rows_to_conditions(&rows, now());
        assert_eq!(
            conditions,
            vec![Condition::ProviderRateLimited {
                provider: "binance".to_string()
            }]
        );
    }

    #[test]
    fn pacer_row_with_past_cooldown_is_not_rate_limited() {
        let rows = vec![PacerRow {
            provider: "binance".to_string(),
            cooldown_until: Some(now() - chrono::Duration::seconds(60)),
            credit_limit: None,
            credits_used: 0,
        }];
        let conditions = pacer_rows_to_conditions(&rows, now());
        assert!(conditions.is_empty());
    }

    #[test]
    fn pacer_row_with_no_cooldown_is_not_rate_limited() {
        let rows = vec![PacerRow {
            provider: "binance".to_string(),
            cooldown_until: None,
            credit_limit: None,
            credits_used: 0,
        }];
        let conditions = pacer_rows_to_conditions(&rows, now());
        assert!(conditions.is_empty());
    }

    #[test]
    fn pacer_row_credits_at_limit_is_credit_exhausted() {
        let rows = vec![PacerRow {
            provider: "coingecko".to_string(),
            cooldown_until: None,
            credit_limit: Some(100),
            credits_used: 100,
        }];
        let conditions = pacer_rows_to_conditions(&rows, now());
        assert_eq!(
            conditions,
            vec![Condition::ProviderCreditExhausted {
                provider: "coingecko".to_string()
            }]
        );
    }

    #[test]
    fn pacer_row_credits_under_limit_is_not_exhausted() {
        let rows = vec![PacerRow {
            provider: "coingecko".to_string(),
            cooldown_until: None,
            credit_limit: Some(100),
            credits_used: 99,
        }];
        let conditions = pacer_rows_to_conditions(&rows, now());
        assert!(conditions.is_empty());
    }

    #[test]
    fn pacer_row_no_credit_limit_never_exhausted() {
        let rows = vec![PacerRow {
            provider: "coingecko".to_string(),
            cooldown_until: None,
            credit_limit: None,
            credits_used: 1_000_000,
        }];
        let conditions = pacer_rows_to_conditions(&rows, now());
        assert!(conditions.is_empty());
    }

    #[test]
    fn pacer_row_can_be_both_rate_limited_and_credit_exhausted() {
        let rows = vec![PacerRow {
            provider: "binance".to_string(),
            cooldown_until: Some(now() + chrono::Duration::seconds(30)),
            credit_limit: Some(50),
            credits_used: 50,
        }];
        let conditions = pacer_rows_to_conditions(&rows, now());
        assert_eq!(conditions.len(), 2);
        assert!(conditions.contains(&Condition::ProviderRateLimited {
            provider: "binance".to_string()
        }));
        assert!(conditions.contains(&Condition::ProviderCreditExhausted {
            provider: "binance".to_string()
        }));
    }

    // ── compute_sweep_actions (REQ-ALARM-011..018) ──────────────────────────────

    #[test]
    fn compute_sweep_actions_raises_every_desired_fingerprint() {
        let desired = vec![
            (
                "crypto-collector:provider-unreachable:binance".to_string(),
                Severity::Warning,
            ),
            (
                "crypto-collector:all-providers-down".to_string(),
                Severity::Critical,
            ),
        ];
        let previously_active = HashMap::new();
        let actions = compute_sweep_actions(&desired, &previously_active);
        let mut raised = actions.to_raise.clone();
        raised.sort();
        assert_eq!(
            raised,
            vec![
                "crypto-collector:all-providers-down".to_string(),
                "crypto-collector:provider-unreachable:binance".to_string(),
            ]
        );
        assert!(actions.fast_clear.is_empty());
    }

    #[test]
    fn compute_sweep_actions_drop_out_produces_no_fast_clear_for_warning() {
        let mut previously_active = HashMap::new();
        previously_active.insert(
            "crypto-collector:provider-unreachable:binance".to_string(),
            Severity::Warning,
        );
        let desired: Vec<(String, Severity)> = vec![];
        let actions = compute_sweep_actions(&desired, &previously_active);
        assert!(actions.to_raise.is_empty());
        assert!(
            actions.fast_clear.is_empty(),
            "Warning severities rely on TTL expiry alone, never fast-cleared"
        );
    }

    #[test]
    fn compute_sweep_actions_drop_out_fires_fast_clear_for_critical() {
        let mut previously_active = HashMap::new();
        previously_active.insert(
            "crypto-collector:all-providers-down".to_string(),
            Severity::Critical,
        );
        let desired: Vec<(String, Severity)> = vec![];
        let actions = compute_sweep_actions(&desired, &previously_active);
        assert_eq!(
            actions.fast_clear,
            vec!["crypto-collector:all-providers-down".to_string()]
        );
    }

    #[test]
    fn compute_sweep_actions_drop_out_fires_fast_clear_for_error() {
        let mut previously_active = HashMap::new();
        previously_active.insert(
            "crypto-collector:provider-credit-exhausted:kraken".to_string(),
            Severity::Error,
        );
        let desired: Vec<(String, Severity)> = vec![];
        let actions = compute_sweep_actions(&desired, &previously_active);
        assert_eq!(
            actions.fast_clear,
            vec!["crypto-collector:provider-credit-exhausted:kraken".to_string()]
        );
    }

    #[test]
    fn compute_sweep_actions_unchanged_active_condition_produces_no_fast_clear() {
        let mut previously_active = HashMap::new();
        previously_active.insert(
            "crypto-collector:all-providers-down".to_string(),
            Severity::Critical,
        );
        let desired = vec![(
            "crypto-collector:all-providers-down".to_string(),
            Severity::Critical,
        )];
        let actions = compute_sweep_actions(&desired, &previously_active);
        assert!(
            actions.fast_clear.is_empty(),
            "a condition still active must not be fast-cleared"
        );
        assert_eq!(
            actions.to_raise,
            vec!["crypto-collector:all-providers-down".to_string()]
        );
    }

    #[test]
    fn compute_sweep_actions_newly_active_condition_is_raised_not_fast_cleared() {
        let previously_active = HashMap::new();
        let desired = vec![(
            "crypto-collector:db-unreachable".to_string(),
            Severity::Critical,
        )];
        let actions = compute_sweep_actions(&desired, &previously_active);
        assert_eq!(
            actions.to_raise,
            vec!["crypto-collector:db-unreachable".to_string()]
        );
        assert!(actions.fast_clear.is_empty());
    }

    // ── DB-gated integration tests (require live DATABASE_URL) ──────────────────
    // Follow the project's existing inline `#[ignore]` + DATABASE_URL convention
    // (see `pacer::tests::setup_db`).

    async fn setup_db() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL must be set for reconciler integration tests");
        crate::db::connect(&url).await.expect("db connect")
    }

    fn test_client() -> Arc<AlarmClient> {
        // Points at an address nothing listens on; raise()/clear() will fail fast,
        // retry, and swallow the error (REQ-ALARM-007) — the DB-integration tests
        // below only assert on the desired-state derivation, not on delivery.
        Arc::new(AlarmClient::new("http://127.0.0.1:0", None, 50, 0, 75))
    }

    /// Scenario (REQ-ALARM-021): seeding a future `cooldown_until` makes
    /// `provider-rate-limited` active; clearing it makes the condition inactive.
    #[tokio::test]
    #[ignore]
    async fn db_pacer_derived_rate_limited_becomes_active_and_clears() {
        let pool = setup_db().await;

        sqlx::query(
            "UPDATE upstream_request_pacer \
             SET cooldown_until = now() + interval '60 seconds' \
             WHERE provider = 'binance'",
        )
        .execute(&pool)
        .await
        .expect("seed cooldown");

        let reconciler = Reconciler::new(
            test_client(),
            Arc::new(HealthRegistry::new()),
            pool.clone(),
            Duration::from_secs(30),
        );
        let rows = reconciler.pacer_rows().await.expect("pacer rows");
        let conditions = pacer_rows_to_conditions(&rows, Utc::now());
        assert!(conditions.contains(&Condition::ProviderRateLimited {
            provider: "binance".to_string()
        }));

        sqlx::query(
            "UPDATE upstream_request_pacer SET cooldown_until = NULL WHERE provider = 'binance'",
        )
        .execute(&pool)
        .await
        .expect("clear cooldown");

        let rows = reconciler.pacer_rows().await.expect("pacer rows");
        let conditions = pacer_rows_to_conditions(&rows, Utc::now());
        assert!(!conditions.contains(&Condition::ProviderRateLimited {
            provider: "binance".to_string()
        }));
    }

    /// Scenario (REQ-ALARM-023): seeding `credits_used >= credit_limit` makes
    /// `provider-credit-exhausted` active; resetting the window clears it.
    #[tokio::test]
    #[ignore]
    async fn db_pacer_derived_credit_exhausted_becomes_active_and_clears() {
        let pool = setup_db().await;

        sqlx::query(
            "UPDATE upstream_request_pacer \
             SET credit_limit = 100, credits_used = 100 \
             WHERE provider = 'coingecko'",
        )
        .execute(&pool)
        .await
        .expect("seed exhausted credits");

        let reconciler = Reconciler::new(
            test_client(),
            Arc::new(HealthRegistry::new()),
            pool.clone(),
            Duration::from_secs(30),
        );
        let rows = reconciler.pacer_rows().await.expect("pacer rows");
        let conditions = pacer_rows_to_conditions(&rows, Utc::now());
        assert!(conditions.contains(&Condition::ProviderCreditExhausted {
            provider: "coingecko".to_string()
        }));

        sqlx::query(
            "UPDATE upstream_request_pacer SET credits_used = 0 WHERE provider = 'coingecko'",
        )
        .execute(&pool)
        .await
        .expect("reset credits");

        let rows = reconciler.pacer_rows().await.expect("pacer rows");
        let conditions = pacer_rows_to_conditions(&rows, Utc::now());
        assert!(!conditions.contains(&Condition::ProviderCreditExhausted {
            provider: "coingecko".to_string()
        }));
    }

    /// Scenario (REQ-ALARM-011/013): a full sweep against a live DB does not panic and
    /// completes even though the client cannot reach a real alarm center (best-effort
    /// delivery swallows the error, REQ-ALARM-007).
    #[tokio::test]
    #[ignore]
    async fn db_sweep_once_completes_without_panicking() {
        let pool = setup_db().await;
        let registry = Arc::new(HealthRegistry::new());
        registry.record_provider_network_failure("binance");

        let reconciler = Reconciler::new(test_client(), registry, pool, Duration::from_secs(30));
        reconciler.sweep_once().await;
    }

    // ── Tier 2 DB-gated integration tests (Batch 3, Milestone 6) ────────────────

    /// Scenario 16 (REQ-ALARM-032): the windowed collection-queue failure count becomes
    /// active at/above the threshold and recovers once no new failures land in the
    /// window (a fresh row seeded now falls back out once the window is shrunk to 0).
    #[tokio::test]
    #[ignore]
    async fn db_queue_failed_windowed_count_recovers_outside_window() {
        let pool = setup_db().await;
        sqlx::query(
            "INSERT INTO collection_queue \
                (target_kind, target_id, kind, status, enqueued_at, updated_at) \
             VALUES ('coin', 'alarm-test-coin', 'candles', 'failed', now(), now())",
        )
        .execute(&pool)
        .await
        .expect("seed a failed row");

        let reconciler = Reconciler::new(
            test_client(),
            Arc::new(HealthRegistry::new()),
            pool.clone(),
            Duration::from_secs(30),
        );

        let count_wide = reconciler
            .queue_failed_windowed_count(3600)
            .await
            .expect("count");
        assert!(
            count_wide >= 1,
            "seeded row must be counted within a wide window"
        );

        let count_zero = reconciler
            .queue_failed_windowed_count(0)
            .await
            .expect("count");
        assert_eq!(
            count_zero, 0,
            "a zero-width window must exclude the just-seeded row (detection recovers \
             despite the terminal 'failed' row persisting, B1)"
        );

        sqlx::query("DELETE FROM collection_queue WHERE target_id = 'alarm-test-coin'")
            .execute(&pool)
            .await
            .expect("cleanup");
    }

    /// Scenario 17a (REQ-ALARM-033 row 8a): same windowed-recovery shape for
    /// `backfill_chunks`.
    #[tokio::test]
    #[ignore]
    async fn db_backfill_failed_windowed_count_recovers_outside_window() {
        let pool = setup_db().await;
        let job_id: i64 = sqlx::query_scalar(
            "INSERT INTO backfill_jobs (coin_id, dataset, status, requested_at, updated_at) \
             VALUES ('alarm-test-coin', 'candles', 'pending', now(), now()) \
             ON CONFLICT (coin_id, dataset) DO UPDATE SET updated_at = now() \
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("seed job");
        sqlx::query(
            "INSERT INTO backfill_chunks \
                (job_id, coin_id, dataset, interval, status, created_at, updated_at) \
             VALUES ($1, 'alarm-test-coin', 'candles', '1d', 'failed', now(), now())",
        )
        .bind(job_id)
        .execute(&pool)
        .await
        .expect("seed a failed chunk");

        let reconciler = Reconciler::new(
            test_client(),
            Arc::new(HealthRegistry::new()),
            pool.clone(),
            Duration::from_secs(30),
        );

        let count_wide = reconciler
            .backfill_failed_windowed_count(3600)
            .await
            .expect("count");
        assert!(count_wide >= 1);

        let count_zero = reconciler
            .backfill_failed_windowed_count(0)
            .await
            .expect("count");
        assert_eq!(
            count_zero, 0,
            "detection must recover outside the window (B1)"
        );

        sqlx::query("DELETE FROM backfill_chunks WHERE coin_id = 'alarm-test-coin'")
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM backfill_jobs WHERE coin_id = 'alarm-test-coin'")
            .execute(&pool)
            .await
            .ok();
    }

    /// Scenario 17b (REQ-ALARM-033 row 8b): a pending chunk whose `updated_at` is older
    /// than the stall threshold makes `backfill-stalled` active; a freshly-touched
    /// pending chunk does not.
    #[tokio::test]
    #[ignore]
    async fn db_backfill_stalled_active_on_stale_pending_chunk() {
        let pool = setup_db().await;
        let job_id: i64 = sqlx::query_scalar(
            "INSERT INTO backfill_jobs (coin_id, dataset, status, requested_at, updated_at) \
             VALUES ('alarm-test-stall', 'candles', 'pending', now(), now()) \
             ON CONFLICT (coin_id, dataset) DO UPDATE SET updated_at = now() \
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("seed job");
        sqlx::query(
            "INSERT INTO backfill_chunks \
                (job_id, coin_id, dataset, interval, status, created_at, updated_at) \
             VALUES ($1, 'alarm-test-stall', 'candles', '1d', 'pending', \
                     now() - interval '2 hours', now() - interval '2 hours')",
        )
        .bind(job_id)
        .execute(&pool)
        .await
        .expect("seed a stale pending chunk");

        let reconciler = Reconciler::new(
            test_client(),
            Arc::new(HealthRegistry::new()),
            pool.clone(),
            Duration::from_secs(30),
        );

        let stalled = reconciler
            .backfill_stalled_active(3600)
            .await
            .expect("stall check");
        assert!(
            stalled,
            "a 2h-stale pending chunk must trip a 1h stall threshold"
        );

        let not_stalled = reconciler
            .backfill_stalled_active(3 * 3600)
            .await
            .expect("stall check");
        assert!(
            !not_stalled,
            "a 2h-stale chunk must not trip a 3h stall threshold"
        );

        sqlx::query("DELETE FROM backfill_chunks WHERE coin_id = 'alarm-test-stall'")
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM backfill_jobs WHERE coin_id = 'alarm-test-stall'")
            .execute(&pool)
            .await
            .ok();
    }

    /// Scenario 15 (REQ-ALARM-031): a configured provider absent from
    /// `upstream_request_pacer` produces a `missing-pacer-row` condition.
    #[tokio::test]
    #[ignore]
    async fn db_missing_pacer_row_detected_by_comparison() {
        let pool = setup_db().await;
        let reconciler = Reconciler::new(
            test_client(),
            Arc::new(HealthRegistry::new()),
            pool.clone(),
            Duration::from_secs(30),
        );
        let rows = reconciler.pacer_rows().await.expect("pacer rows");
        let configured = vec![
            "binance".to_string(),
            "definitely-not-seeded-provider".to_string(),
        ];
        let conditions = missing_pacer_conditions(&configured, &rows);
        assert!(conditions.contains(&Condition::MissingPacerRow {
            provider: "definitely-not-seeded-provider".to_string()
        }));
        assert!(!conditions.contains(&Condition::MissingPacerRow {
            provider: "binance".to_string()
        }));
    }

    /// Scenario 14 (REQ-ALARM-030): `db_ping_ok` reflects a live, reachable database.
    #[tokio::test]
    #[ignore]
    async fn db_ping_ok_true_against_live_database() {
        let pool = setup_db().await;
        let reconciler = Reconciler::new(
            test_client(),
            Arc::new(HealthRegistry::new()),
            pool,
            Duration::from_secs(30),
        );
        assert!(reconciler.db_ping_ok().await);
    }

    // ── Tier 3 DB-gated integration tests (Batch 3, Milestone 7) ────────────────

    /// Scenario 20 (REQ-ALARM-040): a stale `tracked_coins` row is counted toward the
    /// aggregated `coins-stalled` signal; a fresh row is not.
    #[tokio::test]
    #[ignore]
    async fn db_coins_stalled_count_reflects_staleness() {
        let pool = setup_db().await;
        sqlx::query(
            "INSERT INTO tracked_coins (coin_id, symbol, name, status, last_polled_at) \
             VALUES ('alarm-test-stale-coin', 'ATSC', 'Alarm Test Stale Coin', 'active', \
                     now() - interval '2 hours') \
             ON CONFLICT (coin_id) DO UPDATE SET \
                 status = 'active', last_polled_at = now() - interval '2 hours'",
        )
        .execute(&pool)
        .await
        .expect("seed stale coin");

        let reconciler = Reconciler::new(
            test_client(),
            Arc::new(HealthRegistry::new()),
            pool.clone(),
            Duration::from_secs(30),
        );

        let count = reconciler.coins_stalled_count(3600).await.expect("count");
        assert!(count >= 1, "the seeded stale coin must be counted");

        sqlx::query(
            "UPDATE tracked_coins SET last_polled_at = now() \
             WHERE coin_id = 'alarm-test-stale-coin'",
        )
        .execute(&pool)
        .await
        .expect("refresh coin");
        let count_after_refresh = reconciler.coins_stalled_count(3600).await.expect("count");
        assert_eq!(
            count_after_refresh, 0,
            "a freshly-polled coin must no longer count as stale (this specific one)"
        );

        sqlx::query("DELETE FROM tracked_coins WHERE coin_id = 'alarm-test-stale-coin'")
            .execute(&pool)
            .await
            .ok();
    }

    // ── Pure Tier 2/3 core (no DB, no I/O) ───────────────────────────────────────

    #[test]
    fn sustained_state_update_starts_marker_on_first_true() {
        let now = Instant::now();
        let since = sustained_state_update(true, None, now);
        assert_eq!(since, Some(now));
    }

    #[test]
    fn sustained_state_update_holds_marker_steady_on_repeated_true() {
        let t0 = Instant::now();
        let since = sustained_state_update(true, Some(t0), t0 + Duration::from_secs(10));
        assert_eq!(
            since,
            Some(t0),
            "marker must not reset while condition stays true"
        );
    }

    #[test]
    fn sustained_state_update_resets_on_false() {
        let t0 = Instant::now();
        let since = sustained_state_update(false, Some(t0), t0 + Duration::from_secs(10));
        assert_eq!(since, None);
    }

    #[test]
    fn sustained_active_false_when_no_marker() {
        assert!(!sustained_active(
            None,
            Instant::now(),
            Duration::from_secs(60)
        ));
    }

    #[test]
    fn sustained_active_false_before_threshold() {
        let t0 = Instant::now();
        assert!(!sustained_active(
            Some(t0),
            t0 + Duration::from_secs(10),
            Duration::from_secs(60)
        ));
    }

    #[test]
    fn sustained_active_true_at_or_beyond_threshold() {
        let t0 = Instant::now();
        assert!(sustained_active(
            Some(t0),
            t0 + Duration::from_secs(60),
            Duration::from_secs(60)
        ));
        assert!(sustained_active(
            Some(t0),
            t0 + Duration::from_secs(120),
            Duration::from_secs(60)
        ));
    }

    #[test]
    fn missing_pacer_conditions_flags_configured_provider_absent_from_rows() {
        let configured = vec!["binance".to_string(), "kraken".to_string()];
        let rows = vec![PacerRow {
            provider: "binance".to_string(),
            cooldown_until: None,
            credit_limit: None,
            credits_used: 0,
        }];
        let conditions = missing_pacer_conditions(&configured, &rows);
        assert_eq!(
            conditions,
            vec![Condition::MissingPacerRow {
                provider: "kraken".to_string()
            }]
        );
    }

    #[test]
    fn missing_pacer_conditions_empty_when_all_present() {
        let configured = vec!["binance".to_string()];
        let rows = vec![PacerRow {
            provider: "binance".to_string(),
            cooldown_until: None,
            credit_limit: None,
            credits_used: 0,
        }];
        assert!(missing_pacer_conditions(&configured, &rows).is_empty());
    }

    #[test]
    fn worker_crashloop_conditions_active_at_or_beyond_threshold() {
        let reg = HealthRegistry::new();
        reg.record_worker_restart("backfill");
        reg.record_worker_restart("backfill");
        reg.record_worker_restart("backfill");
        let now = Instant::now();
        let conditions = worker_crashloop_conditions(&reg, now, Duration::from_secs(300), 3);
        assert_eq!(
            conditions,
            vec![Condition::WorkerCrashLooping {
                worker: "backfill".to_string()
            }]
        );
    }

    #[test]
    fn worker_crashloop_conditions_empty_below_threshold() {
        let reg = HealthRegistry::new();
        reg.record_worker_restart("backfill");
        reg.record_worker_restart("backfill");
        let now = Instant::now();
        let conditions = worker_crashloop_conditions(&reg, now, Duration::from_secs(300), 3);
        assert!(conditions.is_empty());
    }

    #[test]
    fn queue_failures_active_threshold_boundary() {
        assert!(!queue_failures_active(9, 10));
        assert!(queue_failures_active(10, 10));
        assert!(queue_failures_active(11, 10));
    }

    #[test]
    fn backfill_failed_active_threshold_boundary() {
        assert!(!backfill_failed_active(9, 10));
        assert!(backfill_failed_active(10, 10));
    }

    #[test]
    fn coins_stalled_active_threshold_boundary() {
        assert!(!coins_stalled_active(4, 5));
        assert!(coins_stalled_active(5, 5));
    }

    #[test]
    fn pool_saturated_true_only_when_idle_zero_and_size_at_max() {
        assert!(pool_saturated(10, 0, 10));
        assert!(!pool_saturated(10, 1, 10), "idle connections available");
        assert!(
            !pool_saturated(9, 0, 10),
            "size below max: not yet saturated"
        );
    }

    #[test]
    fn upsert_failures_active_threshold_boundary() {
        assert!(!upsert_failures_active(19, 20));
        assert!(upsert_failures_active(20, 20));
        assert!(upsert_failures_active(25, 20));
    }
}
