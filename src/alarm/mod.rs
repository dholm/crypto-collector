//! Alarm Center integration (SPEC-ALARM-001).
//!
//! Batch 1 contributed the pure fingerprint/mapping catalogue ([`catalog`]) and the
//! [`AlarmClient`] HTTP contract. Batch 2 (this batch) adds the [`registry`]
//! (`HealthRegistry`) and [`reconciler`] (the near-stateless sweep loop) plus the
//! `collectors`/`main` wiring that spawns it as a fourth supervised worker, and wires
//! the Tier 1 conditions (provider-unreachable, all-providers-down, provider
//! rate-limited/cooldown, provider-credit-exhausted). Tier 2/3 conditions and the
//! fatal startup-config raise remain Batch 3 scope.

pub mod catalog;
pub mod reconciler;
pub mod registry;

pub use catalog::{AlarmSpec, Condition, Severity};
pub use registry::HealthRegistry;

use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Duration;
use tracing::error;

const SOURCE_SERVICE: &str = "crypto-collector";

/// The `POST /api/v1/alarms` request body (alarm-center.yaml `RaiseAlarmRequest`).
///
/// `timeout_seconds` is ALWAYS populated — never `None` — so the server always
/// installs/refreshes an auto-clear deadline (REQ-ALARM-052/053); omitting it would
/// revert the alarm to never-expire.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RaiseAlarmRequest<'a> {
    fingerprint: &'a str,
    source_service: &'static str,
    component: &'a str,
    severity: Severity,
    code: &'a str,
    title: &'a str,
    description: &'a str,
    labels: &'a BTreeMap<String, String>,
    details: &'a BTreeMap<String, String>,
    timeout_seconds: u64,
}

/// Thin `reqwest`-backed client for the Alarm Center HTTP API (SPEC-ALARM-001
/// Milestone 2).
///
/// Holds ONE shared `reqwest::Client` (REQ-ALARM-005). Every method enforces a
/// per-attempt timeout, a small bounded retry count, and swallows delivery failures
/// after retries are exhausted — mirroring the existing
/// `let _ = pacer::signal_cooldown(...)` idiom (`coingecko.rs:904`). Callers never see
/// an `Err` from `raise`/`raise_once`/`clear`: alarm delivery must never block, panic, or
/// degrade a collector (REQ-ALARM-007).
///
/// @MX:ANCHOR: the single egress point to the alarm center.
/// @MX:REASON: every raise/clear MUST enforce timeout + bounded retry + swallow-error
/// and MUST NOT propagate errors to callers (REQ-ALARM-005/006/007/008).
pub struct AlarmClient {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    timeout: Duration,
    max_retries: u32,
    ttl_secs: u64,
}

