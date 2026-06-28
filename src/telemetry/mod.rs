//! Structured logging and OpenTelemetry tracing (SPEC-OBS-001 REQ-OBS-020..024).
//!
//! - REQ-OBS-020: JSON logs to stdout via `tracing-subscriber`, filtered by `RUST_LOG`.
//! - REQ-OBS-021: OTLP/gRPC trace export to `OTEL_EXPORTER_OTLP_ENDPOINT` when set.
//! - REQ-OBS-022: Startup succeeds and logging continues when endpoint is unset.
//! - REQ-OBS-023: Per-request span via `TraceLayer` (wired in `api/mod.rs`).
//! - REQ-OBS-024: Service name, version, and deployment.environment resource attributes.

use anyhow::Result;
use opentelemetry::KeyValue;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::{propagation::TraceContextPropagator, trace::SdkTracerProvider, Resource};
use std::sync::OnceLock;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

static TRACER_PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

// @MX:ANCHOR: [AUTO] telemetry::init — global JSON subscriber + optional OTLP tracer installed here
// @MX:REASON: fan_in >= 3: main.rs, tests, future sub-crates.
//             Subscriber is installed globally; calling init() twice panics.
//             Guard at call site with OnceLock if needed. (REQ-OBS-020/021/022)
// @MX:SPEC: SPEC-OBS-001 REQ-OBS-020 REQ-OBS-021 REQ-OBS-022 REQ-OBS-024

/// Initialise structured JSON logging and (conditionally) OTLP/gRPC trace export.
///
/// # Arguments
///
/// - `log_level`: `RUST_LOG` filter string (e.g. `"info"`, `"debug,sqlx=warn"`).
/// - `otel_endpoint`: OTLP/gRPC exporter URL, or `None` to disable export (REQ-OBS-022).
/// - `service_name`: `service.name` trace resource attribute.
/// - `service_version`: `service.version` attribute (`OTEL_SERVICE_VERSION`).
/// - `deployment_environment`: `deployment.environment` attribute (`DEPLOYMENT_ENVIRONMENT`).
///
/// # Behaviour
///
/// When `otel_endpoint` is `None`: installs the JSON subscriber only — tracing export
/// is disabled but logging continues without error (REQ-OBS-022).
///
/// When `otel_endpoint` is `Some`: also installs the OTel tracing layer with W3C
/// `TraceContextPropagator` so inbound `traceparent` headers are extracted correctly
/// (REQ-OBS-021).
pub fn init(
    log_level: &str,
    otel_endpoint: Option<&str>,
    service_name: &str,
    service_version: &str,
    deployment_environment: &str,
) -> Result<()> {
    // W3C TraceContext propagator must be set before any OTel span is created so
    // that inbound traceparent headers are extracted (REQ-OBS-021/023).
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());

    let env_filter = EnvFilter::try_new(log_level).unwrap_or_else(|_| EnvFilter::new("info"));

    let fmt_layer = tracing_subscriber::fmt::layer().json();

    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer);

    if let Some(endpoint) = otel_endpoint {
        let resource = Resource::builder_empty()
            .with_attributes([
                KeyValue::new("service.name", service_name.to_owned()),
                KeyValue::new("service.version", service_version.to_owned()),
                KeyValue::new("deployment.environment", deployment_environment.to_owned()),
            ])
            .build();

        let exporter = SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()?;

        let provider = SdkTracerProvider::builder()
            .with_resource(resource)
            .with_batch_exporter(exporter)
            .build();

        opentelemetry::global::set_tracer_provider(provider.clone());
        let _ = TRACER_PROVIDER.set(provider);

        let tracer = opentelemetry::global::tracer(service_name.to_owned());
        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
        registry.with(otel_layer).init();
    } else {
        registry.init();
    }

    Ok(())
}

/// Flush and shut down the global tracer provider (call during graceful shutdown).
///
/// No-op when OTLP was not configured (REQ-OBS-022).
pub fn shutdown() {
    if let Some(provider) = TRACER_PROVIDER.get() {
        if let Err(e) = provider.shutdown() {
            tracing::error!("failed to shut down tracer provider: {e}");
        }
    }
}

// ── W3C context extractor (used by TraceLayer in main.rs) ─────────────────────

/// Extracts OpenTelemetry context from HTTP request headers.
///
/// Used with `TraceLayer::new_for_http().make_span_with(OtelMakeSpan)` in the
/// API router setup (REQ-OBS-023).
pub struct HeaderExtractor<'a>(pub &'a axum::http::HeaderMap);

impl<'a> opentelemetry::propagation::Extractor for HeaderExtractor<'a> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::propagation::Extractor;

    // ── REQ-OBS-022: init without OTLP endpoint must succeed ──────────────────

    /// Scenario 8: OTel is a no-op when endpoint unset — startup must not fail (REQ-OBS-022).
    ///
    /// NOTE: This test is marked #[ignore] because `tracing_subscriber::registry().init()`
    /// sets a global subscriber and panics if called twice in the same process.
    /// Run in isolation with: `cargo test telemetry_init_no_endpoint -- --ignored`
    #[test]
    #[ignore = "installs global tracing subscriber — run in isolation"]
    fn telemetry_init_no_endpoint_succeeds() {
        init("info", None, "crypto-collector", "0.1.0", "test")
            .expect("telemetry init without OTLP endpoint must succeed");
    }

    // ── W3C propagator is set even without an OTLP endpoint ───────────────────

    /// Scenario 8: W3C propagator is registered (REQ-OBS-021/022).
    ///
    /// set_text_map_propagator is idempotent and does not require subscriber init.
    #[test]
    fn w3c_propagator_can_be_set_without_subscriber() {
        // Calling set_text_map_propagator is safe from any test without a subscriber.
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
        let field_count = opentelemetry::global::get_text_map_propagator(|p| p.fields().count());
        assert!(
            field_count > 0,
            "W3C propagator must expose at least one field (traceparent)"
        );
    }

    // ── HeaderExtractor ────────────────────────────────────────────────────────

    /// Scenario 7/8: HeaderExtractor returns header value for known keys.
    #[test]
    fn header_extractor_returns_traceparent() {
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
        let headers = axum::http::HeaderMap::new();
        let extractor = HeaderExtractor(&headers);
        assert_eq!(extractor.get("traceparent"), None);
    }

    #[test]
    fn header_extractor_keys_lists_all_headers() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("traceparent", "dummy".parse().unwrap());
        headers.insert("x-request-id", "req-1".parse().unwrap());
        let extractor = HeaderExtractor(&headers);
        let keys = extractor.keys();
        assert!(keys.contains(&"traceparent"));
        assert!(keys.contains(&"x-request-id"));
    }

    // ── shutdown() is a no-op when OTLP was not configured ────────────────────

    #[test]
    fn telemetry_shutdown_is_noop_when_no_provider() {
        // TRACER_PROVIDER is OnceLock; if not set, shutdown() must not panic.
        shutdown(); // must not panic
    }
}
