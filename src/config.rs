//! Configuration loading from environment variables (SPEC-PROV-001, SPEC-OBS-001).
//!
//! All configuration is env-var-only — no hardcoded secrets, no config files.
//! Mirrors SPEC-DB-001's env-var-only approach.

// ── SPEC-DB-001 database connection configuration ─────────────────────────────

/// Assemble the PostgreSQL connection URL (SPEC-DB-001).
///
/// Mirrors `ticker-collector`'s pattern: the URL is built from discrete
/// `DB_HOST` / `DB_PORT` / `DB_NAME` parts, with optional `DB_USERNAME` /
/// `DB_PASSWORD` credentials (sourced from Kubernetes Secrets in deployment).
/// No `DATABASE_URL` secret is required.
///
/// As a convenience for local development and integration tests, an explicit
/// non-empty `DATABASE_URL` takes precedence when set.
///
/// Env vars: `DATABASE_URL` (optional override), `DB_HOST` (required),
/// `DB_PORT` (default 5432), `DB_NAME` (required), `DB_USERNAME` / `DB_PASSWORD`
/// (optional; both must be present for credentials to be included).
pub fn database_url() -> anyhow::Result<String> {
    if let Ok(url) = std::env::var("DATABASE_URL") {
        if !url.is_empty() {
            return Ok(url);
        }
    }
    let host = required("DB_HOST")?;
    let port = parse_env_u16("DB_PORT", 5432);
    let name = required("DB_NAME")?;
    let username = std::env::var("DB_USERNAME").ok().filter(|s| !s.is_empty());
    let password = std::env::var("DB_PASSWORD").ok().filter(|s| !s.is_empty());
    Ok(build_database_url(
        &host,
        port,
        &name,
        username.as_deref(),
        password.as_deref(),
    ))
}

/// Pure connection-string assembly (testable without environment mutation).
///
/// Credentials are embedded only when both username and password are present.
fn build_database_url(
    host: &str,
    port: u16,
    name: &str,
    username: Option<&str>,
    password: Option<&str>,
) -> String {
    match (username, password) {
        (Some(u), Some(p)) => format!("postgres://{u}:{p}@{host}:{port}/{name}"),
        _ => format!("postgres://{host}:{port}/{name}"),
    }
}

fn required(name: &str) -> anyhow::Result<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => Err(anyhow::anyhow!("required env var {name} is not set")),
    }
}

// ── SPEC-OBS-001 port and observability configuration ─────────────────────────

/// REST API listener port (SPEC-OBS-001 REQ-OBS-001).
///
/// Env var: `API_PORT`. Default: 8080.
pub fn api_port() -> u16 {
    parse_env_u16("API_PORT", 8080)
}

/// Health endpoint listener port (SPEC-OBS-001 REQ-OBS-001).
///
/// Env var: `HEALTH_PORT`. Default: 8081.
pub fn health_port() -> u16 {
    parse_env_u16("HEALTH_PORT", 8081)
}

/// Prometheus metrics listener port (SPEC-OBS-001 REQ-OBS-001/010).
///
/// Env var: `METRICS_PORT`. Default: 9000.
pub fn metrics_port() -> u16 {
    parse_env_u16("METRICS_PORT", 9000)
}

/// Log filter specification (SPEC-OBS-001 REQ-OBS-020).
///
/// Env var: `RUST_LOG`. Default: `"info"`.
/// Passed to `tracing_subscriber::EnvFilter`.
pub fn rust_log() -> String {
    std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string())
}

