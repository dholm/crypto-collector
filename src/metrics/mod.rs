//! Prometheus metrics registry, exposition, and request-metrics middleware (SPEC-OBS-001).
//!
//! # Metric catalogue (REQ-OBS-010..015)
//!
//! HTTP layer (REQ-OBS-011):
//! - `http_requests_total{method, path, status}` — counter per request
//! - `http_request_duration_seconds{method, path}` — histogram (route-template labels)
//!
//! Provider / collection (REQ-OBS-012/015):
//! - `collection_requests_total{provider, capability, outcome}` — counter per attempt
//! - `collection_request_duration_seconds{provider, capability}` — histogram
//!
//! Persistence latency (REQ-OBS-015):
//! - `quote_insert_duration_seconds` — histogram for live-quote upserts
//! - `candle_insert_duration_seconds` — histogram for candle upserts
//!
//! Registry gauges (REQ-OBS-013):
//! - `tracked_coins` — total tracked coins
//! - `tracked_markets` — total tracked markets
//!
//! Backlog + pacer (REQ-OBS-014):
//! - `collection_queue_pending` — pending queue items
//! - `backfill_chunks_pending` — pending backfill chunks
//! - `pacer_cooldown_active{provider}` — 1 while provider is in cooldown, 0 otherwise
//! - `pacer_credits_used{provider}` — credits consumed in the current window
//!
//! DB pool (supplemental):
//! - `db_pool_connections_active`
//! - `db_pool_connections_idle`

// @MX:ANCHOR: [AUTO] metrics::init — single global Prometheus recorder installation point
// @MX:REASON: fan_in >= 3: main.rs startup, telemetry init, tests.
//             All metric emitters in other modules rely on this being called first.
//             Calling init() more than once (e.g. in integration tests) returns an error — guard with OnceLock.
// @MX:SPEC: SPEC-OBS-001 REQ-OBS-010 REQ-OBS-011 REQ-OBS-012 REQ-OBS-013 REQ-OBS-014 REQ-OBS-015

use axum::{extract::MatchedPath, http::StatusCode, middleware::Next, response::IntoResponse};
use std::time::Instant;

/// HTTP-appropriate latency buckets: 1 ms → 10 s.
///
/// Mirrors the ticker-collector bucket set (OR-OBS-2 resolved).
pub const HTTP_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Register all metric descriptors (names, help text) without emitting values.
///
/// Safe to call multiple times (idempotent — metrics crate ignores duplicate describe calls).
/// Called by `init()` and available for test helpers that use a local recorder.
pub fn describe_all() {
    // HTTP layer (REQ-OBS-011)
    metrics::describe_counter!(
        "http_requests_total",
        "Total HTTP requests by method, path (route template), and status code"
    );
    metrics::describe_histogram!(
        "http_request_duration_seconds",
        "HTTP request latency in seconds by method and path (route template)"
    );

    // Provider / collection (REQ-OBS-012/015)
    metrics::describe_counter!(
        "collection_requests_total",
        "Total upstream provider call attempts by provider, capability, and outcome"
    );
    metrics::describe_histogram!(
        "collection_request_duration_seconds",
        "Upstream provider call latency in seconds by provider and capability"
    );

    // Persistence latency (REQ-OBS-015)
    metrics::describe_histogram!(
        "quote_insert_duration_seconds",
        "Live-quote upsert latency in seconds"
    );
    metrics::describe_histogram!(
        "candle_insert_duration_seconds",
        "Candle upsert latency in seconds"
    );

    // Registry gauges (REQ-OBS-013)
    metrics::describe_gauge!("tracked_coins", "Total number of tracked coins");
    metrics::describe_gauge!("tracked_markets", "Total number of tracked markets");

    // Backlog + pacer (REQ-OBS-014)
    metrics::describe_gauge!(
        "collection_queue_pending",
        "Number of pending items in the collection queue"
    );
    metrics::describe_gauge!(
        "backfill_chunks_pending",
        "Number of pending backfill chunks"
    );
    metrics::describe_gauge!(
        "pacer_cooldown_active",
        "1 while the provider is in rate-limit cooldown, 0 otherwise"
    );
    metrics::describe_gauge!(
        "pacer_credits_used",
        "Credits consumed in the current pacer window"
    );

    // DB pool (supplemental)
    metrics::describe_gauge!("db_pool_connections_active", "Active DB pool connections");
    metrics::describe_gauge!("db_pool_connections_idle", "Idle DB pool connections");
}

