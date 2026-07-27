//! Background collection workers (SPEC-SCHED-001).
//!
//! Three supervised worker loops:
//! - `live_poller`: continuously polls due active markets for live spot quotes.
//! - `collection_queue`: dispatches candles, metadata, market, and derivative tasks.
//! - `backfill`: fetches historical OHLC ranges from the `backfill_chunks` table.
//!
//! # Supervision (REQ-SCHED-050/051)
//!
//! Each worker runs in its own `tokio::spawn()` for panic isolation. The supervisor
//! restarts workers on error or panic until the shutdown signal is received.
//!
//! # Graceful shutdown (REQ-SCHED-050)
//!
//! A `tokio::sync::watch` channel broadcasts a shutdown signal. Workers check the
//! channel on each idle tick; the supervisor waits for all workers to exit cleanly.

pub mod backfill;
pub mod collection_queue;
pub mod cycle_overlay;
pub mod cycle_projection;
pub mod live_poller;
pub mod retry;
pub mod rollup;

use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{error, info};

use crate::alarm::reconciler::{run_reconciler, Reconciler};
use crate::alarm::{AlarmClient, HealthRegistry};
use crate::providers::Provider;

/// Configuration passed to all workers at startup (REQ-SCHED-001, OR-SCHED-1).
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    /// Stable per-replica identifier (lease fencing, REQ-SCHED-015/022).
    pub replica_id: String,

    // ── Live poller ──────────────────────────────────────────────────────────
    /// Global poll cadence in seconds (overridden per-market by `live_poll_interval`).
    pub live_quote_poll_interval_secs: i64,
    /// In-flight claim TTL: self-expiry protects against crashed replicas.
    pub live_poll_claim_ttl_secs: i64,
    /// Max coins claimed per live-poll batch, sized to complete within the claim TTL
    /// (SPEC-SCHED-002 REQ-SCHED-064).
    pub live_poll_claim_batch_limit: i64,
    /// How often to tick the live-poller loop.
    pub live_poller_tick: Duration,

    // ── Collection queue ─────────────────────────────────────────────────────
    /// Worker lease duration in seconds.
    pub collection_lease_secs: i64,
    /// Heartbeat renewal interval in seconds.
    pub collection_heartbeat_interval_secs: u64,
    /// Maximum attempts before permanently failing a row.
    pub collection_max_attempts: i32,
    /// Sleep duration when the queue is empty.
    pub collection_idle_sleep: Duration,

    // ── Backfill ─────────────────────────────────────────────────────────────
    /// Chunk lease duration in seconds (longer for historical fetches).
    pub backfill_lease_secs: i64,
    /// Heartbeat renewal interval in seconds.
    pub backfill_heartbeat_interval_secs: u64,
    /// Maximum attempts before permanently failing a chunk.
    pub backfill_max_attempts: i32,
    /// Sleep duration when the chunk queue is empty.
    pub backfill_idle_sleep: Duration,
}

impl WorkerConfig {
    /// Build from `crate::config` defaults.
    pub fn from_env() -> Self {
        use crate::config;
        Self {
            replica_id: config::replica_id().to_string(),
            live_quote_poll_interval_secs: config::live_quote_poll_interval_secs(),
            live_poll_claim_ttl_secs: config::live_poll_claim_ttl_secs(),
            live_poll_claim_batch_limit: config::live_poll_claim_batch_limit(),
            // Tick at 1/6th of the poll interval (min 5s) so a coin whose due
            // time slips past a tick boundary is picked up within one extra tick,
            // not an entire poll-interval later.
            live_poller_tick: Duration::from_secs(
                (config::live_quote_poll_interval_secs() / 6).max(5) as u64,
            ),
            collection_lease_secs: config::collection_lease_secs(),
            collection_heartbeat_interval_secs: config::collection_heartbeat_interval_secs(),
            collection_max_attempts: config::collection_max_attempts(),
            collection_idle_sleep: Duration::from_millis(config::collection_idle_sleep_ms()),
            backfill_lease_secs: config::backfill_lease_secs(),
            backfill_heartbeat_interval_secs: config::backfill_heartbeat_interval_secs(),
            backfill_max_attempts: config::backfill_max_attempts(),
            backfill_idle_sleep: Duration::from_millis(config::backfill_idle_sleep_ms()),
        }
    }
}

