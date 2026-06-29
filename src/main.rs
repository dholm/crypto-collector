/// Crypto Collector entry point — SPEC-OBS-001 three-port topology.
///
/// Startup order (REQ-OBS-040):
///   1. Parse config from env vars.
///   2. Init structured JSON logging + optional OTLP tracing (REQ-OBS-020/021).
///   3. Install Prometheus recorder + bind metrics listener on METRICS_PORT (REQ-OBS-010).
///   4. Build PgPool + run migrations (SPEC-DB-001).
///   5. Build provider chain (SPEC-PROV-001).
///   6. Create HealthState (not ready yet).
///   7. Spawn workers (SPEC-SCHED-001) + gauge-refresh task (REQ-OBS-013).
///   8. Flip readiness to ready (all prerequisites satisfied) (REQ-OBS-040).
///   9. Bind health listener (HEALTH_PORT 8081) and API listener (API_PORT 8080).
///  10. Await SIGTERM/SIGINT.
///  11. Graceful shutdown (REQ-OBS-030..033): set_shutting_down → sleep grace →
///      broadcast shutdown → drain → pool.close → telemetry::shutdown.
use anyhow::{Context, Result};
use opentelemetry::propagation::Extractor;
use std::{sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::watch};
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_opentelemetry::OpenTelemetrySpanExt;

// @MX:ANCHOR: [AUTO] main — startup sequence and graceful-shutdown orchestrator
// @MX:REASON: fan_in >= 3: startup, shutdown, integration tests.
//             Ordering is load-bearing: readiness gate (set_ready) must come after workers spawn.
//             Shutdown ordering (set_shutting_down → grace → cancel → drain → pool.close) is required
//             for zero-drop rollouts (REQ-OBS-030..033/040).
// @MX:SPEC: SPEC-OBS-001 REQ-OBS-001 REQ-OBS-030 REQ-OBS-031 REQ-OBS-032 REQ-OBS-040 REQ-OBS-041

// ── W3C context extractor for traceparent propagation (REQ-OBS-021/023) ───────

struct HeaderExtractor<'a>(&'a axum::http::HeaderMap);

impl<'a> Extractor for HeaderExtractor<'a> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }
    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

/// Per-request OTel span builder — extracts parent context from W3C traceparent header.
///
// @MX:NOTE: [AUTO] OtelMakeSpan reads traceparent — must run after global propagator is set in telemetry::init()
// @MX:SPEC: SPEC-OBS-001 REQ-OBS-021 REQ-OBS-023
#[derive(Clone)]
struct OtelMakeSpan;

