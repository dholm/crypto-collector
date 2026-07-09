//! Reconciler worker: the near-stateless desired-state sweep loop (SPEC-ALARM-001
//! Milestones 4–5).
//!
//! Each sweep: compute the CURRENT set of active alarm conditions from observable
//! state, `raise()` every active condition with `timeoutSeconds = ALARM_TTL_SECS`, and
//! simply stop refreshing every condition that is no longer active so the Alarm Center
//! auto-clears it once the TTL lapses (REQ-ALARM-011..018). For Critical/Error
//! conditions observed transitioning active→inactive, an optional immediate fast-clear
//! is fired as a latency optimisation (REQ-ALARM-014); Warning conditions rely on TTL
//! expiry alone. This batch (Batch 2) wires Tier 1 desired-state derivation only
//! (registry-derived provider-unreachable/all-providers-down + pacer-derived
//! rate-limited/credit-exhausted); Tier 2/3 are Batch 3 scope.
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
}

impl Thresholds {
    fn from_config() -> Self {
        Self {
            provider_unreachable: Duration::from_secs(
                crate::config::alarm_provider_unreachable_secs(),
            ),
        }
    }
}

/// The near-stateless reconciler (REQ-ALARM-012): the Alarm Center (via TTL) is the
/// source of truth for what is currently active, so this struct needs no
/// correctness-critical record of what it raised. `previously_active` is kept ONLY to
/// detect active→inactive transitions for the optional Critical/Error fast-clear
/// (REQ-ALARM-014); losing it (e.g. on restart) does not affect correctness.
pub struct Reconciler {
    client: Arc<AlarmClient>,
    registry: Arc<HealthRegistry>,
    pool: PgPool,
    interval: Duration,
    previously_active: Mutex<HashMap<String, Severity>>,
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
        }
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    async fn pacer_desired_conditions(&self) -> Vec<Condition> {
        let rows = sqlx::query_as::<_, PacerRow>(
            "SELECT provider, cooldown_until, credit_limit, credits_used \
             FROM upstream_request_pacer",
        )
        .fetch_all(&self.pool)
        .await;

        match rows {
            Ok(rows) => pacer_rows_to_conditions(&rows, Utc::now()),
            Err(e) => {
                error!(
                    error = %e,
                    "reconciler: failed to read upstream_request_pacer; skipping \
                     pacer-derived Tier 1 conditions this sweep"
                );
                Vec::new()
            }
        }
    }

    /// Run exactly one sweep (REQ-ALARM-011): compute Tier 1 desired state, raise every
    /// active condition with the TTL, and fire the optional Critical/Error fast-clear
    /// for any fingerprint that dropped out of the active set since the last sweep.
    pub async fn sweep_once(&self) {
        let thresholds = Thresholds::from_config();
        let now = Instant::now();

        let mut conditions =
            registry_desired_conditions(&self.registry, now, thresholds.provider_unreachable);
        conditions.extend(self.pacer_desired_conditions().await);

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
            let spec = catalog::to_alarm_spec(condition);
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
        let conditions = reconciler.pacer_desired_conditions().await;
        assert!(conditions.contains(&Condition::ProviderRateLimited {
            provider: "binance".to_string()
        }));

        sqlx::query(
            "UPDATE upstream_request_pacer SET cooldown_until = NULL WHERE provider = 'binance'",
        )
        .execute(&pool)
        .await
        .expect("clear cooldown");

        let conditions = reconciler.pacer_desired_conditions().await;
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
        let conditions = reconciler.pacer_desired_conditions().await;
        assert!(conditions.contains(&Condition::ProviderCreditExhausted {
            provider: "coingecko".to_string()
        }));

        sqlx::query(
            "UPDATE upstream_request_pacer SET credits_used = 0 WHERE provider = 'coingecko'",
        )
        .execute(&pool)
        .await
        .expect("reset credits");

        let conditions = reconciler.pacer_desired_conditions().await;
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
}