/// Optional Alarm Center integration components (SPEC-ALARM-001). `None` (the default
/// when `ALARM_CENTER_URL` is unset) is a full no-op — the reconciler is never
/// spawned and `None` is threaded through to every worker's chain-fetch call sites,
/// so their registry pokes are simply skipped (REQ-ALARM-001/002).
#[derive(Clone)]
pub struct AlarmComponents {
    pub client: Arc<AlarmClient>,
    pub registry: Arc<HealthRegistry>,
    pub reconcile_interval: Duration,
}

/// Spawn all workers with supervision, returning a handle that awaits them.
///
/// Each worker runs in its own `tokio::spawn()` for panic isolation (REQ-SCHED-050).
/// The supervisor restarts a worker if it panics or returns an error, continuing
/// until the shutdown signal is broadcast. When `alarm` is `Some`, a fourth worker —
/// the SPEC-ALARM-001 reconciler — is spawned and supervised identically
/// (REQ-ALARM-001/010); when `None`, no reconciler task is created at all.
///
/// `shutdown_tx`: the sender side; callers broadcast `true` to stop all workers.
/// Returns a `JoinHandle` that resolves when all workers have exited.
///
// @MX:ANCHOR: [AUTO] spawn_workers — top-level worker supervisor; shutdown via watch channel
// @MX:REASON: fan_in >= 3: main.rs startup, integration tests, future health-check hooks.
//             REQ-SCHED-050: each worker in its own tokio::spawn for panic isolation.
//             REQ-SCHED-051: supervisor restarts workers on error until shutdown.
//             REQ-ALARM-001/010: the reconciler is a fourth supervised worker, gated on
//             `alarm.is_some()` (i.e. `ALARM_CENTER_URL` configured).
// @MX:SPEC: SPEC-SCHED-001 REQ-SCHED-050 REQ-SCHED-051 SPEC-ALARM-001 REQ-ALARM-001 REQ-ALARM-010
pub async fn spawn_workers(
    pool: PgPool,
    chain: Arc<Vec<Arc<dyn Provider>>>,
    cfg: WorkerConfig,
    mut shutdown_rx: watch::Receiver<bool>,
    alarm: Option<AlarmComponents>,
) -> tokio::task::JoinHandle<()> {
    let registry = alarm.as_ref().map(|a| a.registry.clone());

    tokio::spawn(async move {
        // Three supervised workers + (optionally) the reconciler, all routed through the
        // single generic `run_supervised` (REQ-OBS-065). Each `make_future` closure re-clones
        // its captures on every restart so a fresh worker future is produced each cycle.
        let live_poller = {
            let pool = pool.clone();
            let chain = chain.clone();
            let cfg = cfg.clone();
            let inner_shutdown = shutdown_rx.clone();
            let inner_registry = registry.clone();
            tokio::spawn(run_supervised(
                "live_poller",
                registry.clone(),
                shutdown_rx.clone(),
                move || {
                    live_poller::run_live_poller(
                        pool.clone(),
                        chain.clone(),
                        cfg.live_quote_poll_interval_secs,
                        cfg.live_poll_claim_ttl_secs,
                        cfg.live_poll_claim_batch_limit,
                        cfg.live_poller_tick,
                        inner_shutdown.clone(),
                        inner_registry.clone(),
                    )
                },
            ))
        };

        let queue_worker = {
            let pool = pool.clone();
            let chain = chain.clone();
            let cfg = cfg.clone();
            let inner_shutdown = shutdown_rx.clone();
            let inner_registry = registry.clone();
            tokio::spawn(run_supervised(
                "collection_queue",
                registry.clone(),
                shutdown_rx.clone(),
                move || {
                    collection_queue::run_collection_queue_worker(
                        pool.clone(),
                        chain.clone(),
                        cfg.replica_id.clone(),
                        cfg.collection_lease_secs,
                        cfg.collection_heartbeat_interval_secs,
                        cfg.collection_max_attempts,
                        cfg.collection_idle_sleep,
                        inner_shutdown.clone(),
                        inner_registry.clone(),
                    )
                },
            ))
        };

        let backfill_worker = {
            let pool = pool.clone();
            let chain = chain.clone();
            let cfg = cfg.clone();
            let inner_shutdown = shutdown_rx.clone();
            let inner_registry = registry.clone();
            tokio::spawn(run_supervised(
                "backfill",
                registry.clone(),
                shutdown_rx.clone(),
                move || {
                    backfill::run_backfill_worker(
                        pool.clone(),
                        chain.clone(),
                        cfg.replica_id.clone(),
                        cfg.backfill_lease_secs,
                        cfg.backfill_heartbeat_interval_secs,
                        cfg.backfill_max_attempts,
                        cfg.backfill_idle_sleep,
                        inner_shutdown.clone(),
                        inner_registry.clone(),
                    )
                },
            ))
        };

        // Fourth supervised task: the reconciler, ONLY when the alarm feature is configured
        // (REQ-ALARM-001/002/010). Its inner future returns `()`, so we wrap it as `Ok(())`;
        // supervisor restarts are not recorded for it (registry = None) — it is itself the
        // crash-loop detector and does not track its own restarts.
        let reconciler_task = alarm.map(|components| {
            let reconciler = Arc::new(Reconciler::new(
                components.client,
                components.registry,
                pool.clone(),
                components.reconcile_interval,
            ));
            let inner_shutdown = shutdown_rx.clone();
            tokio::spawn(run_supervised(
                "reconciler",
                None,
                shutdown_rx.clone(),
                move || {
                    let reconciler = reconciler.clone();
                    let inner_shutdown = inner_shutdown.clone();
                    async move {
                        run_reconciler(reconciler, inner_shutdown).await;
                        Ok(())
                    }
                },
            ))
        });

        // Wait for shutdown signal, then wait for all workers.
        shutdown_rx.changed().await.ok();

        info!("supervisor: shutdown signal received; waiting for workers");
        let _ = tokio::join!(live_poller, queue_worker, backfill_worker);
        if let Some(task) = reconciler_task {
            let _ = task.await;
        }
        info!("supervisor: all workers stopped");
    })
}

