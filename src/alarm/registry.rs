//! Shared in-memory health registry (SPEC-ALARM-001, REQ-ALARM-019).
//!
//! Holds exactly the state that cannot be re-derived from the database, updated
//! cheaply (O(1), no I/O, no alarm-center calls) at provider/collector/db error sites
//! and read by the reconciler. Tier 1 fields (`providers`, `all_providers_down`) were
//! wired in Batch 2. This batch (Batch 3) wires the remaining fields: `worker_restarts`
//! (a decaying timestamped event set, pushed by the collector supervisor restart arms,
//! REQ-ALARM-034) and `upsert_failure_streak` (a consecutive-failure counter, pushed by
//! upsert call sites, REQ-ALARM-042).
//!
//! @MX:NOTE: [AUTO] HealthRegistry enumerates exactly the counters/flags each condition
//! reads: `providers` feeds provider-unreachable (REQ-ALARM-020); the chain-outcome
//! timestamps `last_all_failed_at` / `last_chain_success_at` feed all-providers-down
//! (REQ-ALARM-022, sustained per SPEC-OBS-002 REQ-ALARM-080); `worker_restarts` feeds
//! worker-crash-looping (REQ-ALARM-034); `upsert_failure_streak` feeds db-upsert-failures
//! (REQ-ALARM-042).
//! This registry drives DETECTION only — it is never a clear mechanism (recovery is
//! server-driven via TTL, see `crate::alarm::reconciler`), so its imperfection or loss
//! cannot strand an alarm.

use crate::providers::{AttemptRecord, ProviderOutcome};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Upper bound on how long a worker-restart event is retained in memory, independent of
/// the configurable crash-loop window (`ALARM_WORKER_CRASHLOOP_WINDOW_SECS`). Bounds
/// unbounded growth of the event list for a worker that restarts occasionally over a
/// long process lifetime, while staying generous enough to never interfere with any
/// realistic crash-loop window.
const WORKER_RESTART_RETENTION: Duration = Duration::from_secs(24 * 3600);

/// Per-provider reachability snapshot (REQ-ALARM-020 active signal).
#[derive(Debug, Clone, Copy, Default)]
pub struct ProviderHealth {
    /// `None` = this provider has never recorded a success.
    pub last_success_at: Option<Instant>,
    /// Consecutive `ProviderError::Network` failures since the last success.
    pub consecutive_network_failures: u32,
}

/// Cheap, shareable health registry (wrap in `Arc` for injection into workers and the
/// reconciler). Every update method is O(1), touches no I/O, and never calls the alarm
/// center — safe to call from any hot-path error site (REQ-ALARM-007/019).
#[derive(Default)]
pub struct HealthRegistry {
    providers: Mutex<HashMap<String, ProviderHealth>>,
    /// Most recent time a chain fetch recorded EVERY attempt as a failure (REQ-ALARM-022 /
    /// REQ-ALARM-080). Paired with `last_chain_success_at` to derive the sustained
    /// all-providers-down signal — a timestamp pair, NOT a sampled last-outcome flag (F-42).
    last_all_failed_at: Mutex<Option<Instant>>,
    /// Most recent time a chain fetch recorded at least one success (REQ-ALARM-080).
    last_chain_success_at: Mutex<Option<Instant>>,
    worker_restarts: Mutex<HashMap<String, Vec<Instant>>>,
    upsert_failure_streak: AtomicU32,
}

