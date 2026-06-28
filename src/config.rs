//! Configuration loading from environment variables (SPEC-PROV-001).
//!
//! All provider configuration is env-var-only — no hardcoded secrets, no config files.
//! Mirrors SPEC-DB-001's env-var-only approach for DATABASE_URL.

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