// ── Generic supervisor (REQ-OBS-063/064/065) ──────────────────────────────────

/// Capped-exponential-backoff bounds for supervised restarts (REQ-OBS-063), mirroring the
/// `migrate_with_retry` precedent (`src/db/pool.rs`): start small, cap the delay.
const SUPERVISE_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const SUPERVISE_MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A task that stays up at least this long is considered healthy — its restart backoff resets
/// to the initial delay (REQ-OBS-063 "resets after a healthy-run period"), so a task that runs
/// fine for a while then fails once restarts promptly, while a deterministic crasher backs off.
const SUPERVISE_HEALTHY_RESET: Duration = Duration::from_secs(60);

/// Pure: the next restart backoff (capped exponential doubling).
fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(SUPERVISE_MAX_BACKOFF)
}

/// Pure REQ-OBS-063 backoff decision: given the current backoff and the just-finished run's
/// uptime, return `(delay_to_sleep_now, next_backoff)`. A healthy run (uptime >= the reset
/// window) resets the delay to the initial value; a fast crash uses the current backoff and
/// grows it (capped) for the following restart.
fn supervise_next_delay(current: Duration, uptime: Duration) -> (Duration, Duration) {
    let delay = if uptime >= SUPERVISE_HEALTHY_RESET {
        SUPERVISE_INITIAL_BACKOFF
    } else {
        current
    };
    (delay, next_backoff(delay))
}