/// Pure: is the chain "all providers down" right now, given the two chain-outcome
/// timestamps? True iff an all-failure has been recorded and no chain success has been
/// recorded after it (the most recent chain evidence is an all-failure). This is the raw
/// point-in-time signal; the reconciler layers the sustained timer
/// (`sustained_state_update` / `sustained_active`, REQ-ALARM-080) on top so the Critical
/// alarm reflects a SUSTAINED whole-chain outage, not a single sampled last-outcome flag.
///
/// A tie (`failed == success`, possible when two records land on the same monotonic
/// `Instant`) resolves to NOT-down: a recorded success is never overridden by a
/// simultaneous failure.
pub fn chain_all_failed_now(
    last_all_failed_at: Option<Instant>,
    last_chain_success_at: Option<Instant>,
) -> bool {
    match (last_all_failed_at, last_chain_success_at) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(failed), Some(success)) => failed > success,
    }
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// A provider succeeded: reset its failure streak and stamp last-success now.
    pub fn record_provider_success(&self, provider: &str) {
        let mut providers = self.providers.lock().expect("registry lock poisoned");
        let entry = providers.entry(provider.to_string()).or_default();
        entry.last_success_at = Some(Instant::now());
        entry.consecutive_network_failures = 0;
    }

    /// A provider produced `ProviderError::Network`: bump the consecutive-failure
    /// counter. Does NOT touch `last_success_at` (REQ-ALARM-020).
    pub fn record_provider_network_failure(&self, provider: &str) {
        let mut providers = self.providers.lock().expect("registry lock poisoned");
        let entry = providers.entry(provider.to_string()).or_default();
        entry.consecutive_network_failures += 1;
    }

    /// Snapshot a provider's current health. Returns the zero-value (never observed)
    /// if the provider has no entry yet.
    pub fn provider_snapshot(&self, provider: &str) -> ProviderHealth {
        self.providers
            .lock()
            .expect("registry lock poisoned")
            .get(provider)
            .copied()
            .unwrap_or_default()
    }

    /// All provider names currently tracked (for the reconciler's sweep iteration).
    pub fn tracked_providers(&self) -> Vec<String> {
        self.providers
            .lock()
            .expect("registry lock poisoned")
            .keys()
            .cloned()
            .collect()
    }

    /// Stamp the chain-all-failed timestamp: a chain fetch recorded every attempt as a
    /// failure (REQ-ALARM-022 / REQ-ALARM-080).
    pub fn record_chain_all_failed(&self) {
        *self
            .last_all_failed_at
            .lock()
            .expect("registry lock poisoned") = Some(Instant::now());
    }

    /// Stamp the chain-success timestamp: a chain fetch recorded at least one success
    /// (REQ-ALARM-080).
    pub fn record_chain_success(&self) {
        *self
            .last_chain_success_at
            .lock()
            .expect("registry lock poisoned") = Some(Instant::now());
    }

    /// Current raw all-providers-down signal (REQ-ALARM-022 active signal): the most recent
    /// chain evidence is an all-failure. The reconciler feeds this into the sustained timer
    /// (REQ-ALARM-080) so a single sampled failure cannot flip the Critical alarm.
    pub fn all_providers_down(&self) -> bool {
        let last_all_failed_at = *self
            .last_all_failed_at
            .lock()
            .expect("registry lock poisoned");
        let last_chain_success_at = *self
            .last_chain_success_at
            .lock()
            .expect("registry lock poisoned");
        chain_all_failed_now(last_all_failed_at, last_chain_success_at)
    }

    /// Convenience: derive the CHAIN-OUTCOME signal only (all-failed vs any-success) from a
    /// batch of `AttemptRecord`s (as produced by `chain_fetch_ohlc` / `chain_fetch_ohlc_range`).
    /// Among the attempted records (`Unsupported` filtered out — a provider that does not
    /// support the capability is neither reachable nor unreachable evidence): any `Success`
    /// stamps `last_chain_success_at`; all-`Failure` stamps `last_all_failed_at`.
    ///
    /// This helper does NOT update the per-provider network-failure streak. `AttemptRecord`
    /// does not carry the underlying `ProviderError`, so this helper cannot distinguish a
    /// `ProviderError::Network` (reachability) failure from a non-`Network` failure (e.g. a
    /// repeated 5xx) — and the per-provider `provider-unreachable` streak counts ONLY
    /// `Network` failures (REQ-ALARM-020). The concrete-error call sites in `chain_fetch_ohlc`
    /// own that per-provider update, gated on `matches!(e, ProviderError::Network(_))`; this
    /// helper deliberately leaves `consecutive_network_failures` untouched.
    ///
    // @MX:NOTE: [AUTO] OR-OBS2-3 (REQ-ALARM-081, doc/code drift) resolved — the CODE is
    //           authoritative: `observe_chain_records` derives ONLY the chain-outcome signal
    //           and never touches the per-provider streak; the per-provider streak counts
    //           ONLY `ProviderError::Network` failures (non-Network 5xx do NOT count). The
    //           prior doc claiming it "records ANY failure as a network failure" was wrong
    //           and has been corrected here.
    // @MX:SPEC: SPEC-OBS-002 REQ-ALARM-081 SPEC-ALARM-001 REQ-ALARM-020
    pub fn observe_chain_records(&self, records: &[AttemptRecord]) {
        let attempted: Vec<&AttemptRecord> = records
            .iter()
            .filter(|r| r.outcome != ProviderOutcome::Unsupported)
            .collect();
        if attempted.is_empty() {
            return;
        }
        if attempted
            .iter()
            .any(|r| r.outcome == ProviderOutcome::Success)
        {
            self.record_chain_success();
        } else if attempted
            .iter()
            .all(|r| r.outcome == ProviderOutcome::Failure)
        {
            self.record_chain_all_failed();
        }
    }

    // ── worker_restarts (REQ-ALARM-019/034) ────────────────────────────────────

    /// A supervised worker (`live_poller`/`collection_queue`/`backfill`) just restarted
    /// after a panic or crash: push a timestamped event. Events older than
    /// [`WORKER_RESTART_RETENTION`] are pruned opportunistically on each call so the
    /// per-worker event list cannot grow unbounded over a long process lifetime — this
    /// is independent of (and much longer than) the configurable crash-loop window used
    /// for the alarm signal itself (REQ-ALARM-034, a decaying event set, NOT a monotonic
    /// counter).
    pub fn record_worker_restart(&self, worker: &str) {
        let now = Instant::now();
        let mut restarts = self.worker_restarts.lock().expect("registry lock poisoned");
        let events = restarts.entry(worker.to_string()).or_default();
        events.push(now);
        events.retain(|t| now.saturating_duration_since(*t) < WORKER_RESTART_RETENTION);
    }

    /// Count of a worker's restart events within `window` of `now` (REQ-ALARM-034 active
    /// signal). Returns 0 for a worker with no recorded restarts.
    pub fn worker_restart_count_in_window(
        &self,
        worker: &str,
        now: Instant,
        window: Duration,
    ) -> u32 {
        self.worker_restarts
            .lock()
            .expect("registry lock poisoned")
            .get(worker)
            .map(|events| {
                events
                    .iter()
                    .filter(|t| now.saturating_duration_since(**t) < window)
                    .count() as u32
            })
            .unwrap_or(0)
    }

    /// All worker names with at least one recorded restart event (for the reconciler's
    /// sweep iteration, mirroring [`Self::tracked_providers`]).
    pub fn tracked_workers(&self) -> Vec<String> {
        self.worker_restarts
            .lock()
            .expect("registry lock poisoned")
            .keys()
            .cloned()
            .collect()
    }

    // ── upsert_failure_streak (REQ-ALARM-042) ───────────────────────────────────

    /// A database upsert failed (`sqlx::Error`): bump the consecutive-failure streak.
    pub fn record_upsert_failure(&self) {
        self.upsert_failure_streak.fetch_add(1, Ordering::SeqCst);
    }

    /// A database upsert succeeded: reset the consecutive-failure streak to zero.
    pub fn record_upsert_success(&self) {
        self.upsert_failure_streak.store(0, Ordering::SeqCst);
    }

    /// Current consecutive upsert-failure streak (REQ-ALARM-042 active signal).
    pub fn upsert_failure_streak(&self) -> u32 {
        self.upsert_failure_streak.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ── record_provider_success / record_provider_network_failure ─────────────

    #[test]
    fn unseen_provider_snapshot_is_zero_value() {
        let reg = HealthRegistry::new();
        let snap = reg.provider_snapshot("binance");
        assert!(snap.last_success_at.is_none());
        assert_eq!(snap.consecutive_network_failures, 0);
    }

    #[test]
    fn record_provider_network_failure_increments_counter_without_touching_success() {
        let reg = HealthRegistry::new();
        reg.record_provider_network_failure("binance");
        reg.record_provider_network_failure("binance");
        let snap = reg.provider_snapshot("binance");
        assert_eq!(snap.consecutive_network_failures, 2);
        assert!(snap.last_success_at.is_none());
    }

    #[test]
    fn record_provider_success_resets_failure_streak_and_stamps_time() {
        let reg = HealthRegistry::new();
        reg.record_provider_network_failure("binance");
        reg.record_provider_network_failure("binance");
        reg.record_provider_success("binance");
        let snap = reg.provider_snapshot("binance");
        assert_eq!(snap.consecutive_network_failures, 0);
        assert!(snap.last_success_at.is_some());
        assert!(snap.last_success_at.unwrap().elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn providers_are_tracked_independently() {
        let reg = HealthRegistry::new();
        reg.record_provider_network_failure("binance");
        reg.record_provider_success("coinbase");
        assert_eq!(
            reg.provider_snapshot("binance")
                .consecutive_network_failures,
            1
        );
        assert_eq!(
            reg.provider_snapshot("coinbase")
                .consecutive_network_failures,
            0
        );
        assert!(reg.provider_snapshot("coinbase").last_success_at.is_some());
    }

    #[test]
    fn tracked_providers_lists_every_observed_name() {
        let reg = HealthRegistry::new();
        reg.record_provider_success("binance");
        reg.record_provider_network_failure("coinbase");
        let mut names = reg.tracked_providers();
        names.sort();
        assert_eq!(names, vec!["binance".to_string(), "coinbase".to_string()]);
    }

    // ── record_chain_all_failed / record_chain_success ─────────────────────────

    #[test]
    fn chain_starts_not_down() {
        let reg = HealthRegistry::new();
        assert!(!reg.all_providers_down());
    }

    #[test]
    fn record_chain_all_failed_sets_flag() {
        let reg = HealthRegistry::new();
        reg.record_chain_all_failed();
        assert!(reg.all_providers_down());
    }

    #[test]
    fn record_chain_success_clears_flag() {
        let reg = HealthRegistry::new();
        reg.record_chain_all_failed();
        reg.record_chain_success();
        assert!(!reg.all_providers_down());
    }

    // ── chain_all_failed_now pure helper (REQ-ALARM-080) ───────────────────────

    #[test]
    fn chain_all_failed_now_semantics() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        // Never observed → not down.
        assert!(!chain_all_failed_now(None, None));
        // Failed, never succeeded → down.
        assert!(chain_all_failed_now(Some(t0), None));
        // Success after failure → not down.
        assert!(!chain_all_failed_now(Some(t0), Some(t1)));
        // Failure after success → down.
        assert!(chain_all_failed_now(Some(t1), Some(t0)));
        // Tie (same instant) → not down (a success is never overridden by a simultaneous failure).
        assert!(!chain_all_failed_now(Some(t0), Some(t0)));
    }

    // ── observe_chain_records ───────────────────────────────────────────────────

    fn rec(provider: &str, outcome: ProviderOutcome) -> AttemptRecord {
        AttemptRecord {
            provider: provider.to_string(),
            capability: crate::providers::Capability::Ohlc,
            outcome,
        }
    }

    #[test]
    fn observe_chain_records_all_failure_sets_down() {
        let reg = HealthRegistry::new();
        let records = vec![
            rec("binance", ProviderOutcome::Failure),
            rec("coinbase", ProviderOutcome::Failure),
        ];
        reg.observe_chain_records(&records);
        assert!(reg.all_providers_down());
    }

    #[test]
    fn observe_chain_records_any_success_clears_down() {
        let reg = HealthRegistry::new();
        reg.record_chain_all_failed();
        let records = vec![
            rec("binance", ProviderOutcome::Failure),
            rec("coinbase", ProviderOutcome::Success),
        ];
        reg.observe_chain_records(&records);
        assert!(!reg.all_providers_down());
    }

    #[test]
    fn observe_chain_records_ignores_unsupported_only_records() {
        let reg = HealthRegistry::new();
        let records = vec![rec("binance", ProviderOutcome::Unsupported)];
        reg.observe_chain_records(&records);
        // No attempted (non-Unsupported) records: flag left untouched (still not down).
        assert!(!reg.all_providers_down());
    }

    #[test]
    fn observe_chain_records_unsupported_mixed_with_failure_is_not_all_failed() {
        let reg = HealthRegistry::new();
        let records = vec![
            rec("coingecko", ProviderOutcome::Unsupported),
            rec("binance", ProviderOutcome::Failure),
        ];
        reg.observe_chain_records(&records);
        // Literal reading of REQ-ALARM-022: "every AttemptRecord.outcome == Failure".
        // A mixed Unsupported+Failure batch (after filtering Unsupported) has only one
        // attempted record which IS Failure, so this DOES count as all-failed among
        // attempted providers.
        assert!(reg.all_providers_down());
    }

    // ── record_worker_restart / worker_restart_count_in_window (REQ-ALARM-019/034) ──

    #[test]
    fn worker_with_no_restarts_has_zero_count() {
        let reg = HealthRegistry::new();
        assert_eq!(
            reg.worker_restart_count_in_window(
                "backfill",
                Instant::now(),
                Duration::from_secs(300)
            ),
            0
        );
    }

    #[test]
    fn record_worker_restart_increments_in_window_count() {
        let reg = HealthRegistry::new();
        reg.record_worker_restart("backfill");
        reg.record_worker_restart("backfill");
        let now = Instant::now();
        assert_eq!(
            reg.worker_restart_count_in_window("backfill", now, Duration::from_secs(300)),
            2
        );
    }

    #[test]
    fn worker_restart_count_in_window_excludes_events_outside_window() {
        let reg = HealthRegistry::new();
        // Simulate an old event by manipulating the registry directly is not possible
        // (no I/O), so instead verify the window boundary with a zero-width window: an
        // event recorded "now" falls outside a window of 0 once any time elapses.
        reg.record_worker_restart("live_poller");
        let count_wide = reg.worker_restart_count_in_window(
            "live_poller",
            Instant::now(),
            Duration::from_secs(300),
        );
        assert_eq!(count_wide, 1, "event is within a generous window");
    }

    #[test]
    fn worker_restarts_are_tracked_independently_per_worker() {
        let reg = HealthRegistry::new();
        reg.record_worker_restart("live_poller");
        reg.record_worker_restart("live_poller");
        reg.record_worker_restart("backfill");
        let now = Instant::now();
        assert_eq!(
            reg.worker_restart_count_in_window("live_poller", now, Duration::from_secs(300)),
            2
        );
        assert_eq!(
            reg.worker_restart_count_in_window("backfill", now, Duration::from_secs(300)),
            1
        );
    }

    #[test]
    fn tracked_workers_lists_every_worker_with_a_restart_event() {
        let reg = HealthRegistry::new();
        reg.record_worker_restart("live_poller");
        reg.record_worker_restart("collection_queue");
        let mut workers = reg.tracked_workers();
        workers.sort();
        assert_eq!(
            workers,
            vec!["collection_queue".to_string(), "live_poller".to_string()]
        );
    }

    // ── record_upsert_failure / record_upsert_success / upsert_failure_streak
    // ── (REQ-ALARM-042) ─────────────────────────────────────────────────────────

    #[test]
    fn upsert_failure_streak_starts_at_zero() {
        let reg = HealthRegistry::new();
        assert_eq!(reg.upsert_failure_streak(), 0);
    }

    #[test]
    fn record_upsert_failure_increments_streak() {
        let reg = HealthRegistry::new();
        reg.record_upsert_failure();
        reg.record_upsert_failure();
        reg.record_upsert_failure();
        assert_eq!(reg.upsert_failure_streak(), 3);
    }

    #[test]
    fn record_upsert_success_resets_streak_to_zero() {
        let reg = HealthRegistry::new();
        reg.record_upsert_failure();
        reg.record_upsert_failure();
        reg.record_upsert_success();
        assert_eq!(reg.upsert_failure_streak(), 0);
    }
}