/// OTLP/gRPC exporter endpoint URL (SPEC-OBS-001 REQ-OBS-021/022).
///
/// Env var: `OTEL_EXPORTER_OTLP_ENDPOINT`. Unset = tracing export disabled.
/// Example: `http://localhost:4317`.
pub fn otel_exporter_otlp_endpoint() -> Option<String> {
    std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Service version attached as a trace resource attribute (SPEC-OBS-001 REQ-OBS-024).
///
/// Env var: `OTEL_SERVICE_VERSION`. Default: Cargo package version.
pub fn otel_service_version() -> String {
    std::env::var("OTEL_SERVICE_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

/// Deployment environment name (SPEC-OBS-001 REQ-OBS-024).
///
/// Env var: `DEPLOYMENT_ENVIRONMENT`. Default: `"unknown"`.
/// Examples: `production`, `staging`, `development`.
pub fn deployment_environment() -> String {
    std::env::var("DEPLOYMENT_ENVIRONMENT").unwrap_or_else(|_| "unknown".to_string())
}

/// Grace period before drain starts, in seconds (SPEC-OBS-001 REQ-OBS-030/031).
///
/// Env var: `SHUTDOWN_GRACE_SECONDS`. Default: 15 s (ticker-collector value, OR-OBS-1).
/// During this window, `/healthz/ready` returns 503 while the pod is removed from kube-proxy.
pub fn shutdown_grace_seconds() -> u64 {
    parse_env_u64("SHUTDOWN_GRACE_SECONDS", 15)
}

/// Maximum time to wait for in-flight requests to complete, in seconds (SPEC-OBS-001 REQ-OBS-031).
///
/// Env var: `SHUTDOWN_DRAIN_SECONDS`. Default: 30 s (ticker-collector value, OR-OBS-1).
pub fn shutdown_drain_seconds() -> u64 {
    parse_env_u64("SHUTDOWN_DRAIN_SECONDS", 30)
}

/// Interval at which `tracked_coins` / `tracked_markets` gauges are refreshed (SPEC-OBS-001 REQ-OBS-013).
///
/// Env var: `TRACKED_GAUGE_INTERVAL_SECS`. Default: 30 s (OR-OBS-3).
pub fn tracked_gauge_interval_secs() -> u64 {
    parse_env_u64("TRACKED_GAUGE_INTERVAL_SECS", 30)
}

/// Provider chain names in declared fallback priority order.
///
/// Env var: `PROVIDERS` (comma-separated, default: `"coingecko"`).
/// Valid names: `coingecko`, `binance`, `coinbase`, `kraken`.
///
/// Example: `PROVIDERS=coingecko,binance` → CoinGecko is primary, Binance is fallback.
pub fn provider_names() -> Vec<String> {
    std::env::var("PROVIDERS")
        .unwrap_or_else(|_| "coingecko".to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// CoinGecko tier: `"demo"` or `"pro"` (default: `"demo"`).
///
/// Env var: `COINGECKO_TIER`.
/// Determines base URL and API key header (REQ-PROV-011, research §2.3).
pub fn coingecko_tier() -> String {
    std::env::var("COINGECKO_TIER")
        .unwrap_or_else(|_| "demo".to_string())
        .to_lowercase()
}

/// CoinGecko base URL.
///
/// Env var: `COINGECKO_BASE_URL` (overrides tier default).
/// Demo default: `https://api.coingecko.com`
/// Pro default: `https://pro-api.coingecko.com`
pub fn coingecko_base_url() -> String {
    if let Ok(url) = std::env::var("COINGECKO_BASE_URL") {
        return url;
    }
    match coingecko_tier().as_str() {
        "pro" => "https://pro-api.coingecko.com".to_string(),
        _ => "https://api.coingecko.com".to_string(),
    }
}

/// CoinGecko API key.
///
/// Env var: `COINGECKO_API_KEY`. Required for Pro tier; optional for Demo (rate-limited without key).
pub fn coingecko_api_key() -> Option<String> {
    std::env::var("COINGECKO_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
}

/// Fleet-wide cooldown duration in milliseconds after a provider returns HTTP 429.
///
/// Env var: `PACER_{PROVIDER}_COOLDOWN_MS` (e.g. `PACER_COINGECKO_COOLDOWN_MS`).
/// Default: 60 000 ms (1 minute).
pub fn pacer_cooldown_ms(provider: &str) -> u64 {
    let key = format!("PACER_{}_COOLDOWN_MS", provider.to_uppercase());
    std::env::var(&key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60_000)
}

// ── SPEC-SCHED-001 scheduling knobs (OR-SCHED-1 resolved) ────────────────────

/// Stable per-replica identifier used in `claimed_by` for lease fencing.
///
/// Env var: `REPLICA_ID` (optional). Defaults to a UUID v4 generated at startup.
/// Stable for the lifetime of the process.
pub fn replica_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        std::env::var("REPLICA_ID").unwrap_or_else(|_| uuid::Uuid::new_v4().to_string())
    })
}

/// Live-quote poll cadence in seconds (global default, per-coin may override).
///
/// Env var: `LIVE_QUOTE_POLL_INTERVAL_SECS`. Default: 60 s.
/// Respects CoinGecko Demo tier budget (30 calls/min = 2 s/call; 60 s handles ~30 active coins).
pub fn live_quote_poll_interval_secs() -> i64 {
    parse_env_i64("LIVE_QUOTE_POLL_INTERVAL_SECS", 60)
}

/// Minimum allowed per-coin live_poll_interval (REQ-API-113/114 lower bound).
///
/// Env var: `LIVE_POLL_MIN_INTERVAL_SECS`. Default: 5 s.
/// The effective floor is `max(live_poll_min_interval_secs, live_quote_poll_interval_secs)`.
pub fn live_poll_min_interval_secs() -> u64 {
    parse_env_u64("LIVE_POLL_MIN_INTERVAL_SECS", 5)
}

/// Maximum allowed per-coin live_poll_interval (REQ-API-113/114 upper bound).
///
/// Env var: `LIVE_POLL_MAX_INTERVAL_SECS`. Default: 3600 s (1 hour).
pub fn live_poll_max_interval_secs() -> u64 {
    parse_env_u64("LIVE_POLL_MAX_INTERVAL_SECS", 3600)
}

/// TTL for the live-poll in-flight claim marker in seconds.
///
/// Env var: `LIVE_POLL_CLAIM_TTL_SECS`. Default: 120 s.
/// Must exceed the time needed to pace + fetch one market (2 s pacer gap + network latency).
pub fn live_poll_claim_ttl_secs() -> i64 {
    parse_env_i64("LIVE_POLL_CLAIM_TTL_SECS", 120)
}

/// Collection-queue worker lease duration in seconds.
///
/// Env var: `COLLECTION_LEASE_SECONDS`. Default: 120 s.
pub fn collection_lease_secs() -> i64 {
    parse_env_i64("COLLECTION_LEASE_SECONDS", 120)
}

/// Collection-queue worker heartbeat renewal cadence in seconds.
///
/// Env var: `COLLECTION_HEARTBEAT_INTERVAL_SECONDS`. Default: 30 s.
pub fn collection_heartbeat_interval_secs() -> u64 {
    parse_env_u64("COLLECTION_HEARTBEAT_INTERVAL_SECONDS", 30)
}

/// Maximum claim attempts before a collection-queue row is permanently failed.
///
/// Env var: `COLLECTION_MAX_ATTEMPTS`. Default: 5.
pub fn collection_max_attempts() -> i32 {
    parse_env_i32("COLLECTION_MAX_ATTEMPTS", 5)
}

/// Sleep duration in milliseconds when the collection queue is empty.
///
/// Env var: `COLLECTION_IDLE_SLEEP_MS`. Default: 1 000 ms.
pub fn collection_idle_sleep_ms() -> u64 {
    parse_env_u64("COLLECTION_IDLE_SLEEP_MS", 1_000)
}

/// Backfill worker chunk lease duration in seconds.
///
/// Env var: `BACKFILL_LEASE_SECONDS`. Default: 300 s (5 min for large historical fetches).
pub fn backfill_lease_secs() -> i64 {
    parse_env_i64("BACKFILL_LEASE_SECONDS", 300)
}

/// Backfill worker heartbeat renewal cadence in seconds.
///
/// Env var: `BACKFILL_HEARTBEAT_INTERVAL_SECONDS`. Default: 60 s.
pub fn backfill_heartbeat_interval_secs() -> u64 {
    parse_env_u64("BACKFILL_HEARTBEAT_INTERVAL_SECONDS", 60)
}

/// Maximum chunk attempts before a backfill chunk is permanently failed.
///
/// Env var: `BACKFILL_MAX_ATTEMPTS`. Default: 5.
pub fn backfill_max_attempts() -> i32 {
    parse_env_i32("BACKFILL_MAX_ATTEMPTS", 5)
}

/// Sleep duration in milliseconds when the backfill chunk queue is empty.
///
/// Env var: `BACKFILL_IDLE_SLEEP_MS`. Default: 1 000 ms.
pub fn backfill_idle_sleep_ms() -> u64 {
    parse_env_u64("BACKFILL_IDLE_SLEEP_MS", 1_000)
}

/// Metadata periodic-refresh cadence in seconds (OR-SCHED-3 resolved).
///
/// Env var: `METADATA_REFRESH_INTERVAL_SECS`. Default: 86 400 s (24 h).
pub fn metadata_refresh_interval_secs() -> u64 {
    parse_env_u64("METADATA_REFRESH_INTERVAL_SECS", 86_400)
}

// ── Internal env-var helpers ──────────────────────────────────────────────────

fn parse_env_u16(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn parse_env_i64(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn parse_env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn parse_env_i32(name: &str, default: i32) -> i32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── SPEC-DB-001 database URL assembly (ticker-collector pattern) ─────────

    #[test]
    fn build_database_url_with_credentials() {
        assert_eq!(
            build_database_url("h", 5433, "n", Some("u"), Some("p")),
            "postgres://u:p@h:5433/n"
        );
    }

    #[test]
    fn build_database_url_without_credentials() {
        assert_eq!(
            build_database_url("localhost", 5432, "mydb", None, None),
            "postgres://localhost:5432/mydb"
        );
    }

    #[test]
    fn build_database_url_partial_credentials_are_omitted() {
        // Username without password (or vice versa) yields a credential-less URL.
        assert_eq!(
            build_database_url("h", 5432, "n", Some("u"), None),
            "postgres://h:5432/n"
        );
        assert_eq!(
            build_database_url("h", 5432, "n", None, Some("p")),
            "postgres://h:5432/n"
        );
    }

    #[test]
    fn provider_names_default_is_coingecko() {
        // Guard: only test when env var is absent
        if std::env::var("PROVIDERS").is_err() {
            let names = provider_names();
            assert_eq!(names, vec!["coingecko"]);
        }
    }

    #[test]
    fn coingecko_tier_default_is_demo() {
        if std::env::var("COINGECKO_TIER").is_err() {
            assert_eq!(coingecko_tier(), "demo");
        }
    }

    #[test]
    fn coingecko_base_url_demo_default() {
        if std::env::var("COINGECKO_TIER").is_err() && std::env::var("COINGECKO_BASE_URL").is_err()
        {
            assert_eq!(coingecko_base_url(), "https://api.coingecko.com");
        }
    }

    #[test]
    fn pacer_cooldown_ms_default_is_60s() {
        let key = "PACER_TESTPROVIDER_COOLDOWN_MS";
        if std::env::var(key).is_err() {
            assert_eq!(pacer_cooldown_ms("testprovider"), 60_000);
        }
    }

    // ── SPEC-SCHED-001 scheduling knob defaults (OR-SCHED-1) ─────────────────

    #[test]
    fn live_quote_poll_interval_secs_default() {
        if std::env::var("LIVE_QUOTE_POLL_INTERVAL_SECS").is_err() {
            assert_eq!(live_quote_poll_interval_secs(), 60);
        }
    }

    #[test]
    fn live_poll_min_interval_secs_default() {
        if std::env::var("LIVE_POLL_MIN_INTERVAL_SECS").is_err() {
            assert_eq!(live_poll_min_interval_secs(), 5);
        }
    }

    #[test]
    fn live_poll_max_interval_secs_default() {
        if std::env::var("LIVE_POLL_MAX_INTERVAL_SECS").is_err() {
            assert_eq!(live_poll_max_interval_secs(), 3600);
        }
    }

    #[test]
    fn live_poll_claim_ttl_secs_default() {
        if std::env::var("LIVE_POLL_CLAIM_TTL_SECS").is_err() {
            assert_eq!(live_poll_claim_ttl_secs(), 120);
        }
    }

    #[test]
    fn collection_lease_secs_default() {
        if std::env::var("COLLECTION_LEASE_SECONDS").is_err() {
            assert_eq!(collection_lease_secs(), 120);
        }
    }

    #[test]
    fn collection_max_attempts_default() {
        if std::env::var("COLLECTION_MAX_ATTEMPTS").is_err() {
            assert_eq!(collection_max_attempts(), 5);
        }
    }

    #[test]
    fn backfill_lease_secs_default() {
        if std::env::var("BACKFILL_LEASE_SECONDS").is_err() {
            assert_eq!(backfill_lease_secs(), 300);
        }
    }

    #[test]
    fn backfill_max_attempts_default() {
        if std::env::var("BACKFILL_MAX_ATTEMPTS").is_err() {
            assert_eq!(backfill_max_attempts(), 5);
        }
    }

    #[test]
    fn metadata_refresh_interval_default() {
        if std::env::var("METADATA_REFRESH_INTERVAL_SECS").is_err() {
            assert_eq!(metadata_refresh_interval_secs(), 86_400);
        }
    }

    #[test]
    fn replica_id_is_stable_within_process() {
        // Calling replica_id() twice returns the same value.
        let id1 = replica_id();
        let id2 = replica_id();
        assert_eq!(id1, id2);
        assert!(!id1.is_empty());
    }

    // ── SPEC-OBS-001 port + observability config defaults ──────────────────────

    #[test]
    fn api_port_default_is_8080() {
        if std::env::var("API_PORT").is_err() {
            assert_eq!(api_port(), 8080);
        }
    }

    #[test]
    fn health_port_default_is_8081() {
        if std::env::var("HEALTH_PORT").is_err() {
            assert_eq!(health_port(), 8081);
        }
    }

    #[test]
    fn metrics_port_default_is_9000() {
        if std::env::var("METRICS_PORT").is_err() {
            assert_eq!(metrics_port(), 9000);
        }
    }

    #[test]
    fn rust_log_default_is_info() {
        if std::env::var("RUST_LOG").is_err() {
            assert_eq!(rust_log(), "info");
        }
    }

    #[test]
    fn otel_endpoint_absent_when_unset() {
        if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_err() {
            assert!(otel_exporter_otlp_endpoint().is_none());
        }
    }

    #[test]
    fn deployment_environment_default_is_unknown() {
        if std::env::var("DEPLOYMENT_ENVIRONMENT").is_err() {
            assert_eq!(deployment_environment(), "unknown");
        }
    }

    #[test]
    fn shutdown_grace_seconds_default_is_15() {
        if std::env::var("SHUTDOWN_GRACE_SECONDS").is_err() {
            assert_eq!(shutdown_grace_seconds(), 15);
        }
    }

    #[test]
    fn shutdown_drain_seconds_default_is_30() {
        if std::env::var("SHUTDOWN_DRAIN_SECONDS").is_err() {
            assert_eq!(shutdown_drain_seconds(), 30);
        }
    }

    #[test]
    fn tracked_gauge_interval_secs_default_is_30() {
        if std::env::var("TRACKED_GAUGE_INTERVAL_SECS").is_err() {
            assert_eq!(tracked_gauge_interval_secs(), 30);
        }
    }
}