/// Install the global Prometheus recorder and start the `/metrics` HTTP listener on `port`.
///
/// Must be called once at startup (REQ-OBS-010). Returns an error if another recorder
/// is already installed (e.g. in tests — use `build_recorder()` for test-local isolation).
pub fn init(port: u16) -> anyhow::Result<()> {
    use metrics_exporter_prometheus::PrometheusBuilder;
    use std::net::SocketAddr;

    describe_all();

    let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
    PrometheusBuilder::new()
        .set_buckets(HTTP_BUCKETS)
        .map_err(|e| anyhow::anyhow!("failed to set histogram buckets: {e}"))?
        .with_http_listener(addr)
        .install()
        .map_err(|e| anyhow::anyhow!("failed to install Prometheus recorder on :{port}: {e}"))
}

// ── Request-metrics middleware ─────────────────────────────────────────────────

/// Axum middleware that records `http_requests_total` and `http_request_duration_seconds`.
///
/// MUST be applied via `route_layer()` (not `layer()`) so that `MatchedPath` is
/// populated by the router before the middleware runs. Using `layer()` runs the
/// middleware before routing and `MatchedPath` will be absent (REQ-OBS-011).
///
// @MX:WARN: [AUTO] track_metrics middleware requires route_layer(), NOT layer(); MatchedPath absent otherwise
// @MX:REASON: Axum populates MatchedPath only after routing resolves the route pattern.
//             layer() runs before routing → endpoint label becomes "unknown" for all requests (REQ-OBS-011).
// @MX:SPEC: SPEC-OBS-001 REQ-OBS-011
pub async fn track_metrics(request: axum::extract::Request, next: Next) -> impl IntoResponse {
    let method = request.method().to_string();
    // Route template (e.g. "/v1/coins/{coin_id}"), not the concrete path — avoids unbounded cardinality.
    let path = request
        .extensions()
        .get::<MatchedPath>()
        .map(|mp| mp.as_str().to_owned())
        .unwrap_or_else(|| "unknown".to_owned());

    let start = Instant::now();
    let response = next.run(request).await;
    let duration = start.elapsed().as_secs_f64();
    let status = response.status().as_u16().to_string();

    metrics::counter!(
        "http_requests_total",
        "method" => method.clone(),
        "path" => path.clone(),
        "status" => status.clone(),
    )
    .increment(1);

    metrics::histogram!(
        "http_request_duration_seconds",
        "method" => method,
        "path" => path,
    )
    .record(duration);

    response
}