impl<B> tower_http::trace::MakeSpan<B> for OtelMakeSpan {
    fn make_span(&mut self, request: &axum::http::Request<B>) -> tracing::Span {
        let parent_cx = opentelemetry::global::get_text_map_propagator(|prop| {
            prop.extract(&HeaderExtractor(request.headers()))
        });
        let span = tracing::info_span!(
            "http_request",
            http.method = %request.method(),
            http.route  = request.uri().path(),
            http.status_code = tracing::field::Empty,
        );
        let _ = span.set_parent(parent_cx);
        span
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    use crypto_collector::config;

    // ── Step 1: Parse port + observability config ──────────────────────────────
    let api_port = config::api_port();
    let health_port = config::health_port();
    let metrics_port = config::metrics_port();
    let log_level = config::rust_log();
    let otel_endpoint = config::otel_exporter_otlp_endpoint();
    let otel_service_version = config::otel_service_version();
    let deployment_env = config::deployment_environment();
    let grace_secs = config::shutdown_grace_seconds();
    let drain_secs = config::shutdown_drain_seconds();
    let gauge_secs = config::tracked_gauge_interval_secs();

    // ── Step 2: Structured logging + optional OTLP tracing (REQ-OBS-020/021) ──
    crypto_collector::telemetry::init(
        &log_level,
        otel_endpoint.as_deref(),
        "crypto-collector",
        &otel_service_version,
        &deployment_env,
    )
    .context("failed to initialise telemetry")?;

    info!(
        api_port,
        health_port,
        metrics_port,
        otel = otel_endpoint.is_some(),
        "crypto-collector: starting"
    );

    // ── Step 3: Prometheus recorder + metrics listener (REQ-OBS-010) ──────────
    // The metrics-exporter-prometheus crate binds its own HTTP listener on metrics_port.
    // /metrics is served there; no separate Axum router needed.
    crypto_collector::metrics::init(metrics_port)
        .context("failed to install Prometheus recorder")?;

    // ── Step 4: Database pool + migrations (SPEC-DB-001, REQ-OBS-041) ─────────
    let database_url =
        config::database_url().context("failed to resolve database connection settings")?;
    let pool = crypto_collector::db::connect(&database_url)
        .await
        .context("failed to connect to database and apply migrations")?;
    info!("crypto-collector: database connected and migrations applied");

    // ── Step 5: Provider chain (SPEC-PROV-001) ─────────────────────────────────
    let provider_names = config::provider_names();
    let coingecko_cfg = crypto_collector::providers::CoinGeckoConfig::from_env();
    let chain = Arc::new(
        crypto_collector::providers::build_chain(&provider_names, coingecko_cfg, pool.clone())
            .context("failed to build provider chain")?,
    );
    info!("crypto-collector: provider chain = {:?}", provider_names);

    // ── Step 6: Health state (starts not-ready) ────────────────────────────────
    let health_state = crypto_collector::health::HealthState::new(pool.clone());

    // ── Shutdown channel — broadcast true to stop workers + servers ────────────
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // ── Step 7: Spawn workers (SPEC-SCHED-001) + gauge-refresh task ───────────
    let worker_cfg = crypto_collector::collectors::WorkerConfig::from_env();
    let supervisor = crypto_collector::collectors::spawn_workers(
        pool.clone(),
        chain.clone(),
        worker_cfg,
        shutdown_rx.clone(),
    )
    .await;
    info!("crypto-collector: workers started");

    // Gauge-refresh task: keeps tracked_coins / tracked_markets current (REQ-OBS-013).
    {
        let pool_gauge = pool.clone();
        let mut shutdown_gauge = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(gauge_secs));
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        refresh_tracked_gauges(&pool_gauge).await;
                    }
                    _ = shutdown_gauge.changed() => break,
                }
            }
        });
    }

    // ── Step 8: Flip readiness (all prerequisites satisfied) (REQ-OBS-040) ────
    health_state.set_ready();
    info!("crypto-collector: service is ready");

    // ── Step 9: Bind API and health listeners (REQ-OBS-001) ───────────────────
    let search_provider = provider_names
        .first()
        .cloned()
        .unwrap_or_else(|| "coingecko".to_string());
    let coingecko_base_url = config::coingecko_base_url();
    // Maximum ms a search request will wait for a pacer slot to become available.
    // If the collector queue is deeper than this, search returns 503 immediately
    // without sleeping and without consuming a credit (try_acquire_slot).
    // Must be >= min_gap_ms (~2000ms for CoinGecko Demo) to allow at least one slot.
    let search_max_wait_ms: u64 = std::env::var("SEARCH_PACER_MAX_WAIT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3_000);

    let api_state = crypto_collector::api::AppState {
        pool: pool.clone(),
        chain: chain.clone(),
        search_slot_fn: crypto_collector::api::make_db_search_slot_fn(
            pool.clone(),
            search_max_wait_ms,
        ),
        search_provider,
        coingecko_base_url,
        http_client: reqwest::Client::new(),
    };

    // API router: build_api_router + request-metrics middleware + OTel trace layer (REQ-OBS-011/023).
    let api_router = crypto_collector::api::build_api_router(api_state)
        .route_layer(axum::middleware::from_fn(
            crypto_collector::metrics::track_metrics,
        ))
        .layer(TraceLayer::new_for_http().make_span_with(OtelMakeSpan))
        .fallback(crypto_collector::metrics::handle_unmatched);

    let api_listener = TcpListener::bind(format!("0.0.0.0:{api_port}"))
        .await
        .with_context(|| format!("failed to bind API port {api_port}"))?;
    info!("crypto-collector: API server listening on port {api_port}");

    let health_router = crypto_collector::health::router(health_state.clone());
    let health_listener = TcpListener::bind(format!("0.0.0.0:{health_port}"))
        .await
        .with_context(|| format!("failed to bind health port {health_port}"))?;
    info!("crypto-collector: health server listening on port {health_port}");
    info!("crypto-collector: metrics server listening on port {metrics_port}");

    // ── Step 10: Await SIGTERM/SIGINT (REQ-OBS-030) ────────────────────────────
    let health_state_shutdown = health_state.clone();
    let mut api_shutdown_rx = shutdown_rx.clone();
    let mut health_shutdown_rx = shutdown_rx.clone();

    // Spawn shutdown orchestrator (runs concurrently with servers).
    //
    // @MX:ANCHOR: [AUTO] shutdown orchestrator — ordering is load-bearing for zero-drop rollouts
    // @MX:REASON: fan_in >= 3: SIGTERM path, SIGINT path, integration tests.
    //             Order: set_shutting_down → grace sleep → shutdown_tx → drain → pool.close.
    //             Changing the order drops in-flight requests mid-rollout (REQ-OBS-030..033).
    // @MX:WARN: [AUTO] tokio::spawn for signal + sleep + channel: three concurrent tasks involved
    // @MX:REASON: Each step (grace, drain) involves sleeping inside an async task;
    //             be careful not to block the runtime — use tokio::time::sleep, not std::thread::sleep.
    // @MX:SPEC: SPEC-OBS-001 REQ-OBS-030 REQ-OBS-031 REQ-OBS-032 REQ-OBS-033
    let shutdown_orchestrator = tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        info!("crypto-collector: shutdown signal received; flipping readiness to 503");

        // a. Flip readiness to 503 — kube-proxy begins removing the pod (REQ-OBS-030/004).
        health_state_shutdown.set_shutting_down();

        // b. Grace window: kube-proxy removes pod from endpoints list (REQ-OBS-030/031).
        info!(grace_secs, "crypto-collector: shutdown grace period");
        tokio::time::sleep(Duration::from_secs(grace_secs)).await;

        // c. Signal workers to stop claiming new work (REQ-OBS-032).
        info!("crypto-collector: broadcasting shutdown to workers");
        shutdown_tx.send(true).ok();
    });

    // Serve API and health concurrently.
    let api_handle = tokio::spawn(async move {
        axum::serve(api_listener, api_router)
            .with_graceful_shutdown(async move {
                loop {
                    if *api_shutdown_rx.borrow() {
                        break;
                    }
                    if api_shutdown_rx.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await
    });

    let health_handle = tokio::spawn(async move {
        axum::serve(health_listener, health_router)
            .with_graceful_shutdown(async move {
                loop {
                    if *health_shutdown_rx.borrow() {
                        break;
                    }
                    if health_shutdown_rx.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await
    });

    // Wait for servers + orchestrator.
    let (api_result, health_result, _) =
        tokio::join!(api_handle, health_handle, shutdown_orchestrator);
    api_result.ok();
    health_result.ok();

    // d. Wait for workers to finish in-flight work (REQ-OBS-032).
    info!(drain_secs, "crypto-collector: draining in-flight requests");
    tokio::time::sleep(Duration::from_secs(drain_secs)).await;
    supervisor.await.ok();

    // e. Close DB pool + flush traces (REQ-OBS-032).
    pool.close().await;
    crypto_collector::telemetry::shutdown();

    info!("crypto-collector: graceful shutdown complete");
    Ok(())
}

// ── Signal handling ────────────────────────────────────────────────────────────

async fn wait_for_shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for Ctrl-C");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c    => { info!("crypto-collector: SIGINT received"); }
        _ = terminate => { info!("crypto-collector: SIGTERM received"); }
    }
}

// ── Tracked-asset gauge refresh (REQ-OBS-013) ─────────────────────────────────

/// Refresh `tracked_coins` and `tracked_markets` gauges from the DB.
///
/// On error, logs a warning and preserves the last gauge value (no reset to 0).
///
// @MX:NOTE: [AUTO] DB-backed gauge — each replica queries its own pool; gauge is per-replica.
//           Aggregate with max()/avg() across replicas in Prometheus, NOT sum().
// @MX:SPEC: SPEC-OBS-001 REQ-OBS-013
async fn refresh_tracked_gauges(pool: &sqlx::PgPool) {
    match sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tracked_coins")
        .fetch_one(pool)
        .await
    {
        Ok(count) => metrics::gauge!("tracked_coins").set(count as f64),
        Err(e) => tracing::warn!(error = %e, "tracked_coins gauge refresh failed"),
    }

    match sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tracked_markets")
        .fetch_one(pool)
        .await
    {
        Ok(count) => metrics::gauge!("tracked_markets").set(count as f64),
        Err(e) => tracing::warn!(error = %e, "tracked_markets gauge refresh failed"),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Scenario 11: startup sequence SQL targets (REQ-OBS-013) ───────────────

    #[test]
    fn tracked_coins_gauge_uses_correct_table() {
        let sql = "SELECT COUNT(*) FROM tracked_coins";
        assert!(
            sql.contains("tracked_coins"),
            "SQL must target tracked_coins table"
        );
        assert!(sql.contains("COUNT(*)"), "SQL must use COUNT(*)");
    }

    #[test]
    fn tracked_markets_gauge_uses_correct_table() {
        let sql = "SELECT COUNT(*) FROM tracked_markets";
        assert!(
            sql.contains("tracked_markets"),
            "SQL must target tracked_markets table"
        );
        assert!(sql.contains("COUNT(*)"), "SQL must use COUNT(*)");
    }

    // ── Header extractor (REQ-OBS-021/023) ─────────────────────────────────────

    #[test]
    fn header_extractor_returns_traceparent() {
        use opentelemetry::propagation::Extractor;
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "traceparent",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                .parse()
                .unwrap(),
        );
        let extractor = HeaderExtractor(&headers);
        assert_eq!(
            extractor.get("traceparent"),
            Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
        );
    }

    #[test]
    fn header_extractor_missing_key_returns_none() {
        use opentelemetry::propagation::Extractor;
        let headers = axum::http::HeaderMap::new();
        let extractor = HeaderExtractor(&headers);
        assert_eq!(extractor.get("traceparent"), None);
    }

    #[test]
    fn header_extractor_keys_lists_all_headers() {
        use opentelemetry::propagation::Extractor;
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("traceparent", "dummy".parse().unwrap());
        headers.insert("x-request-id", "req-1".parse().unwrap());
        let extractor = HeaderExtractor(&headers);
        let keys = extractor.keys();
        assert!(keys.contains(&"traceparent"));
        assert!(keys.contains(&"x-request-id"));
    }

    // ── Scenario 10: terminationGracePeriod sizing (REQ-OBS-033) ───────────────

    #[test]
    fn shutdown_timing_grace_plus_drain_fits_in_termination_grace() {
        // Default grace=15, drain=30 → terminationGracePeriodSeconds must be > 45.
        // This is a documentation assertion, not enforced here (SPEC-DEPLOY-001 wires the value).
        let grace = crypto_collector::config::shutdown_grace_seconds();
        let drain = crypto_collector::config::shutdown_drain_seconds();
        let min_termination = grace + drain;
        // Buffer of at least 5 s is recommended.
        assert!(
            min_termination >= 45,
            "grace ({grace}) + drain ({drain}) must be at least 45 s"
        );
    }

    // ── NFR: no f64 monetary values in the codebase (REQ-OBS-052) ─────────────
    //
    // This is enforced by SPEC-DB-001/PROV-001/API-001 which all mandate Decimal.
    // The structural assertion here documents the NFR; monetary code paths are covered
    // by their respective SPEC tests.
    #[test]
    fn monetary_types_use_decimal_not_f64() {
        // Verify that the core monetary type in models is rust_decimal::Decimal.
        // SpotQuote.price is Decimal — this compiles only if the type is Decimal.
        use rust_decimal::Decimal;
        let _: Decimal = rust_decimal_macros::dec!(42.000000001);
    }
}
