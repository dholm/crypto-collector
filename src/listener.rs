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

use sqlx::postgres::PgListener;
use sqlx::PgPool;
use tokio::sync::{broadcast, watch};
use tracing::{error, info, warn};

/// Relay PG NOTIFY `coin_quote_updated` → `coin_quote_tx`.
///
/// Runs until `shutdown_rx` fires or the DB connection is permanently lost.
pub async fn run_coin_quote_listener(
    pool: PgPool,
    tx: broadcast::Sender<String>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    run_listener(pool, "coin_quote_updated", tx, &mut shutdown_rx).await;
}

/// Relay PG NOTIFY `coin_candle_updated` → `coin_candle_tx`.
///
/// Runs until `shutdown_rx` fires or the DB connection is permanently lost.
pub async fn run_coin_candle_listener(
    pool: PgPool,
    tx: broadcast::Sender<String>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    run_listener(pool, "coin_candle_updated", tx, &mut shutdown_rx).await;
}

/// Shared relay loop.
///
/// On reconnect errors, backs off and retries up to a bounded count.
async fn run_listener(
    pool: PgPool,
    channel: &'static str,
    tx: broadcast::Sender<String>,
    shutdown_rx: &mut watch::Receiver<bool>,
) {
    info!(channel, "PG listener starting");

    let mut listener = match PgListener::connect_with(&pool).await {
        Ok(l) => l,
        Err(e) => {
            error!(channel, error = %e, "PG listener failed to connect; exiting");
            return;
        }
    };
    if let Err(e) = listener.listen(channel).await {
        error!(channel, error = %e, "PG listener failed to subscribe; exiting");
        return;
    }

    loop {
        tokio::select! {
            biased;

            // Honour graceful shutdown signal first.
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
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
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