// @MX:ANCHOR: [AUTO] run_supervised — the single generic supervisor for every background task
//             (the 3 workers + the reconciler + both PG LISTEN relays). One restart+backoff
//             policy, one shutdown contract, replacing the 4 copy-pasted supervisors (F-46).
// @MX:REASON: fan_in >= 3 — live_poller, collection_queue, backfill, reconciler, and both
//             relays all route through it; diverging the policy per task re-introduces the
//             copy-paste drift and the fixed-delay restart storm.
// @MX:WARN: [AUTO] the SUPERVISE_HEALTHY_RESET window gates restart-storm protection: too short
//           makes a deterministic crasher reset every cycle and storm at the initial delay.
// @MX:REASON: the backoff MUST actually grow across consecutive fast failures — a wrong reset
//             window re-introduces the deterministic-crasher restart storm F-46 set out to fix.
// @MX:SPEC: SPEC-OBS-002 REQ-OBS-063 REQ-OBS-064 REQ-OBS-065
///
/// Supervise a background task: run `make_future()` in its own `tokio::spawn` for panic
/// isolation, and on a returned error OR a panic restart it after a capped exponential
/// backoff that resets after a healthy run (REQ-OBS-063). Exits cleanly when the inner future
/// returns `Ok(())` (a clean shutdown) or when the shutdown watch fires. Both PG LISTEN relays
/// (REQ-OBS-064) and the three workers + reconciler (REQ-OBS-065) route through this one
/// function. Log severity is identical between the error arm and the panic arm.
pub async fn run_supervised<F, Fut>(
    name: &'static str,
    registry: Option<Arc<HealthRegistry>>,
    shutdown: watch::Receiver<bool>,
    mut make_future: F,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let mut backoff = SUPERVISE_INITIAL_BACKOFF;
    loop {
        if *shutdown.borrow() {
            break;
        }

        let started = tokio::time::Instant::now();
        let result = tokio::spawn(make_future()).await;

        match result {
            Ok(Ok(())) => break, // clean shutdown
            Ok(Err(e)) => {
                if *shutdown.borrow() {
                    break;
                }
                error!(worker = name, error = %e, "supervised task returned an error; restarting after backoff");
            }
            Err(join_err) => {
                if *shutdown.borrow() {
                    break;
                }
                // Same severity as the error arm (REQ-OBS-065): a panic is not less severe.
                error!(worker = name, panic = %join_err, "supervised task panicked; restarting after backoff");
            }
        }

        if let Some(reg) = &registry {
            reg.record_worker_restart(name);
        }

        let (delay, next) = supervise_next_delay(backoff, started.elapsed());
        backoff = next;
        info!(
            worker = name,
            backoff_secs = delay.as_secs(),
            "supervised task restarting after backoff"
        );
        tokio::time::sleep(delay).await;
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── SPEC-OBS-002 generic supervisor (F-46 / REQ-OBS-063/065) ────────────────

    #[test]
    fn supervise_backoff_grows_caps_and_resets() {
        // AC-OBS-063: a deterministic fast crasher (uptime below the reset window) grows the
        // restart delay exponentially and caps at the ceiling.
        let fast = Duration::from_millis(10);
        let mut backoff = SUPERVISE_INITIAL_BACKOFF;
        let mut delays = Vec::new();
        for _ in 0..8 {
            let (delay, next) = supervise_next_delay(backoff, fast);
            delays.push(delay);
            backoff = next;
        }
        assert_eq!(delays[0], Duration::from_secs(1));
        assert_eq!(delays[1], Duration::from_secs(2));
        assert_eq!(delays[2], Duration::from_secs(4));
        assert_eq!(delays[3], Duration::from_secs(8));
        assert_eq!(delays[4], Duration::from_secs(16));
        assert_eq!(delays[5], Duration::from_secs(30)); // 32 capped to 30
        assert_eq!(delays[6], Duration::from_secs(30)); // stays capped
        assert!(
            delays.iter().all(|d| *d <= SUPERVISE_MAX_BACKOFF),
            "restart delay must never exceed the configured cap"
        );

        // A healthy run (uptime >= the reset window) resets the delay to the initial value.
        let (delay, next) = supervise_next_delay(SUPERVISE_MAX_BACKOFF, SUPERVISE_HEALTHY_RESET);
        assert_eq!(
            delay, SUPERVISE_INITIAL_BACKOFF,
            "a healthy run resets the backoff to the initial delay (REQ-OBS-063)"
        );
        assert_eq!(next, Duration::from_secs(2));
    }

    #[test]
    fn only_one_generic_supervisor_no_underscore_variants() {
        // AC-OBS-065: the four former per-worker supervised-runner functions (live poller,
        // queue worker, backfill worker, reconciler) are gone — everything routes through the
        // single generic run_supervised. The needle is concatenated so neither this assertion
        // nor its message is itself a match for the external `grep` the AC runs.
        let src = std::fs::read_to_string("src/collectors/mod.rs").expect("read collectors/mod.rs");
        let code = src.split("#[cfg(test)]").next().unwrap_or(&src);
        let per_worker_variant = format!("run_supervised{}", "_");
        assert!(
            !code.contains(&per_worker_variant),
            "no per-worker supervised-runner variant may remain — use the single generic supervisor (REQ-OBS-065)"
        );
    }

    #[test]
    fn worker_config_from_env_has_sensible_defaults() {
        // Guard: only run when no env overrides are set.
        if std::env::var("LIVE_QUOTE_POLL_INTERVAL_SECS").is_err() {
            let cfg = WorkerConfig::from_env();
            assert_eq!(cfg.live_quote_poll_interval_secs, 60);
            assert_eq!(cfg.live_poll_claim_ttl_secs, 120);
            assert_eq!(cfg.collection_lease_secs, 120);
            assert_eq!(cfg.collection_max_attempts, 5);
            assert_eq!(cfg.backfill_lease_secs, 300);
            assert_eq!(cfg.backfill_max_attempts, 5);
            assert!(!cfg.replica_id.is_empty());
        }
    }

    #[test]
    fn worker_config_replica_id_is_stable() {
        let cfg1 = WorkerConfig::from_env();
        let cfg2 = WorkerConfig::from_env();
        assert_eq!(
            cfg1.replica_id, cfg2.replica_id,
            "replica_id must be stable within process"
        );
    }

    #[tokio::test]
    async fn spawn_workers_shuts_down_cleanly() {
        // This test creates a minimal worker setup with no real pool/chain
        // and verifies the supervisor handles shutdown signal correctly.
        // We use a fake pool from an invalid URL (connect() not called) and
        // just test the supervision state-machine.
        //
        // Skipped if DATABASE_URL is absent (no real DB needed for this test
        // since we never actually call the worker inner futures here).
        //
        // The purpose is to verify the watch channel and JoinHandle wiring.

        let (tx, rx) = watch::channel(false);

        // Immediately broadcast shutdown.
        tx.send(true).expect("send shutdown");

        // Create a dummy pool from a known bad URL so connect() never blocks.
        // Since workers check shutdown before doing any DB work, this is fine.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://localhost/does_not_exist")
            .expect("lazy pool");

        let chain: Arc<Vec<Arc<dyn Provider>>> = Arc::new(vec![]);
        let cfg = WorkerConfig::from_env();

        let handle = spawn_workers(pool, chain, cfg, rx, None).await;

        // Should resolve quickly since we sent shutdown=true before spawning.
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("timeout: workers did not stop within 5s")
            .expect("join error");
    }
}
