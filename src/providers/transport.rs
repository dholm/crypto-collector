//! Shared provider request-path scaffolding (SPEC-PROV-002 — F-14, F-11, F-19).
//!
//! This module is the **single request-path frame** every provider endpoint routes
//! through. It generalises Bitstamp's already-factored `acquire()`/`signal_rate_limit()`
//! shape across all providers so the pacer + 429 + response-parse scaffolding cannot
//! silently drift per-endpoint again (F-10 arose precisely because two endpoints were
//! written outside the frame).
//!
//! Three pieces:
//! - [`build_client`] — the one place a provider `reqwest::Client` is constructed. Applies
//!   a total-request timeout (default 30 s), a shorter connect timeout (default 10 s), and
//!   a `User-Agent` (REQ-PROV-055/056/057). No provider client is built without a timeout,
//!   so a black-holed upstream can never hang a worker indefinitely.
//!   Timeouts are env-tunable via `PROVIDER_HTTP_TIMEOUT_SECS` /
//!   `PROVIDER_HTTP_CONNECT_TIMEOUT_SECS`, zero-guarded back to the defaults.
//! - [`paced`] — the prelude + 429 postlude around a provider call: `local_throttle.acquire()`
//!   then `pacer::acquire_slot`, and on a `RateLimited` result `pacer::signal_cooldown`
//!   before returning the error (REQ-PROV-050).
//! - [`get_json`] — the response epilogue: `429 → RateLimited`, other non-success →
//!   `Http { status, body }`, JSON decode → `Parse` (REQ-PROV-051).

use std::future::Future;
use std::time::Duration;

use serde::de::DeserializeOwned;
use sqlx::PgPool;

use super::ProviderError;
use crate::config;
use crate::pacer::{self, LocalThrottle};

// ── Shared client constructor (F-11, F-19) ────────────────────────────────────

/// Construct a provider `reqwest::Client` with the timeout + connect-timeout + User-Agent
/// the whole provider layer shares, resolving the timeouts from the environment
/// (`PROVIDER_HTTP_TIMEOUT_SECS` / `PROVIDER_HTTP_CONNECT_TIMEOUT_SECS`, zero-guarded).
///
// @MX:ANCHOR: [AUTO] build_client — the single provider reqwest::Client constructor
// @MX:REASON: Availability invariant — every provider client (CoinGecko, Binance, Bitstamp;
//             fan_in >= 3) is built here, and NONE may be built without a total-request
//             timeout: a client without a timeout hangs a worker indefinitely after the
//             pacer already charged a credit, while health probes stay green (F-11).
// @MX:SPEC: SPEC-PROV-002 REQ-PROV-055 REQ-PROV-056 REQ-PROV-057
pub fn build_client() -> reqwest::Client {
    build_client_with(
        config::provider_http_timeout_secs(),
        config::provider_http_connect_timeout_secs(),
    )
}

