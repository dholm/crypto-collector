/// Crypto Collector entry point.
///
/// SPEC-DB-001: connect pool + run migrations.
/// SPEC-PROV-001: build provider chain from PROVIDERS env var.
/// SPEC-SCHED-001: spawn live-poller, collection-queue worker, backfill worker (REQ-SCHED-050/051).
use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::watch;
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

    let pool = crypto_collector::db::connect(&database_url)
        .await
        .context("Failed to connect to database and apply migrations")?;

    info!("crypto-collector: database connected and migrations applied");

    // Build the provider chain from PROVIDERS env var (REQ-PROV-002/003).
    let provider_names = crypto_collector::config::provider_names();
    let coingecko_cfg = crypto_collector::providers::CoinGeckoConfig::from_env();
    let chain = Arc::new(
        crypto_collector::providers::build_chain(&provider_names, coingecko_cfg, pool.clone())
            .context("Failed to build provider chain")?,
    );

    info!("crypto-collector: provider chain = {:?}", provider_names);

    // Shutdown channel (REQ-SCHED-050): broadcast true on SIGTERM/SIGINT.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Spawn worker supervisor (REQ-SCHED-050/051).
    let cfg = crypto_collector::collectors::WorkerConfig::from_env();
    let supervisor =
        crypto_collector::collectors::spawn_workers(pool, chain, cfg, shutdown_rx).await;

    info!("crypto-collector: workers started; waiting for shutdown signal");

    // Wait for SIGTERM or SIGINT.
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).context("SIGTERM handler")?;
        let mut sigint = signal(SignalKind::interrupt()).context("SIGINT handler")?;
        tokio::select! {
            _ = sigterm.recv() => info!("SIGTERM received"),
            _ = sigint.recv() => info!("SIGINT received"),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.context("Ctrl-C handler")?;
        info!("Ctrl-C received");
    }

    // Broadcast shutdown to all workers.
    shutdown_tx.send(true).ok();
    supervisor.await.ok();

    info!("crypto-collector: graceful shutdown complete");
    Ok(())
}
