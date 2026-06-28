//! Health endpoints: `/healthz/live` and `/healthz/ready` (SPEC-OBS-001).
//!
//! - REQ-OBS-002: `/healthz/live` → 200 always (process-only, no dependency checks).
//! - REQ-OBS-003: `/healthz/ready` → 200 only when: app initialised + DB reachable;
//!   otherwise 503. A 2-second cache throttles DB query rate under concurrent probe traffic.
//! - REQ-OBS-004: `/healthz/ready` → 503 during shutdown grace window.

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Json, Router};
use serde::Serialize;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// DB-probe result cache TTL — limits probe-driven database load (REQ-OBS-003).
const CACHE_TTL: Duration = Duration::from_secs(2);

// @MX:ANCHOR: [AUTO] HealthState — Kubernetes traffic-admission readiness gate
// @MX:REASON: fan_in >= 3: main.rs (startup + shutdown), readiness handler, tests.
//             set_ready() MUST only be called after DB ping + migrations + workers complete (REQ-OBS-003/040).
//             set_shutting_down() MUST be called as first shutdown step (REQ-OBS-004/030).
// @MX:SPEC: SPEC-OBS-001 REQ-OBS-002 REQ-OBS-003 REQ-OBS-004 REQ-OBS-030 REQ-OBS-040
#[derive(Clone)]
pub struct HealthState {
    inner: Arc<Inner>,
}

struct Inner {
    /// True once startup prerequisites (DB + migrations + workers) are complete.
    ready: AtomicBool,
    /// True once the shutdown grace window begins (forces /healthz/ready → 503).
    shutting_down: AtomicBool,
    /// Production pool for DB ping; None skips the ping (used in offline unit tests).
    pool: Option<sqlx::PgPool>,
    /// TTL-cached readiness result to avoid hammering the DB under probe traffic.
    cache: RwLock<CachedResult>,
}

#[derive(Default)]
struct CachedResult {
    result: Option<ReadinessResult>,
    checked_at: Option<Instant>,
}

#[derive(Clone)]
struct ReadinessResult {
    ok: bool,
    failed: Vec<String>,
}

impl HealthState {
    /// Production constructor — performs a real DB ping on each cache miss.
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self {
            inner: Arc::new(Inner {
                ready: AtomicBool::new(false),
                shutting_down: AtomicBool::new(false),
                pool: Some(pool),
                cache: RwLock::new(CachedResult::default()),
            }),
        }
    }

    /// Offline / test constructor — DB ping always passes (pool = None).
    pub fn for_test() -> Self {
        Self {
            inner: Arc::new(Inner {
                ready: AtomicBool::new(false),
                shutting_down: AtomicBool::new(false),
                pool: None,
                cache: RwLock::new(CachedResult::default()),
            }),
        }
    }

    /// Mark the service ready — called after DB ping + migrations + workers spawn (REQ-OBS-040).
    pub fn set_ready(&self) {
        self.inner.ready.store(true, Ordering::Release);
    }

    /// Enter the shutdown grace window — forces `/healthz/ready` → 503 (REQ-OBS-004/030).
    pub fn set_shutting_down(&self) {
        self.inner.shutting_down.store(true, Ordering::Release);
    }

    // @MX:WARN: [AUTO] check_readiness uses dual async RwLock (cache + ready/shutdown flags); read→write upgrade on cache miss
    // @MX:REASON: 2 s TTL bounds DB query rate under concurrent health probes; upgrade is non-atomic — brief re-check possible but harmless
    async fn check_readiness(&self) -> ReadinessResult {
        // Fast path: serve cached result within TTL.
        {
            let cache = self.inner.cache.read().await;
            if let (Some(result), Some(checked_at)) = (&cache.result, cache.checked_at) {
                if checked_at.elapsed() < CACHE_TTL {
                    return result.clone();
                }
            }
        }

        let mut failed = Vec::new();

        // Shutdown grace window forces 503 first (REQ-OBS-004).
        if self.inner.shutting_down.load(Ordering::Acquire) {
            failed.push("shutting_down: graceful shutdown in progress".to_string());
        }

        // Readiness flag — set after startup prerequisites complete (REQ-OBS-003).
        if !self.inner.ready.load(Ordering::Acquire) {
            failed.push("startup: initialization not yet complete".to_string());
        }

        // DB ping — only if no prior failure (avoids DB call during early startup).
        if failed.is_empty() {
            if let Some(ref pool) = self.inner.pool {
                if let Err(e) = sqlx::query("SELECT 1").fetch_one(pool).await {
                    failed.push(format!("postgres: {e}"));
                }
            }
        }

        let result = ReadinessResult {
            ok: failed.is_empty(),
            failed,
        };

        let mut cache = self.inner.cache.write().await;
        cache.result = Some(result.clone());
        cache.checked_at = Some(Instant::now());

        result
    }
}

