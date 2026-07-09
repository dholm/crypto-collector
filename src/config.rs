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

/// Fleet-wide cooldown duration in milliseconds between consecutive requests to a provider.
///
/// Env var: `PACER_{PROVIDER}_COOLDOWN_MS` (e.g. `PACER_BINANCE_COOLDOWN_MS`).
/// Default: 500 ms. Override per-provider for stricter APIs (e.g. CoinGecko demo = 60 000 ms).
pub fn pacer_cooldown_ms(provider: &str) -> u64 {
    let key = format!("PACER_{}_COOLDOWN_MS", provider.to_uppercase());
    std::env::var(&key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500)
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

/// Resolve the effective candle collection interval in seconds for a single coin.
///
/// Priority: per-coin `live_poll_interval` (PG INTERVAL as TEXT) → global default.
///
/// `live_poll_interval` is returned from the DB as `live_poll_interval::TEXT`, which
/// PostgreSQL formats as `"HH:MM:SS"` for sub-day intervals. This function parses that
/// back to seconds and falls back to `global_secs` when the field is absent or unparseable.
pub fn effective_candle_interval_secs(live_poll_interval: Option<&str>, global_secs: i64) -> i64 {
    live_poll_interval
        .and_then(crate::api::poll_interval::pg_interval_to_secs)
        .filter(|&v| v > 0)
        .unwrap_or(global_secs)
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

/// Market + metadata periodic-refresh cadence in seconds (OR-SCHED-3 resolved).
///
/// Env var: `METADATA_REFRESH_INTERVAL_SECS`. Default: 3 600 s (1 h).
pub fn metadata_refresh_interval_secs() -> u64 {
    parse_env_u64("METADATA_REFRESH_INTERVAL_SECS", 3_600)
}

/// Historical lookback window (in days) for the once-per-coin startup backfill
/// (`collectors::backfill::enqueue_startup_backfills`).
///
/// Env var: `BACKFILL_LOOKBACK_DAYS`. Default: 3 650 days (~10 years).
pub fn backfill_lookback_days() -> u32 {
    parse_env_u32("BACKFILL_LOOKBACK_DAYS", 3_650)
}

/// Coins to deep-backfill: daily history older than the regular lookback window, sourced
/// from a deep-history provider (Bitstamp — BTC/USD daily back to 2011-08-18)
/// via `collectors::backfill::enqueue_deep_history_backfills`.
///
/// Env var: `DEEP_BACKFILL_COINS` (comma-separated). Default: `"bitcoin"` (the halving
/// cycle-overlay coin, SPEC-CYCLE-001). Empty disables the deep-history backfill.
/// Include only coins the deep-history source actually lists (Bitstamp fiat pairs).
pub fn deep_backfill_coins() -> Vec<String> {
    std::env::var("DEEP_BACKFILL_COINS")
        .unwrap_or_else(|_| "bitcoin".to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Earliest date for the deep-history daily backfill window.
///
/// Env var: `DEEP_BACKFILL_START_DATE` (`YYYY-MM-DD`). Default: `2011-08-18`, the first
/// day Bitstamp serves BTC/USD daily candles.
pub fn deep_backfill_start_date() -> chrono::NaiveDate {
    std::env::var("DEEP_BACKFILL_START_DATE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            chrono::NaiveDate::from_ymd_opt(2011, 8, 18).expect("valid default deep-backfill date")
        })
}

/// Date where native (exchange) daily history begins — the deep-history window's upper
/// bound. Default: `2017-08-17`, Binance's first BTC/USDT candle.
///
/// Env var: `DEEP_BACKFILL_END_DATE` (`YYYY-MM-DD`). The deep window is
/// `[DEEP_BACKFILL_START_DATE, this_date)` and must stop before the primary exchange's
/// first candle: the range chain hands each page's `end` to Binance first, and if that
/// `end` reaches Binance's listing candle, Binance returns it (non-empty) and
/// short-circuits the chain before the deep-history source — so Bitstamp is never
/// reached and the pre-2017 years stay empty. `main.rs` ends the window one second
/// before this date's midnight so Binance's `endTime` stays strictly before its listing
/// candle (Binance returns empty → Bitstamp fills) while the window still covers the
/// prior day. The daily series is then contiguous: deep source below this date, native
/// data from it onward.
pub fn deep_backfill_end_date() -> chrono::NaiveDate {
    std::env::var("DEEP_BACKFILL_END_DATE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            chrono::NaiveDate::from_ymd_opt(2017, 8, 17).expect("valid default deep-backfill end")
        })
}

// ── SPEC-CYCLE-001 halving-cycle overlay configuration (REQ-CYCLE-043) ────────

/// Target coin for the Bitcoin halving-cycle overlay.
///
/// Env var: `CYCLE_OVERLAY_COIN_ID`. Default: `"bitcoin"`.
pub fn cycle_overlay_coin_id() -> String {
    std::env::var("CYCLE_OVERLAY_COIN_ID").unwrap_or_else(|_| "bitcoin".to_string())
}

/// Quote currency for the halving-cycle overlay's daily price basis.
///
/// Env var: `CYCLE_OVERLAY_VS_CURRENCY`. Default: `"usd"`.
pub fn cycle_overlay_vs_currency() -> String {
    std::env::var("CYCLE_OVERLAY_VS_CURRENCY")
        .unwrap_or_else(|_| "usd".to_string())
        .to_lowercase()
}

// ── SPEC-ALARM-001 Alarm Center integration configuration ─────────────────────

/// Alarm Center base URL — the feature gate (REQ-ALARM-001/002/050, D5).
///
/// Env var: `ALARM_CENTER_URL`. Unset or empty = the whole alarm feature is disabled:
/// no `AlarmClient` is built, no reconciler is spawned, no request is ever sent.
pub fn alarm_center_url() -> Option<String> {
    std::env::var("ALARM_CENTER_URL")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Alarm Center API key, attached best-effort as an auth header (REQ-ALARM-009, OR-ALARM-6).
///
/// Env var: `ALARM_CENTER_API_KEY`. Unset = no auth header is sent.
pub fn alarm_center_api_key() -> Option<String> {
    std::env::var("ALARM_CENTER_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Per-attempt timeout for alarm-center HTTP requests, in milliseconds (REQ-ALARM-006).
///
/// Env var: `ALARM_CENTER_TIMEOUT_MS`. Default: 5000 ms.
pub fn alarm_center_timeout_ms() -> u64 {
    parse_env_u64("ALARM_CENTER_TIMEOUT_MS", 5_000)
}

/// Maximum retry attempts for a raise/clear before the client swallows the error and logs
/// (REQ-ALARM-006/007).
///
/// Env var: `ALARM_CENTER_MAX_RETRIES`. Default: 3.
pub fn alarm_center_max_retries() -> u32 {
    parse_env_u32("ALARM_CENTER_MAX_RETRIES", 3)
}

/// Reconciler sweep cadence in seconds (REQ-ALARM-011, D2).
///
/// Env var: `ALARM_RECONCILE_INTERVAL_SECS`. Default: 30 s (aligned with
/// `TRACKED_GAUGE_INTERVAL_SECS`).
pub fn alarm_reconcile_interval_secs() -> u64 {
    parse_env_u64("ALARM_RECONCILE_INTERVAL_SECS", 30)
}

/// Server-side auto-clear TTL sent as `timeoutSeconds` on every raise/heartbeat
/// (REQ-ALARM-050/052/053).
///
/// Env var: `ALARM_TTL_SECS`. Default: `ceil(2.5 * alarm_reconcile_interval_secs())`
/// (≈75 s at the 30 s default interval), sized to exceed the reconcile interval by a
/// safety margin so a single slow/missed sweep cannot let an active alarm's deadline lapse.
pub fn alarm_ttl_secs() -> u64 {
    std::env::var("ALARM_TTL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            let interval = alarm_reconcile_interval_secs();
            // ceil(2.5 * interval) computed in integer arithmetic: (5 * interval + 1) / 2.
            (5 * interval).div_ceil(2)
        })
}

/// Sustained-outage threshold before a single unreachable provider's Warning alarm activates
/// (REQ-ALARM-020/051).
///
/// Env var: `ALARM_PROVIDER_UNREACHABLE_SECS`. Default: 300 s.
pub fn alarm_provider_unreachable_secs() -> u64 {
    parse_env_u64("ALARM_PROVIDER_UNREACHABLE_SECS", 300)
}

/// Sustained readiness-ping failure threshold before the DB-unreachable Critical alarm
/// activates (REQ-ALARM-030/051).
///
/// Env var: `ALARM_DB_UNREACHABLE_SECS`. Default: 60 s.
pub fn alarm_db_unreachable_secs() -> u64 {
    parse_env_u64("ALARM_DB_UNREACHABLE_SECS", 60)
}

/// Windowed collection-queue failure count threshold (REQ-ALARM-032/051).
///
/// Env var: `ALARM_QUEUE_FAILED_THRESHOLD`. Default: 10.
pub fn alarm_queue_failed_threshold() -> u32 {
    parse_env_u32("ALARM_QUEUE_FAILED_THRESHOLD", 10)
}

/// Window (in seconds) bounding the collection-queue failure-rate signal so the terminal
/// `'failed'` status does not latch the alarm permanently (REQ-ALARM-032/051, B1).
///
/// Env var: `ALARM_QUEUE_FAILED_WINDOW_SECS`. Default: 3600 s (1 h).
pub fn alarm_queue_failed_window_secs() -> u64 {
    parse_env_u64("ALARM_QUEUE_FAILED_WINDOW_SECS", 3_600)
}

/// Windowed backfill-chunk failure count threshold (REQ-ALARM-033/051).
///
/// Env var: `ALARM_BACKFILL_FAILED_THRESHOLD`. Default: 10.
pub fn alarm_backfill_failed_threshold() -> u32 {
    parse_env_u32("ALARM_BACKFILL_FAILED_THRESHOLD", 10)
}

/// Window (in seconds) bounding the backfill-chunk failure-rate signal, for the same
/// terminal-state reason as `alarm_queue_failed_window_secs` (REQ-ALARM-033/051, B1).
///
/// Env var: `ALARM_BACKFILL_FAILED_WINDOW_SECS`. Default: 3600 s (1 h).
pub fn alarm_backfill_failed_window_secs() -> u64 {
    parse_env_u64("ALARM_BACKFILL_FAILED_WINDOW_SECS", 3_600)
}

/// Duration (in seconds) pending backfill chunks may sit without progress before the
/// `backfill-stalled` Warning alarm activates (REQ-ALARM-033/051).
///
/// Env var: `ALARM_BACKFILL_STALL_SECS`. Default: 3600 s (1 h).
pub fn alarm_backfill_stall_secs() -> u64 {
    parse_env_u64("ALARM_BACKFILL_STALL_SECS", 3_600)
}

/// Restart-event count threshold within the crash-loop window before the
/// `worker-crash-looping` Error alarm activates (REQ-ALARM-034/051).
///
/// Env var: `ALARM_WORKER_CRASHLOOP_THRESHOLD`. Default: 3.
pub fn alarm_worker_crashloop_threshold() -> u32 {
    parse_env_u32("ALARM_WORKER_CRASHLOOP_THRESHOLD", 3)
}

/// Sliding window (in seconds) over which worker restart events are counted for the
/// crash-loop signal (REQ-ALARM-034/051, M2).
///
/// Env var: `ALARM_WORKER_CRASHLOOP_WINDOW_SECS`. Default: 300 s.
pub fn alarm_worker_crashloop_window_secs() -> u64 {
    parse_env_u64("ALARM_WORKER_CRASHLOOP_WINDOW_SECS", 300)
}

/// Staleness threshold (in seconds) beyond which a tracked coin counts toward the
/// aggregated `coins-stalled` alarm (REQ-ALARM-040/051, OR-ALARM-3).
///
/// Env var: `ALARM_COIN_STALENESS_SECS`. Default: 900 s.
pub fn alarm_coin_staleness_secs() -> u64 {
    parse_env_u64("ALARM_COIN_STALENESS_SECS", 900)
}

/// Number of stale coins required before the aggregated `coins-stalled` Warning alarm
/// activates (REQ-ALARM-040/051, OR-ALARM-3).
///
/// Env var: `ALARM_COINS_STALLED_THRESHOLD`. Default: 5.
pub fn alarm_coins_stalled_threshold() -> u32 {
    parse_env_u32("ALARM_COINS_STALLED_THRESHOLD", 5)
}

/// Sustained pool-saturation duration (in seconds) before the `db-pool-exhausted` Error
/// alarm activates (REQ-ALARM-041/051).
///
/// Env var: `ALARM_DB_POOL_SATURATION_SECS`. Default: 60 s.
pub fn alarm_db_pool_saturation_secs() -> u64 {
    parse_env_u64("ALARM_DB_POOL_SATURATION_SECS", 60)
}

/// Consecutive upsert-failure count before the `db-upsert-failures` Error alarm activates
/// (REQ-ALARM-042/051).
///
/// Env var: `ALARM_UPSERT_FAILURE_STREAK`. Default: 20.
pub fn alarm_upsert_failure_streak() -> u32 {
    parse_env_u32("ALARM_UPSERT_FAILURE_STREAK", 20)
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

fn parse_env_u32(name: &str, default: u32) -> u32 {
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
    fn pacer_cooldown_ms_default_is_500ms() {
        let key = "PACER_TESTPROVIDER_COOLDOWN_MS";
        if std::env::var(key).is_err() {
            assert_eq!(pacer_cooldown_ms("testprovider"), 500);
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
            assert_eq!(metadata_refresh_interval_secs(), 3_600);
        }
    }

    #[test]
    fn backfill_lookback_days_default_is_3650() {
        if std::env::var("BACKFILL_LOOKBACK_DAYS").is_err() {
            assert_eq!(backfill_lookback_days(), 3_650);
        }
    }

    // ── Scenario 13 (REQ-CYCLE-043): cycle-overlay env-var-only defaults ──────

    #[test]
    fn cycle_overlay_coin_id_defaults_to_bitcoin() {
        if std::env::var("CYCLE_OVERLAY_COIN_ID").is_err() {
            assert_eq!(cycle_overlay_coin_id(), "bitcoin");
        }
    }

    #[test]
    fn cycle_overlay_vs_currency_defaults_to_usd() {
        if std::env::var("CYCLE_OVERLAY_VS_CURRENCY").is_err() {
            assert_eq!(cycle_overlay_vs_currency(), "usd");
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

    // ── effective_candle_interval_secs ────────────────────────────────────────

    #[test]
    fn effective_interval_falls_back_to_global_when_none() {
        assert_eq!(effective_candle_interval_secs(None, 60), 60);
        assert_eq!(effective_candle_interval_secs(None, 300), 300);
    }

    #[test]
    fn effective_interval_uses_per_coin_when_set() {
        // PG INTERVAL wire format "00:05:00" = 300 s
        assert_eq!(effective_candle_interval_secs(Some("00:05:00"), 60), 300);
        // Human-readable also accepted
        assert_eq!(effective_candle_interval_secs(Some("1h"), 60), 3_600);
    }

    #[test]
    fn effective_interval_falls_back_on_parse_error() {
        // Unparseable value → fall back to global
        assert_eq!(effective_candle_interval_secs(Some("garbage"), 60), 60);
    }

    // ── SPEC-ALARM-001 alarm-center configuration (Milestone 1) ───────────────

    #[test]
    fn alarm_center_url_none_when_unset_or_empty() {
        if std::env::var("ALARM_CENTER_URL").is_err() {
            assert!(alarm_center_url().is_none());
        }
    }

    #[test]
    fn alarm_center_url_some_when_set() {
        // Guard: does not mutate global env state; only asserts when explicitly present.
        if let Ok(v) = std::env::var("ALARM_CENTER_URL") {
            if !v.is_empty() {
                assert_eq!(alarm_center_url(), Some(v));
            }
        }
    }

    #[test]
    fn alarm_center_timeout_ms_default_is_5000() {
        if std::env::var("ALARM_CENTER_TIMEOUT_MS").is_err() {
            assert_eq!(alarm_center_timeout_ms(), 5_000);
        }
    }

    #[test]
    fn alarm_center_max_retries_default_is_3() {
        if std::env::var("ALARM_CENTER_MAX_RETRIES").is_err() {
            assert_eq!(alarm_center_max_retries(), 3);
        }
    }

    #[test]
    fn alarm_reconcile_interval_secs_default_is_30() {
        if std::env::var("ALARM_RECONCILE_INTERVAL_SECS").is_err() {
            assert_eq!(alarm_reconcile_interval_secs(), 30);
        }
    }

    #[test]
    fn alarm_ttl_secs_default_is_ceil_2_5x_interval_and_exceeds_it() {
        // REQ-ALARM-052: default = ceil(2.5 * interval); must strictly exceed the interval.
        if std::env::var("ALARM_TTL_SECS").is_err()
            && std::env::var("ALARM_RECONCILE_INTERVAL_SECS").is_err()
        {
            let interval = alarm_reconcile_interval_secs();
            let ttl = alarm_ttl_secs();
            assert_eq!(ttl, 75); // ceil(2.5 * 30) = 75
            assert!(ttl > interval);
        }
    }

    #[test]
    fn alarm_ttl_secs_ceil_computation_matches_2_5x_for_arbitrary_interval() {
        // Pure arithmetic check independent of env: ceil(2.5 * interval) via (5*i).div_ceil(2).
        for interval in [1u64, 2, 3, 7, 29, 30, 33, 100] {
            let expected = ((interval as f64) * 2.5).ceil() as u64;
            let computed = (5 * interval).div_ceil(2);
            assert_eq!(computed, expected, "interval={interval}");
            assert!(computed > interval);
        }
    }

    #[test]
    fn alarm_queue_failed_window_secs_default_is_3600() {
        if std::env::var("ALARM_QUEUE_FAILED_WINDOW_SECS").is_err() {
            assert_eq!(alarm_queue_failed_window_secs(), 3_600);
        }
    }

    #[test]
    fn alarm_backfill_failed_window_secs_default_is_3600() {
        if std::env::var("ALARM_BACKFILL_FAILED_WINDOW_SECS").is_err() {
            assert_eq!(alarm_backfill_failed_window_secs(), 3_600);
        }
    }

    #[test]
    fn alarm_threshold_defaults() {
        if std::env::var("ALARM_PROVIDER_UNREACHABLE_SECS").is_err() {
            assert_eq!(alarm_provider_unreachable_secs(), 300);
        }
        if std::env::var("ALARM_DB_UNREACHABLE_SECS").is_err() {
            assert_eq!(alarm_db_unreachable_secs(), 60);
        }
        if std::env::var("ALARM_QUEUE_FAILED_THRESHOLD").is_err() {
            assert_eq!(alarm_queue_failed_threshold(), 10);
        }
        if std::env::var("ALARM_BACKFILL_FAILED_THRESHOLD").is_err() {
            assert_eq!(alarm_backfill_failed_threshold(), 10);
        }
        if std::env::var("ALARM_BACKFILL_STALL_SECS").is_err() {
            assert_eq!(alarm_backfill_stall_secs(), 3_600);
        }
        if std::env::var("ALARM_WORKER_CRASHLOOP_THRESHOLD").is_err() {
            assert_eq!(alarm_worker_crashloop_threshold(), 3);
        }
        if std::env::var("ALARM_WORKER_CRASHLOOP_WINDOW_SECS").is_err() {
            assert_eq!(alarm_worker_crashloop_window_secs(), 300);
        }
        if std::env::var("ALARM_COIN_STALENESS_SECS").is_err() {
            assert_eq!(alarm_coin_staleness_secs(), 900);
        }
        if std::env::var("ALARM_COINS_STALLED_THRESHOLD").is_err() {
            assert_eq!(alarm_coins_stalled_threshold(), 5);
        }
        if std::env::var("ALARM_DB_POOL_SATURATION_SECS").is_err() {
            assert_eq!(alarm_db_pool_saturation_secs(), 60);
        }
        if std::env::var("ALARM_UPSERT_FAILURE_STREAK").is_err() {
            assert_eq!(alarm_upsert_failure_streak(), 20);
        }
    }
}
