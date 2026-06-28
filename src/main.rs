/// Crypto Collector entry point.
///
/// SPEC-DB-001 minimal: connect pool + run migrations + log.
/// Future SPECs (SPEC-API-001, SPEC-SCHED-001, SPEC-OBS-001) will extend this with
/// HTTP server startup, background workers, and observability.
use anyhow::{Context, Result};
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    // Minimal tracing setup — SPEC-OBS-001 will replace with full JSON + OTLP.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let database_url =
        std::env::var("DATABASE_URL").context("DATABASE_URL environment variable not set")?;

    let _pool = crypto_collector::db::connect(&database_url)
        .await
        .context("Failed to connect to database and apply migrations")?;

    info!("crypto-collector: database connected and migrations applied");
    // Future SPECs will start the HTTP server and background workers here.

    Ok(())
}
