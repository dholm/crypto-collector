// @MX:ANCHOR: [AUTO] connect() — pool initializer and migration runner
// @MX:REASON: Every DB consumer (main, workers, integration tests) calls this exactly once per
//             process start. Changing pool config or the migration path here affects all call sites.
//             fan_in >= 3 (main, integration tests, future SPEC-SCHED-001 workers).

use anyhow::Result;
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::time::Duration;
use tokio::sync::watch;
use tracing::{info, warn};

/// Pool size shared by every connection path (eager, lazy).
const MAX_CONNECTIONS: u32 = 10;

/// Retry backoff bounds for `migrate_with_retry` (REQ-OBS-041).
const RETRY_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const RETRY_MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Connection-acquire timeout for the lazy production pool (REQ-OBS-041).
///
/// Bounds how long a single migration attempt (and each readiness DB ping) waits
/// on an unreachable database before failing, so the retry loop keeps logging and
/// backing off promptly instead of hanging on sqlx's 30 s default.
const LAZY_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// Initialize a PostgreSQL connection pool and apply sqlx migrations.
///
/// Establishes a live connection eagerly, then applies migrations. Used by
/// integration tests and any caller that requires the database to be reachable
/// immediately. `main` uses [`connect_lazy`] + [`migrate_with_retry`] instead so
/// startup survives a transiently-unavailable database (REQ-OBS-041).
///
/// Uses `sqlx::migrate!("./migrations")` — runtime migration application from embedded SQL files.
/// No DATABASE_URL is needed at compile time; no `.sqlx/` offline cache is required for this SPEC.
pub async fn connect(database_url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(MAX_CONNECTIONS)
        .connect(database_url)
        .await?;

    run_migrations(&pool).await?;

    info!("migrations applied successfully");
    Ok(pool)
}

/// Build a connection pool without contacting the database.
///
/// `connect_lazy` only parses the connection string; the first real connection is
/// established on demand. This lets `main` construct the pool, hand it to the
/// health server, and bind the health/readiness listeners *before* the database
/// is confirmed reachable — keeping `/healthz/live` answerable during a database
/// outage so Kubernetes does not kill the pod (REQ-OBS-041).
pub fn connect_lazy(database_url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(MAX_CONNECTIONS)
        .acquire_timeout(LAZY_ACQUIRE_TIMEOUT)
        .connect_lazy(database_url)?;
    Ok(pool)
}

/// Apply migrations, retrying with capped exponential backoff until the database
/// is reachable (REQ-OBS-041).
///
/// The service must not crash-loop when the database is unavailable at startup.
/// Instead of propagating the connection error, this loops — logging each failure
/// and backing off from [`RETRY_INITIAL_BACKOFF`] up to [`RETRY_MAX_BACKOFF`] —
/// until `sqlx::migrate!` succeeds.
///
/// Returns `Ok(true)` once migrations are applied, or `Ok(false)` if a shutdown
/// signal (`shutdown` flips to `true`) arrives first — so SIGTERM during a
/// database outage exits cleanly instead of hanging.
//
// @MX:NOTE: [AUTO] Called before set_ready(); readiness stays 503 for the whole retry window.
// @MX:SPEC: SPEC-OBS-001 REQ-OBS-041
pub async fn migrate_with_retry(
    pool: &PgPool,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<bool> {
    // Fast path: already asked to shut down before we started.
    if *shutdown.borrow() {
        return Ok(false);
    }

    let mut backoff = RETRY_INITIAL_BACKOFF;
    loop {
        // Race the attempt against shutdown — a single attempt can block up to the
        // pool's acquire timeout, so we must be able to abort it mid-flight.
        tokio::select! {
            res = run_migrations(pool) => match res {
                Ok(()) => {
                    info!("migrations applied successfully");
                    return Ok(true);
                }
                Err(e) => warn!(
                    error = %e,
                    retry_in_secs = backoff.as_secs(),
                    "database unavailable at startup; retrying"
                ),
            },
            res = shutdown.changed() => {
                if res.is_err() || *shutdown.borrow() {
                    return Ok(false);
                }
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            res = shutdown.changed() => {
                // Sender dropped or flipped to true → abort startup.
                if res.is_err() || *shutdown.borrow() {
                    return Ok(false);
                }
            }
        }

        backoff = (backoff * 2).min(RETRY_MAX_BACKOFF);
    }
}

/// Run embedded migrations against an existing pool.
async fn run_migrations(pool: &PgPool) -> Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── REQ-OBS-041: migrate_with_retry honors shutdown during a DB outage ────
    //
    // Uses a lazy pool pointed at an unreachable port so no live database is
    // needed. The first migration attempt fails fast (connection refused); the
    // retry then sleeps on the backoff. Flipping shutdown must break the loop and
    // return Ok(false) rather than retrying forever or erroring out.

    #[tokio::test]
    async fn migrate_with_retry_returns_false_on_shutdown_during_outage() {
        // Port 1 is unbound → connection refused → migration attempts fail.
        let pool = connect_lazy("postgres://localhost:1/does_not_exist")
            .expect("lazy pool construction never contacts the DB");
        let (tx, mut rx) = watch::channel(false);

        let handle = tokio::spawn(async move { migrate_with_retry(&pool, &mut rx).await });

        // Let at least one attempt fail and enter backoff, then request shutdown.
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(true).expect("receiver still alive");

        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("must not hang: shutdown breaks the retry loop")
            .expect("task must not panic")
            .expect("must not error: outage is retried, not propagated");
        assert!(
            !result,
            "shutdown during outage must return Ok(false), not migrate"
        );
    }

    #[tokio::test]
    async fn migrate_with_retry_returns_false_when_shutdown_already_set() {
        let pool = connect_lazy("postgres://localhost:1/does_not_exist").expect("lazy pool");
        let (_tx, mut rx) = watch::channel(true); // already shutting down

        let result =
            tokio::time::timeout(Duration::from_secs(2), migrate_with_retry(&pool, &mut rx))
                .await
                .expect("must return immediately when shutdown is already set")
                .expect("must not error");
        assert!(
            !result,
            "must return Ok(false) without attempting migration"
        );
    }
}