impl AlarmClient {
    /// Construct a client with explicit settings (testable without reading env vars).
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        timeout_ms: u64,
        max_retries: u32,
        ttl_secs: u64,
    ) -> Self {
        let client = reqwest::Client::builder().build().expect("reqwest client");
        Self {
            client,
            base_url: base_url.into(),
            api_key,
            timeout: Duration::from_millis(timeout_ms),
            max_retries,
            ttl_secs,
        }
    }

    /// Construct a client from `src/config.rs` free functions, or `None` when
    /// `ALARM_CENTER_URL` is unset/empty (the feature gate, REQ-ALARM-001/002).
    pub fn from_config() -> Option<Self> {
        let base_url = crate::config::alarm_center_url()?;
        Some(Self::new(
            base_url,
            crate::config::alarm_center_api_key(),
            crate::config::alarm_center_timeout_ms(),
            crate::config::alarm_center_max_retries(),
            crate::config::alarm_ttl_secs(),
        ))
    }

    fn apply_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => req.header(reqwest::header::AUTHORIZATION, format!("Bearer {key}")),
            None => req,
        }
    }

    /// Raise (or dedup-heartbeat) an alarm, always carrying
    /// `timeoutSeconds = ALARM_TTL_SECS`. Retries up to `ALARM_CENTER_MAX_RETRIES` times;
    /// on exhaustion logs `error!` and returns without propagating (REQ-ALARM-006/007).
    pub async fn raise(&self, spec: &AlarmSpec) {
        self.raise_with_retries(spec, self.max_retries).await;
    }

    /// The one-shot fatal startup-config raise: exactly one attempt, zero retries, still
    /// carrying `timeoutSeconds` (REQ-ALARM-035).
    pub async fn raise_once(&self, spec: &AlarmSpec) {
        self.raise_with_retries(spec, 0).await;
    }

    async fn raise_with_retries(&self, spec: &AlarmSpec, retries: u32) {
        let body = RaiseAlarmRequest {
            fingerprint: &spec.fingerprint,
            source_service: SOURCE_SERVICE,
            component: spec.component,
            severity: spec.severity,
            code: spec.code,
            title: &spec.title,
            description: &spec.description,
            labels: &spec.labels,
            details: &spec.details,
            timeout_seconds: self.ttl_secs,
        };
        let url = format!("{}/api/v1/alarms", self.base_url);
        let attempts = retries + 1;
        for attempt in 0..attempts {
            let last_attempt = attempt + 1 == attempts;
            let req = self.apply_auth(self.client.post(&url).timeout(self.timeout).json(&body));
            match req.send().await {
                Ok(resp) if resp.status().is_success() => return,
                Ok(resp) if last_attempt => {
                    error!(
                        fingerprint = %spec.fingerprint,
                        status = %resp.status(),
                        "alarm raise failed after exhausting retries"
                    );
                }
                Err(err) if last_attempt => {
                    error!(
                        fingerprint = %spec.fingerprint,
                        error = %err,
                        "alarm raise failed after exhausting retries"
                    );
                }
                _ => {}
            }
        }
    }

    /// Optional Critical/Error fast-clear path (REQ-ALARM-014). A `404` response (the
    /// fingerprint was never raised, or already TTL-expired) is treated as success
    /// (REQ-ALARM-008). Same timeout/retry/swallow-error contract as `raise`.
    pub async fn clear(&self, fingerprint: &str) {
        let url = format!("{}/api/v1/alarms/{}/clear", self.base_url, fingerprint);
        let attempts = self.max_retries + 1;
        for attempt in 0..attempts {
            let last_attempt = attempt + 1 == attempts;
            let req = self.apply_auth(self.client.post(&url).timeout(self.timeout));
            match req.send().await {
                Ok(resp)
                    if resp.status().is_success()
                        || resp.status() == reqwest::StatusCode::NOT_FOUND =>
                {
                    return;
                }
                Ok(resp) if last_attempt => {
                    error!(
                        fingerprint = %fingerprint,
                        status = %resp.status(),
                        "alarm clear failed after exhausting retries"
                    );
                }
                Err(err) if last_attempt => {
                    error!(
                        fingerprint = %fingerprint,
                        error = %err,
                        "alarm clear failed after exhausting retries"
                    );
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sample_spec() -> AlarmSpec {
        catalog::to_alarm_spec(&Condition::AllProvidersDown)
    }

    // ── Scenario 3b: every raise carries timeoutSeconds (REQ-ALARM-052/053) ────
    // ── Scenario 4: new (201) vs repeat (200) both accepted (REQ-ALARM-013/015/017) ─

    #[tokio::test]
    async fn raise_sends_correct_body_and_accepts_201_then_200() {
        let server = MockServer::start().await;
        let spec = sample_spec();
        let expected_fingerprint = spec.fingerprint.clone();

        Mock::given(method("POST"))
            .and(path("/api/v1/alarms"))
            .respond_with(move |req: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                assert_eq!(body["sourceService"], json!("crypto-collector"));
                assert_eq!(body["fingerprint"], json!(expected_fingerprint));
                assert_eq!(body["timeoutSeconds"], json!(75));
                ResponseTemplate::new(201)
            })
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/v1/alarms"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = AlarmClient::new(server.uri(), None, 5_000, 3, 75);

        // First raise: accepted as create (201).
        client.raise(&spec).await;
        // Repeat raise: accepted as dedup heartbeat (200).
        client.raise(&spec).await;
    }

    // ── Scenario 6: clear-404 is treated as success (REQ-ALARM-008) ────────────

    #[tokio::test]
    async fn clear_404_is_treated_as_success_not_error() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path(
                "/api/v1/alarms/crypto-collector:all-providers-down/clear",
            ))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = AlarmClient::new(server.uri(), None, 5_000, 3, 75);

        // Must not panic and must return normally (no error propagation).
        client.clear("crypto-collector:all-providers-down").await;
    }

    // ── Scenario 6b: fast-clear POSTs the fingerprint path ──────────────────────

    #[tokio::test]
    async fn clear_posts_to_fingerprint_path() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/v1/alarms/crypto-collector:db-unreachable/clear"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = AlarmClient::new(server.uri(), None, 5_000, 3, 75);
        client.clear("crypto-collector:db-unreachable").await;
    }

    // ── Scenario 7: timeout + bounded retry then swallow-and-log (REQ-ALARM-006/007) ─

    #[tokio::test]
    async fn raise_retries_then_swallows_error_on_persistent_failure() {
        let server = MockServer::start().await;
        let spec = sample_spec();

        // Always fail: the client must exhaust retries and return without panicking.
        Mock::given(method("POST"))
            .and(path("/api/v1/alarms"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = AlarmClient::new(server.uri(), None, 500, 2, 75);
        // Must complete without propagating an error and without panicking.
        client.raise(&spec).await;
    }

    #[tokio::test]
    async fn clear_retries_then_swallows_error_on_persistent_failure() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path(
                "/api/v1/alarms/crypto-collector:all-providers-down/clear",
            ))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = AlarmClient::new(server.uri(), None, 500, 2, 75);
        client.clear("crypto-collector:all-providers-down").await;
    }

    // ── Scenario 19: startup fatal raise = exactly one attempt, zero retries ───

    #[tokio::test]
    async fn raise_once_makes_exactly_one_attempt() {
        let server = MockServer::start().await;
        let spec = catalog::to_alarm_spec(&Condition::StartupConfigError);

        Mock::given(method("POST"))
            .and(path("/api/v1/alarms"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        // max_retries=3 but raise_once must still make only 1 attempt.
        let client = AlarmClient::new(server.uri(), None, 500, 3, 75);
        client.raise_once(&spec).await;

        // wiremock's `.expect(1)` is verified on Drop of the MockServer; explicit
        // verification also confirms during the test body.
        server.verify().await;
    }

    #[tokio::test]
    async fn raise_once_still_carries_timeout_seconds() {
        let server = MockServer::start().await;
        let spec = catalog::to_alarm_spec(&Condition::StartupConfigError);

        Mock::given(method("POST"))
            .and(path("/api/v1/alarms"))
            .respond_with(move |req: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                assert_eq!(body["timeoutSeconds"], json!(75));
                ResponseTemplate::new(201)
            })
            .mount(&server)
            .await;

        let client = AlarmClient::new(server.uri(), None, 500, 0, 75);
        client.raise_once(&spec).await;
    }

    // ── REQ-ALARM-009: Authorization header attached when api key set ──────────

    #[tokio::test]
    async fn raise_attaches_bearer_auth_header_when_api_key_set() {
        let server = MockServer::start().await;
        let spec = sample_spec();

        Mock::given(method("POST"))
            .and(path("/api/v1/alarms"))
            .respond_with(move |req: &wiremock::Request| {
                let auth = req
                    .headers
                    .get("authorization")
                    .expect("authorization header present")
                    .to_str()
                    .unwrap();
                assert_eq!(auth, "Bearer secret-key");
                ResponseTemplate::new(201)
            })
            .mount(&server)
            .await;

        let client = AlarmClient::new(server.uri(), Some("secret-key".to_string()), 5_000, 0, 75);
        client.raise(&spec).await;
    }

    #[tokio::test]
    async fn raise_omits_auth_header_when_api_key_unset() {
        let server = MockServer::start().await;
        let spec = sample_spec();

        Mock::given(method("POST"))
            .and(path("/api/v1/alarms"))
            .respond_with(move |req: &wiremock::Request| {
                assert!(req.headers.get("authorization").is_none());
                ResponseTemplate::new(201)
            })
            .mount(&server)
            .await;

        let client = AlarmClient::new(server.uri(), None, 5_000, 0, 75);
        client.raise(&spec).await;
    }
}
