//! PostgreSQL LISTEN/NOTIFY relay for WebSocket broadcast channels (SPEC-API-002 REQ-API-148).
//!
//! Two long-running tasks each hold a `PgListener` subscription to a named channel.
//! When the live_poller or collection_queue upserts a row, it calls `pg_notify(...)` in
//! the same transaction so all replicas receive the event (cross-replica delivery via PG).
//! The relay forwards the raw JSON payload string to a `broadcast::Sender<String>` that
//! WebSocket handlers subscribe to.
//!
//! # Channel names
//!
//! - `coin_quote_updated` → relayed to `AppState.coin_quote_tx`
//! - `coin_candle_updated` → relayed to `AppState.coin_candle_tx`
//!
//! # Lag handling
//!
//! WebSocket receivers use `broadcast::Receiver::recv()` which returns `Lagged` if they
//! fall behind by more than the channel capacity. Lagged receivers log a warning and
//! continue from the newest message (best-effort delivery; no retry, no backpressure).

// @MX:WARN: [AUTO] Long-running tokio task; must be given a shutdown token
// @MX:REASON: These tasks hold open a dedicated DB connection each for the lifetime of the process.
//             Without the shutdown_rx guard they block graceful shutdown.
// @MX:SPEC: SPEC-API-002 SPEC-OBS-001

use anyhow::Context;
use sqlx::postgres::PgListener;
use sqlx::PgPool;
use tokio::sync::{broadcast, watch};
use tracing::{info, warn};

use crate::shutdown::shutdown_arm_should_break;

/// Relay PG NOTIFY `coin_quote_updated` → `coin_quote_tx`.
///
/// Returns `Ok(())` on a clean shutdown-signal exit; returns `Err` on an initial connect /
/// `listen()` failure so the supervisor restarts it with capped backoff (REQ-OBS-064).
pub async fn run_coin_quote_listener(
    pool: PgPool,
    tx: broadcast::Sender<String>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    run_listener(pool, "coin_quote_updated", tx, &mut shutdown_rx).await
}

/// Relay PG NOTIFY `coin_candle_updated` → `coin_candle_tx`.
///
/// Returns `Ok(())` on a clean shutdown-signal exit; returns `Err` on an initial connect /
/// `listen()` failure so the supervisor restarts it with capped backoff (REQ-OBS-064).
pub async fn run_coin_candle_listener(
    pool: PgPool,
    tx: broadcast::Sender<String>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    run_listener(pool, "coin_candle_updated", tx, &mut shutdown_rx).await
}