// ── Response body ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ReadinessBody {
    pub status: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<String>,
}

// ── Router ────────────────────────────────────────────────────────────────────

/// Build the health Axum router. Bind on `HEALTH_PORT` (default 8081) in `main`.
pub fn router(state: HealthState) -> Router {
    Router::new()
        .route("/healthz/live", get(liveness))
        .route("/healthz/ready", get(readiness))
        .with_state(state)
}

/// GET /healthz/live — always 200, no dependency checks (REQ-OBS-002).
async fn liveness() -> StatusCode {
    StatusCode::OK
}

/// GET /healthz/ready — 200 when ready + DB ok; 503 otherwise (REQ-OBS-003/004).
async fn readiness(State(state): State<HealthState>) -> impl IntoResponse {
    let result = state.check_readiness().await;
    if result.ok {
        (
            StatusCode::OK,
            Json(ReadinessBody {
                status: "ok",
                failed: vec![],
            }),
        )
            .into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ReadinessBody {
                status: "unavailable",
                failed: result.failed,
            }),
        )
            .into_response()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum_test::TestServer;

    // ── Scenario 2: liveness is always 200 (REQ-OBS-002) ──────────────────────

    #[tokio::test]
    async fn liveness_always_returns_200() {
        let state = HealthState::for_test();
        let router = router(state);
        let server = TestServer::new(router);
        let resp = server.get("/healthz/live").await;
        assert_eq!(
            resp.status_code(),
            StatusCode::OK,
            "/healthz/live must always return 200"
        );
    }

    #[tokio::test]
    async fn liveness_returns_200_even_when_not_ready() {
        let state = HealthState::for_test();
        // Do NOT call set_ready — service is not initialised
        let router = router(state);
        let server = TestServer::new(router);
        let resp = server.get("/healthz/live").await;
        assert_eq!(resp.status_code(), StatusCode::OK);
    }

    // ── Scenario 3: readiness gates on initialization (REQ-OBS-003) ───────────

    #[tokio::test]
    async fn readiness_503_when_not_initialized() {
        let state = HealthState::for_test();
        // Not ready (no set_ready call)
        let router = router(state);
        let server = TestServer::new(router);
        let resp = server.get("/healthz/ready").await;
        assert_eq!(
            resp.status_code(),
            StatusCode::SERVICE_UNAVAILABLE,
            "/healthz/ready must return 503 before initialization"
        );
    }

    #[tokio::test]
    async fn readiness_200_when_ready_no_db_check() {
        let state = HealthState::for_test();
        state.set_ready(); // pool = None → DB ping always passes
        let router = router(state);
        let server = TestServer::new(router);
        let resp = server.get("/healthz/ready").await;
        assert_eq!(
            resp.status_code(),
            StatusCode::OK,
            "/healthz/ready must return 200 when initialized and DB ok"
        );
    }

    // ── Scenario 4: readiness 503 during shutdown grace (REQ-OBS-004/030) ──────

    #[tokio::test]
    async fn readiness_503_during_shutdown_grace() {
        let state = HealthState::for_test();
        state.set_ready();
        state.set_shutting_down(); // Flip to 503 immediately
        let router = router(state);
        let server = TestServer::new(router);
        let resp = server.get("/healthz/ready").await;
        assert_eq!(
            resp.status_code(),
            StatusCode::SERVICE_UNAVAILABLE,
            "/healthz/ready must return 503 during shutdown grace (REQ-OBS-004)"
        );
    }

    // ── Readiness body shape ────────────────────────────────────────────────────

    #[tokio::test]
    async fn readiness_body_has_status_ok_when_ready() {
        let state = HealthState::for_test();
        state.set_ready();
        let router = router(state);
        let server = TestServer::new(router);
        let resp = server.get("/healthz/ready").await;
        assert_eq!(resp.status_code(), StatusCode::OK);
        let body: serde_json::Value = resp.json();
        assert_eq!(body["status"], "ok", "body.status must be 'ok' when ready");
        assert!(
            body.get("failed").is_none()
                || body["failed"]
                    .as_array()
                    .map(|a| a.is_empty())
                    .unwrap_or(true),
            "failed array must be absent or empty when ready"
        );
    }

    #[tokio::test]
    async fn readiness_body_has_status_unavailable_when_503() {
        let state = HealthState::for_test();
        // Do not call set_ready
        let router = router(state);
        let server = TestServer::new(router);
        let resp = server.get("/healthz/ready").await;
        assert_eq!(resp.status_code(), StatusCode::SERVICE_UNAVAILABLE);
        let body: serde_json::Value = resp.json();
        assert_eq!(
            body["status"], "unavailable",
            "body.status must be 'unavailable' when not ready"
        );
        let failed = body["failed"].as_array().expect("failed must be an array");
        assert!(
            !failed.is_empty(),
            "failed array must be non-empty when 503"
        );
    }

    // ── Readiness state: set_shutting_down takes precedence ───────────────────

    #[test]
    fn readiness_state_initial_not_ready() {
        let state = HealthState::for_test();
        assert!(
            !state.inner.ready.load(Ordering::Acquire),
            "readiness must start false"
        );
    }

    #[test]
    fn readiness_state_set_ready_flips_flag() {
        let state = HealthState::for_test();
        state.set_ready();
        assert!(
            state.inner.ready.load(Ordering::Acquire),
            "set_ready must flip ready to true"
        );
    }

    #[test]
    fn readiness_state_shutting_down_starts_false() {
        let state = HealthState::for_test();
        assert!(
            !state.inner.shutting_down.load(Ordering::Acquire),
            "shutting_down must start false"
        );
    }

    #[test]
    fn readiness_state_set_shutting_down_flips_flag() {
        let state = HealthState::for_test();
        state.set_shutting_down();
        assert!(
            state.inner.shutting_down.load(Ordering::Acquire),
            "set_shutting_down must flip shutting_down to true"
        );
    }

    // ── ReadinessBody serialization ────────────────────────────────────────────

    #[test]
    fn readiness_body_ok_serializes_without_failed_field() {
        let body = ReadinessBody {
            status: "ok",
            failed: vec![],
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains(r#""status":"ok""#));
        assert!(
            !json.contains("failed"),
            "failed must be omitted when empty"
        );
    }

    #[test]
    fn readiness_body_unavailable_serializes_with_failed() {
        let body = ReadinessBody {
            status: "unavailable",
            failed: vec!["startup: not yet initialized".to_string()],
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains(r#""status":"unavailable""#));
        assert!(
            json.contains("failed"),
            "failed must be included when non-empty"
        );
        assert!(json.contains("startup"));
    }

    // ── DB-check is skipped when pool = None (offline test path) ──────────────

    #[tokio::test]
    async fn readiness_no_db_ping_when_pool_absent() {
        // for_test() uses pool = None → no DB call → test can run offline
        let state = HealthState::for_test();
        state.set_ready();
        // If a DB ping were attempted, it would fail (no real pool) → 503.
        // Since pool is None, it skips the ping → 200.
        let router = router(state);
        let server = TestServer::new(router);
        let resp = server.get("/healthz/ready").await;
        assert_eq!(resp.status_code(), StatusCode::OK);
    }
}