/// [`build_client`] with explicit timeouts (seconds) — the testable core, so a wiremock
/// timeout test can use a short timeout without mutating the process environment.
///
/// Both timeouts are strictly positive by the `config::resolve_timeout_secs` guard, so a
/// `Duration::ZERO` (unbounded) timeout can never reach the builder (REQ-PROV-056).
pub fn build_client_with(timeout_secs: u64, connect_timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .gzip(true)
        .timeout(Duration::from_secs(timeout_secs))
        .connect_timeout(Duration::from_secs(connect_timeout_secs))
        .user_agent(concat!("crypto-collector/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("reqwest client")
}

// ── Shared request prelude + 429 postlude (F-14) ──────────────────────────────

/// Run a provider call through the standard pacer prelude + 429 postlude.
///
/// Prelude: `throttle.acquire()` (per-replica burst smoothing) then
/// `pacer::acquire_slot(pool, provider)` (fleet-wide egress governor). Postlude: on a
/// `ProviderError::RateLimited` result, `pacer::signal_cooldown` is fired for the provider
/// before the error is returned, so every replica backs off (REQ-PROV-050). Any other
/// result (including other errors) is returned unchanged.
///
/// This is byte-for-byte the behaviour of the inline
/// `acquire → call → match RateLimited => { cooldown; signal; return }` blocks it replaces,
/// so migrating each endpoint onto it is behaviour-preserving.
///
// @MX:ANCHOR: [AUTO] paced — the shared request-path frame (throttle + slot + 429 postlude)
// @MX:REASON: Drift prevention — every CoinGecko/Binance/Bitstamp endpoint (fan_in >= 3)
//             routes through this single enforcement point for throttle + pacer slot +
//             signal_cooldown-on-429. F-10 existed because search/tickers were written
//             OUTSIDE this frame and never signalled cooldown; the frame makes that class
//             of drift structurally impossible.
// @MX:SPEC: SPEC-PROV-002 REQ-PROV-050 REQ-PROV-052 REQ-PROV-053 REQ-PROV-054
pub async fn paced<T, F, Fut>(
    pool: &PgPool,
    throttle: &LocalThrottle,
    provider: &str,
    call: F,
) -> Result<T, ProviderError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    throttle.acquire().await;
    pacer::acquire_slot(pool, provider)
        .await
        .map_err(ProviderError::Pacer)?;

    match call().await {
        Err(ProviderError::RateLimited) => {
            let cooldown_ms = config::pacer_cooldown_ms(provider);
            let _ = pacer::signal_cooldown(pool, provider, cooldown_ms).await;
            Err(ProviderError::RateLimited)
        }
        other => other,
    }
}

// ── Shared response epilogue (F-14) ───────────────────────────────────────────

/// Map an HTTP response to a typed value or the canonical `ProviderError`.
///
/// `429 → RateLimited`; any other non-success → `Http { status, body }` (body captured for
/// diagnostics); a successful body that fails to decode → `Parse` with `ctx` naming the
/// endpoint (e.g. `"markets parse error: ..."`). Endpoints that special-case a status
/// (e.g. Bitstamp's `404 → Ok(vec![])`) MUST handle it at the call site BEFORE delegating
/// the generic tail here (REQ-PROV-051).
///
// @MX:ANCHOR: [AUTO] get_json — the shared response epilogue (429/status/parse mapping)
// @MX:REASON: Drift prevention — every CoinGecko/Binance/Bitstamp endpoint (fan_in >= 3)
//             maps its HTTP response through this single epilogue, so the 429/non-success/
//             decode-failure classification cannot silently diverge per endpoint again
//             (the copy-pasted-epilogue half of F-14, alongside `paced`'s prelude half).
// @MX:SPEC: SPEC-PROV-002 REQ-PROV-051 REQ-PROV-052
pub async fn get_json<T: DeserializeOwned>(
    resp: reqwest::Response,
    ctx: &str,
) -> Result<T, ProviderError> {
    let status = resp.status().as_u16();
    if status == 429 {
        return Err(ProviderError::RateLimited);
    }
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ProviderError::Http { status, body });
    }
    resp.json::<T>()
        .await
        .map_err(|e| ProviderError::Parse(format!("{ctx} parse error: {e}")))
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ── Scenario 3 (REQ-PROV-055/056): a hanging upstream errors at the timeout ──

    #[tokio::test]
    async fn hanging_upstream_errors_at_timeout_instead_of_hanging() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/slow"))
            // Respond after 5 s — well beyond the 1 s test timeout below.
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
            .mount(&server)
            .await;

        let client = build_client_with(1, 1); // 1 s total, 1 s connect
        let start = Instant::now();
        let result = client.get(format!("{}/slow", server.uri())).send().await;

        assert!(
            result.is_err(),
            "a hanging upstream must error at the timeout, not hang"
        );
        let err = result.unwrap_err();
        assert!(err.is_timeout(), "the error must be a timeout, got: {err}");
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "must error near the 1 s timeout, took {:?}",
            start.elapsed()
        );
        // A reqwest timeout surfaces through the provider layer as ProviderError::Network,
        // which the chain and alarm already classify correctly (REQ-PROV-055).
        let perr: ProviderError = err.into();
        assert!(
            matches!(perr, ProviderError::Network(_)),
            "a timeout must surface as ProviderError::Network"
        );
    }

    // ── Scenario 3 (REQ-PROV-055): the shared constructor attaches the User-Agent ──

    #[tokio::test]
    async fn build_client_attaches_user_agent() {
        let server = MockServer::start().await;
        let expected_ua = concat!("crypto-collector/", env!("CARGO_PKG_VERSION"));
        // The mock only matches when the User-Agent header is present with our value.
        Mock::given(method("GET"))
            .and(path("/"))
            .and(header("user-agent", expected_ua))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = build_client();
        let resp = client.get(server.uri()).send().await.expect("send");
        assert_eq!(
            resp.status(),
            200,
            "request must carry User-Agent {expected_ua}"
        );
    }

    // ── Scenario 1 (REQ-PROV-051): get_json response epilogue ─────────────────

    async fn response_from(server_status: u16, body: &str) -> reqwest::Response {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(server_status).set_body_string(body))
            .mount(&server)
            .await;
        // A plain client (no need for the shared constructor here).
        reqwest::get(server.uri()).await.expect("send")
    }

    #[tokio::test]
    async fn get_json_maps_429_to_rate_limited() {
        let resp = response_from(429, "Too Many Requests").await;
        let result: Result<Vec<i64>, _> = get_json(resp, "test").await;
        assert!(matches!(result, Err(ProviderError::RateLimited)));
    }

    #[tokio::test]
    async fn get_json_maps_non_success_to_http_with_body() {
        let resp = response_from(503, "upstream down").await;
        let result: Result<Vec<i64>, _> = get_json(resp, "test").await;
        match result {
            Err(ProviderError::Http { status, body }) => {
                assert_eq!(status, 503);
                assert_eq!(body, "upstream down");
            }
            other => panic!("expected Http{{503}}, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_json_parses_a_successful_body() {
        let resp = response_from(200, "[1,2,3]").await;
        let result: Vec<i64> = get_json(resp, "test").await.expect("parse");
        assert_eq!(result, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn get_json_maps_undecodable_success_body_to_parse_error() {
        let resp = response_from(200, "not json").await;
        let result: Result<Vec<i64>, _> = get_json(resp, "widgets").await;
        match result {
            Err(ProviderError::Parse(msg)) => {
                assert!(
                    msg.contains("widgets parse error"),
                    "Parse message must carry the ctx, got: {msg}"
                );
            }
            other => panic!("expected Parse, got {other:?}"),
        }
    }
}
