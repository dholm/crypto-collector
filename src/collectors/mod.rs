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
pub mod live_poller;

use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{error, info, warn};

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
            live_poller_tick: Duration::from_secs(
                config::live_quote_poll_interval_secs().max(1) as u64
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

/// Spawn all three workers with supervision, returning a handle that awaits them.
///
/// Each worker runs in its own `tokio::spawn()` for panic isolation (REQ-SCHED-050).
/// The supervisor restarts a worker if it panics or returns an error, continuing
/// until the shutdown signal is broadcast.
///
/// `shutdown_tx`: the sender side; callers broadcast `true` to stop all workers.
/// Returns a `JoinHandle` that resolves when all workers have exited.
///
// @MX:ANCHOR: [AUTO] spawn_workers — top-level worker supervisor; shutdown via watch channel
// @MX:REASON: fan_in >= 3: main.rs startup, integration tests, future health-check hooks.
//             REQ-SCHED-050: each worker in its own tokio::spawn for panic isolation.
//             REQ-SCHED-051: supervisor restarts workers on error until shutdown.
// @MX:SPEC: SPEC-SCHED-001 REQ-SCHED-050 REQ-SCHED-051
pub async fn spawn_workers(
    pool: PgPool,
    chain: Arc<Vec<Arc<dyn Provider>>>,
    cfg: WorkerConfig,
    mut shutdown_rx: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let pool_lp = pool.clone();
    let chain_lp = chain.clone();
    let cfg_lp = cfg.clone();
    let shutdown_lp = shutdown_rx.clone();

    let pool_cq = pool.clone();
    let chain_cq = chain.clone();
    let cfg_cq = cfg.clone();
    let shutdown_cq = shutdown_rx.clone();

    let pool_bf = pool.clone();
    let chain_bf = chain.clone();
    let cfg_bf = cfg.clone();
    let shutdown_bf = shutdown_rx.clone();

    tokio::spawn(async move {
        let live_poller = tokio::spawn(run_supervised_live_poller(
            pool_lp,
            chain_lp,
            cfg_lp,
            shutdown_lp,
        ));
        let queue_worker = tokio::spawn(run_supervised_queue_worker(
            pool_cq,
            chain_cq,
            cfg_cq,
            shutdown_cq,
        ));
        let backfill_worker = tokio::spawn(run_supervised_backfill_worker(
            pool_bf,
            chain_bf,
            cfg_bf,
            shutdown_bf,
        ));

        // Wait for shutdown signal, then wait for all workers.
        shutdown_rx.changed().await.ok();

        info!("supervisor: shutdown signal received; waiting for workers");
        let _ = tokio::join!(live_poller, queue_worker, backfill_worker);
        info!("supervisor: all workers stopped");
    })
}

// ── Supervised runner functions ───────────────────────────────────────────────
//
// Each runner loops: spawns the inner worker future, watches for panics and errors,
// restarts on failure, and exits cleanly when the shutdown channel fires.

async fn run_supervised_live_poller(
    pool: PgPool,
    chain: Arc<Vec<Arc<dyn Provider>>>,
    cfg: WorkerConfig,
    shutdown: watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            break;
        }

        let pool_inner = pool.clone();
        let chain_inner = chain.clone();
        let shutdown_inner = shutdown.clone();
        let cfg_inner = cfg.clone();

        let result = tokio::spawn(async move {
            live_poller::run_live_poller(
                pool_inner,
                chain_inner,
                cfg_inner.live_quote_poll_interval_secs,
                cfg_inner.live_poll_claim_ttl_secs,
                cfg_inner.live_poller_tick,
                shutdown_inner,
            )
            .await
        })
        .await;

        match result {
            Ok(Ok(())) => break, // clean shutdown
            Ok(Err(e)) => {
                if *shutdown.borrow() {
                    break;
                }
                error!("live_poller crashed with error: {e}; restarting in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Err(join_err) => {
                if *shutdown.borrow() {
                    break;
                }
                warn!("live_poller panicked: {join_err}; restarting in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn run_supervised_queue_worker(
    pool: PgPool,
    chain: Arc<Vec<Arc<dyn Provider>>>,
    cfg: WorkerConfig,
    shutdown: watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            break;
        }

        let pool_inner = pool.clone();
        let chain_inner = chain.clone();
        let shutdown_inner = shutdown.clone();
        let cfg_inner = cfg.clone();

        let result = tokio::spawn(async move {
            collection_queue::run_collection_queue_worker(
                pool_inner,
                chain_inner,
                cfg_inner.replica_id.clone(),
                cfg_inner.collection_lease_secs,
                cfg_inner.collection_heartbeat_interval_secs,
                cfg_inner.collection_max_attempts,
                cfg_inner.collection_idle_sleep,
                shutdown_inner,
            )
            .await
        })
        .await;

        match result {
            Ok(Ok(())) => break,
            Ok(Err(e)) => {
                if *shutdown.borrow() {
                    break;
                }
                error!("collection_queue_worker crashed with error: {e}; restarting in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Err(join_err) => {
                if *shutdown.borrow() {
                    break;
                }
                warn!("collection_queue_worker panicked: {join_err}; restarting in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn run_supervised_backfill_worker(
    pool: PgPool,
    chain: Arc<Vec<Arc<dyn Provider>>>,
    cfg: WorkerConfig,
    shutdown: watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            break;
        }

        let pool_inner = pool.clone();
        let chain_inner = chain.clone();
        let shutdown_inner = shutdown.clone();
        let cfg_inner = cfg.clone();

        let result = tokio::spawn(async move {
            backfill::run_backfill_worker(
                pool_inner,
                chain_inner,
                cfg_inner.replica_id.clone(),
                cfg_inner.backfill_lease_secs,
                cfg_inner.backfill_heartbeat_interval_secs,
                cfg_inner.backfill_max_attempts,
                cfg_inner.backfill_idle_sleep,
                shutdown_inner,
            )
            .await
        })
        .await;

        match result {
            Ok(Ok(())) => break,
            Ok(Err(e)) => {
                if *shutdown.borrow() {
                    break;
                }
                error!("backfill_worker crashed with error: {e}; restarting in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Err(join_err) => {
                if *shutdown.borrow() {
                    break;
                }
                warn!("backfill_worker panicked: {join_err}; restarting in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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

        let handle = spawn_workers(pool, chain, cfg, rx).await;

        // Should resolve quickly since we sent shutdown=true before spawning.
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("timeout: workers did not stop within 5s")
            .expect("join error");
    }
}