/// Shared relay loop.
///
/// Returns `Ok(())` on a clean shutdown-signal exit. Returns `Err` if the initial
/// `PgListener::connect_with` or `listen()` fails — the caller supervises this relay via
/// `collectors::run_supervised`, which restarts it with capped exponential backoff
/// (REQ-OBS-063/064), so a transient DB hiccup at spawn does NOT permanently disable
/// cross-replica WebSocket delivery (REQ-API-148). Mid-stream connection drops are handled
/// internally by `PgListener` (it reconnects on the next `recv()`); only an initial-setup
/// failure surfaces as an `Err` to trigger a supervised restart.
async fn run_listener(
    pool: PgPool,
    channel: &'static str,
    tx: broadcast::Sender<String>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    info!(channel, "PG listener starting");

    // An initial connect/listen failure returns Err (not a permanent early return) so the
    // supervisor restarts the relay with capped backoff (REQ-OBS-064 / F-39).
    let mut listener = PgListener::connect_with(&pool)
        .await
        .with_context(|| format!("PG listener {channel} failed to connect"))?;
    listener
        .listen(channel)
        .await
        .with_context(|| format!("PG listener {channel} failed to subscribe"))?;

    loop {
        tokio::select! {
            biased;

            // Honour graceful shutdown signal first. Break on the shutdown value AND on a
            // dropped sender (`changed()` → Err): a dropped sender means the orchestrator is
            // gone, so there is nothing left to wait for — breaking avoids busy-spinning on
            // the immediately-ready error (REQ-OBS-068 / F-47).
            res = shutdown_rx.changed() => {
                if shutdown_arm_should_break(res.is_err(), *shutdown_rx.borrow()) {
                    info!(channel, "PG listener received shutdown; exiting");
                    break;
                }
            }

            notification = listener.recv() => {
                match notification {
                    Ok(n) => {
                        let payload = n.payload().to_string();
                        // Ignore send errors — no subscribers is normal at startup.
                        if tx.send(payload).is_err() {
                            // All receivers dropped; wait for reconnect.
                        }
                    }
                    Err(e) => {
                        warn!(channel, error = %e, "PG listener recv error; reconnecting");
                        // PgListener handles reconnection internally.
                        // If the error is fatal, the next recv() will also error out.
                    }
                }
            }
        }
    }

    info!(channel, "PG listener stopped");
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// AC-OBS-068 (mechanical): the relay loop's shutdown select arm CAPTURES the result
    /// (`res = shutdown_rx.changed()`) rather than discarding it in an un-captured
    /// `_ = shutdown_rx.changed()` arm that would busy-spin on the immediately-ready error
    /// (REQ-OBS-068 / F-47). The break decision itself is verified behaviorally by
    /// `crate::shutdown::tests::shutdown_arm_should_break_truth_table` (pure fn), which
    /// replaces the former brittle `res.is_err()` source-text scan.
    #[test]
    fn listener_shutdown_arm_captures_changed_result() {
        let src = std::fs::read_to_string("src/listener.rs").expect("read listener.rs");
        let code = src.split("#[cfg(test)]").next().unwrap_or(&src);
        // Needle built by concatenation so this assertion string is not itself a match.
        let uncaptured = format!("_ {} shutdown_rx.changed()", "=");
        assert!(
            !code.contains(&uncaptured),
            "no un-captured shutdown_rx.changed() arm may remain (REQ-OBS-068)"
        );
    }

    /// AC-OBS-064 (F-39): a relay whose initial connect fails returns `Err` (NOT a permanent
    /// silent early-return), so the supervisor restarts it with capped backoff and a transient
    /// DB hiccup at spawn does not permanently disable cross-replica delivery (REQ-API-148).
    /// Uses an unreachable lazy pool — no live DB needed.
    #[tokio::test]
    async fn relay_returns_err_on_initial_connect_failure() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(500))
            .connect_lazy("postgres://127.0.0.1:1/does_not_exist")
            .expect("lazy pool");
        let (tx, _rx) = broadcast::channel::<String>(4);
        let (_sd_tx, sd_rx) = watch::channel(false);
        let result = run_coin_quote_listener(pool, tx, sd_rx).await;
        assert!(
            result.is_err(),
            "an initial connect failure must return Err so the supervisor retries (REQ-OBS-064)"
        );
    }

    /// AC-OBS-064: the run_listener doc describes the supervised capped-backoff retry now
    /// implemented (F-39 doc/code drift closed), not the old permanent-return behavior.
    #[test]
    fn run_listener_doc_describes_supervised_retry() {
        let src = std::fs::read_to_string("src/listener.rs").expect("read listener.rs");
        let code = src.split("#[cfg(test)]").next().unwrap_or(&src);
        assert!(
            code.contains("run_supervised") && code.contains("capped"),
            "run_listener doc must describe the supervised capped-backoff retry (REQ-OBS-064 / F-39)"
        );
    }

    // Unit: verify the public function signatures compile — no DB needed.
    #[test]
    fn listener_fns_exist() {
        let _: fn(PgPool, broadcast::Sender<String>, watch::Receiver<bool>) -> _ =
            run_coin_quote_listener;
        let _: fn(PgPool, broadcast::Sender<String>, watch::Receiver<bool>) -> _ =
            run_coin_candle_listener;
    }

    // DB-gated integration tests
    #[tokio::test]
    #[ignore]
    async fn db_coin_quote_listener_receives_notify() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::db::connect(&url).await.expect("db connect");
        let (tx, mut rx) = broadcast::channel::<String>(16);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let pool2 = pool.clone();
        tokio::spawn(run_coin_quote_listener(pool2, tx, shutdown_rx));

        // Give the listener a moment to connect.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Trigger NOTIFY from a separate connection.
        sqlx::query("SELECT pg_notify('coin_quote_updated', 'test-payload')")
            .execute(&pool)
            .await
            .expect("pg_notify failed");

        let msg = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("timeout waiting for notification")
            .expect("recv failed");

        assert_eq!(msg, "test-payload");
        let _ = shutdown_tx.send(true);
    }
}