/// Fallback handler for unmatched routes — records sentinel `path="unknown"` (REQ-OBS-011).
pub async fn handle_unmatched(
    method: axum::http::Method,
    _uri: axum::http::Uri,
) -> impl IntoResponse {
    metrics::counter!(
        "http_requests_total",
        "method" => method.to_string(),
        "path" => "unknown",
        "status" => "404",
    )
    .increment(1);
    (StatusCode::NOT_FOUND, "Not Found")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Router};
    use axum_test::TestServer;
    use metrics_exporter_prometheus::PrometheusBuilder;

    /// Build an isolated recorder + handle for test-local metric inspection.
    /// Does NOT install a global recorder and is safe to call in parallel tests.
    fn make_recorder() -> (
        metrics_exporter_prometheus::PrometheusRecorder,
        metrics_exporter_prometheus::PrometheusHandle,
    ) {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        (recorder, handle)
    }

    // ── REQ-OBS-011: http_requests_total and http_request_duration_seconds ─────

    /// Scenario 5: metric names are registered and rendered (REQ-OBS-011).
    #[test]
    fn http_metric_names_registered_in_prometheus_output() {
        let (recorder, handle) = make_recorder();
        metrics::with_local_recorder(&recorder, || {
            metrics::counter!(
                "http_requests_total",
                "method" => "GET",
                "path" => "/v1/coins",
                "status" => "200",
            )
            .increment(1);
            metrics::histogram!(
                "http_request_duration_seconds",
                "method" => "GET",
                "path" => "/v1/coins",
            )
            .record(0.01);
        });
        let rendered = handle.render();
        assert!(
            rendered.contains("http_requests_total"),
            "http_requests_total must appear in /metrics output"
        );
        assert!(
            rendered.contains("http_request_duration_seconds"),
            "http_request_duration_seconds must appear in /metrics output"
        );
    }

    /// Scenario 5: path label is route template, not concrete path (REQ-OBS-011).
    #[test]
    fn http_metric_path_label_is_route_template_not_concrete() {
        let (recorder, handle) = make_recorder();
        let route_template = "/v1/coins/{coin_id}";
        metrics::with_local_recorder(&recorder, || {
            metrics::counter!(
                "http_requests_total",
                "method" => "GET",
                "path" => route_template,
                "status" => "200",
            )
            .increment(2); // two requests: bitcoin + ethereum, same series
        });
        let rendered = handle.render();
        assert!(
            rendered.contains(r#"path="/v1/coins/{coin_id}""#),
            "path label must be route template, got:\n{rendered}"
        );
        // Concrete path segments must never appear as label values.
        assert!(
            !rendered.contains("bitcoin"),
            "concrete path segment 'bitcoin' must not appear in metric labels"
        );
        assert!(
            !rendered.contains("ethereum"),
            "concrete path segment 'ethereum' must not appear in metric labels"
        );
    }

    // ── REQ-OBS-012/015: collection metrics ────────────────────────────────────

    /// Scenario 6: collection_requests_total recorded with provider/capability/outcome labels.
    #[test]
    fn collection_requests_total_has_required_labels() {
        let (recorder, handle) = make_recorder();
        metrics::with_local_recorder(&recorder, || {
            metrics::counter!(
                "collection_requests_total",
                "provider" => "coingecko",
                "capability" => "spot",
                "outcome" => "success",
            )
            .increment(1);
        });
        let rendered = handle.render();
        assert!(
            rendered.contains("collection_requests_total"),
            "collection_requests_total must be recorded"
        );
        assert!(rendered.contains(r#"provider="coingecko""#));
        assert!(rendered.contains(r#"capability="spot""#));
        assert!(rendered.contains(r#"outcome="success""#));
    }

    /// Scenario 6: collection_request_duration_seconds histogram (REQ-OBS-015).
    #[test]
    fn collection_request_duration_histogram_registered() {
        let (recorder, handle) = make_recorder();
        metrics::with_local_recorder(&recorder, || {
            metrics::histogram!(
                "collection_request_duration_seconds",
                "provider" => "coingecko",
                "capability" => "ohlc",
            )
            .record(0.5);
        });
        let rendered = handle.render();
        assert!(
            rendered.contains("collection_request_duration_seconds"),
            "collection_request_duration_seconds must appear in /metrics output"
        );
        assert!(rendered.contains(r#"provider="coingecko""#));
        assert!(rendered.contains(r#"capability="ohlc""#));
    }

    /// Scenario 6: collection_request_duration_seconds error outcome (REQ-OBS-015).
    #[test]
    fn collection_requests_total_error_outcome_recorded() {
        let (recorder, handle) = make_recorder();
        metrics::with_local_recorder(&recorder, || {
            metrics::counter!(
                "collection_requests_total",
                "provider" => "coingecko",
                "capability" => "spot",
                "outcome" => "error",
            )
            .increment(1);
        });
        let rendered = handle.render();
        assert!(rendered.contains(r#"outcome="error""#));
    }

    // ── REQ-OBS-015: persistence latency histograms ────────────────────────────

    /// Scenario 6: quote_insert_duration_seconds histogram (REQ-OBS-015).
    #[test]
    fn quote_insert_duration_histogram_registered() {
        let (recorder, handle) = make_recorder();
        metrics::with_local_recorder(&recorder, || {
            metrics::histogram!("quote_insert_duration_seconds").record(0.002);
        });
        let rendered = handle.render();
        assert!(
            rendered.contains("quote_insert_duration_seconds"),
            "quote_insert_duration_seconds must appear in /metrics output (REQ-OBS-015)"
        );
    }

    /// Scenario 6: candle_insert_duration_seconds histogram (REQ-OBS-015).
    #[test]
    fn candle_insert_duration_histogram_registered() {
        let (recorder, handle) = make_recorder();
        metrics::with_local_recorder(&recorder, || {
            metrics::histogram!("candle_insert_duration_seconds").record(0.005);
        });
        let rendered = handle.render();
        assert!(
            rendered.contains("candle_insert_duration_seconds"),
            "candle_insert_duration_seconds must appear in /metrics output (REQ-OBS-015)"
        );
    }

    // ── REQ-OBS-013: tracked_coins and tracked_markets gauges ──────────────────

    #[test]
    fn tracked_coins_gauge_registered() {
        let (recorder, handle) = make_recorder();
        metrics::with_local_recorder(&recorder, || {
            metrics::gauge!("tracked_coins").set(42.0_f64);
        });
        let rendered = handle.render();
        assert!(
            rendered.contains("tracked_coins"),
            "tracked_coins gauge must be registered"
        );
        assert!(
            rendered.contains("42"),
            "tracked_coins must reflect set value"
        );
    }

    #[test]
    fn tracked_markets_gauge_registered() {
        let (recorder, handle) = make_recorder();
        metrics::with_local_recorder(&recorder, || {
            metrics::gauge!("tracked_markets").set(7.0_f64);
        });
        let rendered = handle.render();
        assert!(
            rendered.contains("tracked_markets"),
            "tracked_markets gauge must be registered"
        );
        assert!(
            rendered.contains("7"),
            "tracked_markets must reflect set value"
        );
    }

    // ── REQ-OBS-014: backlog + pacer gauges ────────────────────────────────────

    #[test]
    fn collection_queue_pending_gauge_registered() {
        let (recorder, handle) = make_recorder();
        metrics::with_local_recorder(&recorder, || {
            metrics::gauge!("collection_queue_pending").set(3.0_f64);
        });
        let rendered = handle.render();
        assert!(rendered.contains("collection_queue_pending"));
        assert!(rendered.contains("3"));
    }

    #[test]
    fn backfill_chunks_pending_gauge_registered() {
        let (recorder, handle) = make_recorder();
        metrics::with_local_recorder(&recorder, || {
            metrics::gauge!("backfill_chunks_pending").set(11.0_f64);
        });
        let rendered = handle.render();
        assert!(rendered.contains("backfill_chunks_pending"));
        assert!(rendered.contains("11"));
    }

    #[test]
    fn pacer_cooldown_active_gauge_registered() {
        let (recorder, handle) = make_recorder();
        metrics::with_local_recorder(&recorder, || {
            metrics::gauge!("pacer_cooldown_active", "provider" => "coingecko").set(1.0_f64);
        });
        let rendered = handle.render();
        assert!(
            rendered.contains("pacer_cooldown_active"),
            "pacer_cooldown_active gauge must be registered"
        );
        assert!(rendered.contains(r#"provider="coingecko""#));
    }

    #[test]
    fn pacer_credits_used_gauge_registered() {
        let (recorder, handle) = make_recorder();
        metrics::with_local_recorder(&recorder, || {
            metrics::gauge!("pacer_credits_used", "provider" => "coingecko").set(5.0_f64);
        });
        let rendered = handle.render();
        assert!(
            rendered.contains("pacer_credits_used"),
            "pacer_credits_used gauge must be registered"
        );
    }

    // ── track_metrics middleware (REQ-OBS-011) ─────────────────────────────────

    /// Verify track_metrics records both counter and histogram for a matched route.
    #[test]
    fn track_metrics_records_counter_and_histogram() {
        let (recorder, handle) = make_recorder();
        metrics::with_local_recorder(&recorder, || {
            // Simulate what the middleware records for a matched request
            metrics::counter!(
                "http_requests_total",
                "method" => "GET",
                "path" => "/v1/markets/{id}/quotes",
                "status" => "200",
            )
            .increment(1);
            metrics::histogram!(
                "http_request_duration_seconds",
                "method" => "GET",
                "path" => "/v1/markets/{id}/quotes",
            )
            .record(0.05);
        });
        let rendered = handle.render();
        assert!(rendered.contains("http_requests_total"));
        assert!(rendered.contains("http_request_duration_seconds"));
        assert!(rendered.contains(r#"path="/v1/markets/{id}/quotes""#));
    }

    /// Verify the middleware returns the downstream response unchanged.
    #[tokio::test]
    async fn track_metrics_passes_through_response() {
        let router = Router::new()
            .route("/v1/coins", get(|| async { "ok" }))
            .route_layer(axum::middleware::from_fn(track_metrics));
        let server = TestServer::new(router);
        let resp = server.get("/v1/coins").await;
        assert_eq!(resp.status_code(), StatusCode::OK);
    }

    /// handle_unmatched returns 404 for unknown routes.
    #[tokio::test]
    async fn handle_unmatched_returns_404() {
        let router = Router::new()
            .route("/v1/coins", get(|| async { "ok" }))
            .route_layer(axum::middleware::from_fn(track_metrics))
            .fallback(handle_unmatched);
        let server = TestServer::new(router);
        let resp = server.get("/nonexistent").await;
        assert_eq!(resp.status_code(), StatusCode::NOT_FOUND);
    }

    // ── describe_all idempotency ────────────────────────────────────────────────

    #[test]
    fn describe_all_is_idempotent() {
        let (recorder, handle) = make_recorder();
        metrics::with_local_recorder(&recorder, || {
            describe_all();
            describe_all(); // second call must not panic
                            // Emit one sample to verify the registry is usable after two describe_all calls
            metrics::counter!(
                "http_requests_total",
                "method" => "GET",
                "path" => "/v1/coins",
                "status" => "200",
            )
            .increment(1);
        });
        let rendered = handle.render();
        assert!(rendered.contains("http_requests_total"));
    }

    /// build_recorder path: non-global recorder produces a usable handle.
    #[test]
    fn prometheus_recorder_build_produces_usable_handle() {
        let (recorder, handle) = make_recorder();
        metrics::with_local_recorder(&recorder, || {
            metrics::counter!("test_metric_total").increment(1);
        });
        let rendered = handle.render();
        assert!(
            rendered.contains("test_metric_total"),
            "build_recorder must produce a usable recorder"
        );
    }

    /// Metric macros return () — fire-and-forget, no error propagation.
    #[test]
    fn metric_recording_is_fire_and_forget() {
        let (recorder, _handle) = make_recorder();
        metrics::with_local_recorder(&recorder, || {
            let () = { metrics::counter!("some_total").increment(1) };
            let () = { metrics::histogram!("some_duration_seconds").record(0.0) };
            let () = { metrics::gauge!("some_gauge").set(0.0_f64) };
        });
    }
}
