//! CoinGecko hand-rolled `reqwest` client and `Provider` implementation (SPEC-PROV-001).
//!
//! Covers all three data domains (spot/markets, OHLC, coin metadata/tokenomics, derivatives)
//! using the endpoints enumerated in research §2.2.
//!
//! Key design decisions (research §3.2, D2/D4/D6):
//! - Hand-rolled: no CoinGecko SDK crate (none covers Pro+derivatives reliably).
//! - Decimal everywhere: `serde_json::Number::to_string()` → `Decimal` parse (no f64 path).
//! - Dual auth: Demo (`x-cg-demo-api-key`) vs Pro (`x-cg-pro-api-key`) per COINGECKO_TIER.
//! - Pacer: every outbound call acquires `upstream_request_pacer` slot before HTTP.

use super::{
    Capability, CoinMarket, CoinMeta, CoinSearchResult, DerivTick, MarketQuery, MarketSearchResult,
    OhlcCandle, Provider, ProviderError, SpotQuote,
};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;
use sqlx::PgPool;
use std::str::FromStr;
use std::sync::Arc;

use crate::pacer::{self, LocalThrottle};

/// CoinGecko client configuration.
#[derive(Debug, Clone)]
pub struct CoinGeckoConfig {
    /// Base URL (e.g. `https://api.coingecko.com` for Demo).
    pub base_url: String,
    /// API key (optional for Demo, required for Pro).
    pub api_key: Option<String>,
    /// Tier: `"demo"` or `"pro"`.
    pub tier: String,
}

impl CoinGeckoConfig {
    /// Build config from environment variables (production entry point).
    pub fn from_env() -> Self {
        Self {
            base_url: crate::config::coingecko_base_url(),
            api_key: crate::config::coingecko_api_key(),
            tier: crate::config::coingecko_tier(),
        }
    }

    /// API key header name for the configured tier (REQ-PROV-011).
    pub fn key_header_name(&self) -> &'static str {
        if self.tier == "pro" {
            "x-cg-pro-api-key"
        } else {
            "x-cg-demo-api-key"
        }
    }

    /// Whether this tier supports the range-bounded OHLC endpoint `/ohlc/range` (Analyst+).
    pub fn supports_ohlc_range(&self) -> bool {
        matches!(
            self.tier.as_str(),
            "analyst" | "lite" | "enterprise" | "pro"
        )
    }
}

// ── Wire-format deserialization helpers ──────────────────────────────────────

/// Parse a `serde_json::Number` into `Decimal` using the exact string representation.
///
/// With `serde_json/arbitrary_precision`, `Number::to_string()` preserves the original
/// JSON string (e.g. `"0.00000000001234"`), giving exact `Decimal` parse (REQ-PROV-012).
/// This path never goes through `f64` and cannot lose precision.
fn decimal_from_number(n: &serde_json::Number) -> Result<Decimal, ProviderError> {
    let s = n.to_string();
    Decimal::from_str(&s).map_err(|e| ProviderError::Parse(format!("Decimal parse '{s}': {e}")))
}

/// Parse epoch milliseconds to `DateTime<Utc>` (Scenario 14, REQ-PROV-032).
pub fn ts_from_ms(ms: i64) -> Result<DateTime<Utc>, ProviderError> {
    DateTime::from_timestamp_millis(ms)
        .ok_or_else(|| ProviderError::Parse(format!("invalid epoch ms: {ms}")))
}

/// Parse epoch seconds to `DateTime<Utc>` (Scenario 14, REQ-PROV-032).
pub fn ts_from_secs(secs: i64) -> Result<DateTime<Utc>, ProviderError> {
    DateTime::from_timestamp(secs, 0)
        .ok_or_else(|| ProviderError::Parse(format!("invalid epoch secs: {secs}")))
}

// ── Wire format DTOs ──────────────────────────────────────────────────────────

/// Wire format for one item from `/coins/markets`.
#[derive(Debug, Deserialize)]
struct CgMarketItem {
    id: String,
    vs_currency: Option<String>,
    current_price: Option<serde_json::Number>,
    market_cap: Option<serde_json::Number>,
    fully_diluted_valuation: Option<serde_json::Number>,
    circulating_supply: Option<serde_json::Number>,
    total_supply: Option<serde_json::Number>,
    total_volume: Option<serde_json::Number>,
    last_updated: Option<String>,
}

/// Wire format for `/coins/{id}` (coin detail).
#[derive(Debug, Deserialize)]
struct CgCoinDetail {
    id: String,
    symbol: String,
    name: String,
    categories: Option<Vec<String>>,
    description: Option<CgDescription>,
    links: Option<Value>,
    platforms: Option<Value>,
    market_data: Option<CgMarketData>,
    genesis_date: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CgDescription {
    en: Option<String>,
}

/// Subset of `/coins/{id}` market_data used for coin metadata (max_supply only).
/// Full market aggregates come from the dedicated `/coins/markets` endpoint.
#[derive(Debug, Deserialize)]
struct CgMarketData {
    max_supply: Option<serde_json::Number>,
}

/// Wire format for one item from `/derivatives/tickers`.
#[derive(Debug, Deserialize)]
struct CgDerivTicker {
    market: Option<String>,
    symbol: Option<String>,
    price: Option<serde_json::Number>,
    contract_type: Option<String>,
    index: Option<serde_json::Number>,
    basis: Option<serde_json::Number>,
    funding_rate: Option<serde_json::Number>,
    open_interest: Option<serde_json::Number>,
    volume_24h: Option<serde_json::Number>,
    last_traded_at: Option<i64>,
}

// ── CoinGecko HTTP client ─────────────────────────────────────────────────────

/// Thin `reqwest` client for CoinGecko V3 API endpoints.
///
/// Handles auth header injection and response parsing. No pacer logic here —
/// pacer calls live in `CoinGeckoProvider`.
pub struct CoinGeckoClient {
    client: reqwest::Client,
    config: CoinGeckoConfig,
}

impl CoinGeckoClient {
    pub fn new(config: CoinGeckoConfig) -> Self {
        let client = reqwest::Client::builder()
            .gzip(true)
            .build()
            .expect("reqwest client");
        Self { client, config }
    }

    /// Build a GET request with the correct API key header for the configured tier (REQ-PROV-011).
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.config.base_url, path);
        let mut req = self.client.get(&url);
        if let Some(key) = &self.config.api_key {
            req = req.header(self.config.key_header_name(), key.as_str());
        }
        req
    }

    /// The key header name this client would use (exposed for tier-switching tests, Scenario 5).
    pub fn key_header_name(&self) -> &'static str {
        self.config.key_header_name()
    }

    /// The configured base URL (exposed for tier-switching tests, Scenario 5).
    pub fn base_url(&self) -> &str {
        &self.config.base_url
    }

    /// True if this client targets the Demo base URL.
    pub fn is_demo(&self) -> bool {
        self.config.tier == "demo"
    }

    // ── Endpoint methods ───────────────────────────────────────────────────────

    /// `GET /coins/markets` — spot price + market aggregates for a list of coin IDs.
    ///
    /// Returns normalised `CoinMarket` items (one per coin).
    pub async fn fetch_markets(
        &self,
        coin_ids: &[&str],
        vs_currency: &str,
    ) -> Result<Vec<CoinMarket>, ProviderError> {
        let ids = coin_ids.join(",");
        let resp = self
            .get("/api/v3/coins/markets")
            .query(&[
                ("vs_currency", vs_currency),
                ("ids", &ids),
                ("price_change_percentage", ""),
            ])
            .send()
            .await?;

        let status = resp.status().as_u16();
        if status == 429 {
            return Err(ProviderError::RateLimited);
        }
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Http { status, body });
        }

        let items: Vec<CgMarketItem> = resp
            .json()
            .await
            .map_err(|e| ProviderError::Parse(format!("markets parse error: {e}")))?;

        items
            .into_iter()
            .map(|item| normalise_market_item(item, vs_currency))
            .collect()
    }

    /// `GET /coins/{id}/ohlc?vs_currency={vs}&days={days}` — day-bucketed OHLC.
    ///
    /// Returns candles with `volume = None` and `source = "coingecko"` (REQ-PROV-013/031).
    pub async fn fetch_ohlc(
        &self,
        coin_id: &str,
        vs_currency: &str,
        days: u32,
        market_id: i64,
    ) -> Result<Vec<OhlcCandle>, ProviderError> {
        let days_str = days.to_string();
        let resp = self
            .get(&format!("/api/v3/coins/{coin_id}/ohlc"))
            .query(&[("vs_currency", vs_currency), ("days", &days_str)])
            .send()
            .await?;

        let status = resp.status().as_u16();
        if status == 429 {
            return Err(ProviderError::RateLimited);
        }
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Http { status, body });
        }

        let raw: Vec<Value> = resp
            .json()
            .await
            .map_err(|e| ProviderError::Parse(format!("ohlc parse error: {e}")))?;

        raw.iter()
            .map(|v| normalise_ohlc_item(v, market_id, vs_currency))
            .collect()
    }

    /// `GET /coins/{id}` — full coin detail (metadata + market data).
    async fn fetch_coin_detail(&self, coin_id: &str) -> Result<CgCoinDetail, ProviderError> {
        let resp = self
            .get(&format!("/api/v3/coins/{coin_id}"))
            .query(&[
                ("localization", "false"),
                ("tickers", "false"),
                ("market_data", "true"),
                ("community_data", "false"),
                ("developer_data", "false"),
            ])
            .send()
            .await?;

        let status = resp.status().as_u16();
        if status == 429 {
            return Err(ProviderError::RateLimited);
        }
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Http { status, body });
        }

        resp.json::<CgCoinDetail>()
            .await
            .map_err(|e| ProviderError::Parse(format!("coin detail parse error: {e}")))
    }

    /// `GET /api/v3/search` — search coins by name / symbol (SPEC-PROV-001 REQ-PROV-005).
    ///
    /// Uses the authenticated `get()` helper so the correct tier key header is attached.
    /// Returns up to `cap` results. Empty `q` returns `Ok(vec![])` immediately.
    ///
    /// On upstream non-success the call degrades to `Ok(vec![])` (REQ-PROV-005) and
    /// emits a WARN log carrying the HTTP status, query string, and a 512-char body preview
    /// so operators can distinguish rate-limit / auth failures from a genuinely empty result.
    pub async fn search_coins(
        &self,
        q: &str,
        cap: usize,
    ) -> Result<Vec<CoinSearchResult>, ProviderError> {
        if q.is_empty() {
            return Ok(vec![]);
        }

        let resp = self
            .get("/api/v3/search")
            .query(&[("query", q)])
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            // Non-fatal: degrade to empty on upstream errors (REQ-PROV-005).
            // Log at WARN so operators can distinguish rate-limit / auth failures
            // from a genuinely empty result set.
            let body_text = resp.text().await.unwrap_or_default();
            let body_preview: String = body_text.chars().take(512).collect();
            tracing::warn!(
                http.status = %status,
                q = q,
                upstream_body = %body_preview,
                "upstream CoinGecko /search returned non-success; degrading to empty result"
            );
            return Ok(vec![]);
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Parse(format!("search parse error: {e}")))?;

        let coins = body["coins"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .take(cap)
            .filter_map(|c| {
                Some(CoinSearchResult {
                    coin_id: c["id"].as_str()?.to_string(),
                    symbol: c["symbol"].as_str()?.to_string(),
                    name: c["name"].as_str()?.to_string(),
                })
            })
            .collect();

        Ok(coins)
    }

    /// `GET /api/v3/search` — search market pairs (exchanges) by query string (SPEC-PROV-001 REQ-PROV-005).
    ///
    /// Uses the authenticated `get()` helper so the correct tier key header is attached.
    /// Parses the `exchanges[]` array: base / target / market.identifier → `MarketSearchResult`.
    /// Returns up to `cap` results. Empty `q` returns `Ok(vec![])` immediately.
    ///
    /// On upstream non-success the call degrades to `Ok(vec![])` (REQ-PROV-005) and
    /// emits a WARN log carrying the HTTP status, query string, and a 512-char body preview
    /// so operators can distinguish rate-limit / auth failures from a genuinely empty result.
    pub async fn search_markets(
        &self,
        q: &str,
        cap: usize,
    ) -> Result<Vec<MarketSearchResult>, ProviderError> {
        if q.is_empty() {
            return Ok(vec![]);
        }

        let resp = self
            .get("/api/v3/search")
            .query(&[("query", q)])
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            // Non-fatal: degrade to empty on upstream errors (REQ-PROV-005).
            let body_text = resp.text().await.unwrap_or_default();
            let body_preview: String = body_text.chars().take(512).collect();
            tracing::warn!(
                http.status = %status,
                q = q,
                upstream_body = %body_preview,
                "upstream CoinGecko /search returned non-success for market search; degrading to empty result"
            );
            return Ok(vec![]);
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Parse(format!("market search parse error: {e}")))?;

        let markets = body["exchanges"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .take(cap)
            .filter_map(|e| {
                Some(MarketSearchResult {
                    base: e["base"].as_str()?.to_string(),
                    quote: e["target"].as_str()?.to_string(),
                    venue: e["market"]["identifier"].as_str().map(|s| s.to_string()),
                })
            })
            .collect();

        Ok(markets)
    }

    /// `GET /derivatives/tickers` — all derivatives tickers.
    ///
    /// Returns all tickers; caller filters by symbol/venue.
    async fn fetch_derivatives_tickers(&self) -> Result<Vec<CgDerivTicker>, ProviderError> {
        let resp = self.get("/api/v3/derivatives/tickers").send().await?;

        let status = resp.status().as_u16();
        if status == 429 {
            return Err(ProviderError::RateLimited);
        }
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Http { status, body });
        }

        resp.json::<Vec<CgDerivTicker>>()
            .await
            .map_err(|e| ProviderError::Parse(format!("derivatives tickers parse error: {e}")))
    }
}

// ── Normalisation functions (pure, testable without HTTP) ─────────────────────

/// Normalise a `/coins/markets` wire item into `CoinMarket`.
///
// @MX:NOTE: [AUTO] volume = None is intentional for CoinGecko OHLC; see normalise_ohlc_item.
fn normalise_market_item(
    item: CgMarketItem,
    vs_currency: &str,
) -> Result<CoinMarket, ProviderError> {
    let price = item
        .current_price
        .as_ref()
        .ok_or_else(|| ProviderError::Parse("current_price missing".to_string()))
        .and_then(decimal_from_number)?;

    let ts = match &item.last_updated {
        Some(s) => DateTime::parse_from_rfc3339(s)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
        None => Utc::now(),
    };

    Ok(CoinMarket {
        coin_id: item.id,
        vs_currency: item.vs_currency.unwrap_or_else(|| vs_currency.to_string()),
        ts,
        price,
        market_cap: item
            .market_cap
            .as_ref()
            .map(decimal_from_number)
            .transpose()?,
        fully_diluted_valuation: item
            .fully_diluted_valuation
            .as_ref()
            .map(decimal_from_number)
            .transpose()?,
        circulating_supply: item
            .circulating_supply
            .as_ref()
            .map(decimal_from_number)
            .transpose()?,
        total_supply: item
            .total_supply
            .as_ref()
            .map(decimal_from_number)
            .transpose()?,
        volume_24h: item
            .total_volume
            .as_ref()
            .map(decimal_from_number)
            .transpose()?,
        source: "coingecko".to_string(),
    })
}

/// Normalise one OHLC array `[ts_ms, open, high, low, close]` into `OhlcCandle`.
///
/// CoinGecko OHLC has **no volume** → `volume = None`, `source = "coingecko"` (REQ-PROV-013/031).
///
// @MX:NOTE: [AUTO] volume intentionally None — CoinGecko /coins/{id}/ohlc returns [ts,O,H,L,C] with no volume field.
// @MX:SPEC: SPEC-PROV-001 REQ-PROV-013/031 research §2.2
pub fn normalise_ohlc_item(
    v: &Value,
    market_id: i64,
    vs_currency: &str,
) -> Result<OhlcCandle, ProviderError> {
    let arr = v
        .as_array()
        .ok_or_else(|| ProviderError::Parse("OHLC item must be array".to_string()))?;

    if arr.len() < 5 {
        return Err(ProviderError::Parse(format!(
            "OHLC item must have 5 elements, got {}",
            arr.len()
        )));
    }

    let ts_ms = arr[0]
        .as_i64()
        .ok_or_else(|| ProviderError::Parse("OHLC timestamp must be integer".to_string()))?;
    let ts = ts_from_ms(ts_ms)?;

    let open = parse_ohlc_field(&arr[1], "open")?;
    let high = parse_ohlc_field(&arr[2], "high")?;
    let low = parse_ohlc_field(&arr[3], "low")?;
    let close = parse_ohlc_field(&arr[4], "close")?;

    Ok(OhlcCandle {
        market_id,
        // CoinGecko auto granularity: 30m (1-2d), 4h (3-30d), 4d (31d+).
        // We use "auto" as the interval label to indicate CoinGecko's auto-bucket.
        interval: "auto".to_string(),
        ts,
        open,
        high,
        low,
        close,
        // CoinGecko OHLC has no volume — explicitly None, never 0 (REQ-PROV-013)
        volume: None,
        vs_currency: vs_currency.to_string(),
        source: "coingecko".to_string(),
    })
}

fn parse_ohlc_field(v: &Value, name: &str) -> Result<Decimal, ProviderError> {
    match v {
        Value::Number(n) => decimal_from_number(n),
        Value::String(s) => Decimal::from_str(s)
            .map_err(|e| ProviderError::Parse(format!("ohlc {name} parse '{s}': {e}"))),
        _ => Err(ProviderError::Parse(format!(
            "ohlc {name} must be number, got {v:?}"
        ))),
    }
}

/// Normalise `CgCoinDetail` into `CoinMeta`.
fn normalise_coin_detail(detail: CgCoinDetail) -> CoinMeta {
    let description = detail
        .description
        .and_then(|d| d.en)
        .filter(|s| !s.is_empty());

    let homepage = detail
        .links
        .as_ref()
        .and_then(|l| l.get("homepage"))
        .and_then(|h| h.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);

    let genesis_date = detail
        .genesis_date
        .as_deref()
        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());

    let max_supply = detail
        .market_data
        .as_ref()
        .and_then(|md| md.max_supply.as_ref())
        .and_then(|n| decimal_from_number(n).ok());

    CoinMeta {
        coin_id: detail.id,
        name: detail.name,
        symbol: detail.symbol,
        categories: detail.categories.filter(|v| !v.is_empty()),
        description,
        homepage,
        links: detail.links,
        contract_addresses: detail.platforms,
        max_supply,
        genesis_date,
    }
}

/// Normalise `CgDerivTicker` into `DerivTick`.
fn normalise_deriv_ticker(
    ticker: &CgDerivTicker,
    market_id: i64,
) -> Result<DerivTick, ProviderError> {
    let ts = ticker
        .last_traded_at
        .map(ts_from_secs)
        .transpose()?
        .unwrap_or_else(Utc::now);

    let funding_rate = ticker
        .funding_rate
        .as_ref()
        .map(decimal_from_number)
        .transpose()?;
    let open_interest = ticker
        .open_interest
        .as_ref()
        .map(decimal_from_number)
        .transpose()?;
    let mark_price = ticker.price.as_ref().map(decimal_from_number).transpose()?;
    let index_price = ticker.index.as_ref().map(decimal_from_number).transpose()?;
    let basis = ticker.basis.as_ref().map(decimal_from_number).transpose()?;
    let volume_24h = ticker
        .volume_24h
        .as_ref()
        .map(decimal_from_number)
        .transpose()?;

    Ok(DerivTick {
        market_id,
        ts,
        funding_rate,
        open_interest,
        open_interest_usd: None, // CoinGecko /derivatives/tickers doesn't separate OI in USD
        mark_price,
        index_price,
        basis,
        volume_24h,
        contract_type: ticker.contract_type.clone(),
        venue: ticker.market.clone(),
        source: "coingecko".to_string(),
    })
}

// ── CoinGeckoProvider (implements Provider trait) ─────────────────────────────

/// CoinGecko `Provider` implementation.
///
/// Wraps `CoinGeckoClient` with the pacer protocol: every outbound call acquires
/// a slot from `upstream_request_pacer` before HTTP (REQ-PROV-040/045).
pub struct CoinGeckoProvider {
    client: CoinGeckoClient,
    pool: PgPool,
    local_throttle: Arc<LocalThrottle>,
}

impl CoinGeckoProvider {
    pub fn new(config: CoinGeckoConfig, pool: PgPool) -> Self {
        let local_throttle = Arc::new(LocalThrottle::new(0)); // pacer handles primary timing
        Self {
            client: CoinGeckoClient::new(config),
            pool,
            local_throttle,
        }
    }

    /// True if this instance targets the Demo tier.
    #[cfg(test)]
    pub fn is_demo(&self) -> bool {
        self.client.is_demo()
    }
}

#[async_trait]
impl Provider for CoinGeckoProvider {
    fn name(&self) -> &str {
        "coingecko"
    }

    fn supports(&self, cap: Capability) -> bool {
        matches!(
            cap,
            Capability::Spot
                | Capability::Ohlc
                | Capability::CoinMetadata
                | Capability::CoinMarket
                | Capability::Derivatives
        )
    }

    async fn fetch_spot(&self, market: &MarketQuery) -> Result<SpotQuote, ProviderError> {
        let coin_id = market.coin_id.as_deref().ok_or_else(|| {
            ProviderError::Other(anyhow::anyhow!("coin_id required for CoinGecko spot"))
        })?;

        // Pacer: acquire slot before outbound HTTP (REQ-PROV-040)
        self.local_throttle.acquire().await;
        pacer::acquire_slot(&self.pool, "coingecko")
            .await
            .map_err(ProviderError::Pacer)?;

        let markets = match self
            .client
            .fetch_markets(&[coin_id], &market.vs_currency)
            .await
        {
            Ok(m) => m,
            Err(ProviderError::RateLimited) => {
                let cooldown_ms = crate::config::pacer_cooldown_ms("coingecko");
                let _ = pacer::signal_cooldown(&self.pool, "coingecko", cooldown_ms).await;
                return Err(ProviderError::RateLimited);
            }
            Err(e) => return Err(e),
        };

        let cm = markets
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::Parse("no market data returned".to_string()))?;

        Ok(SpotQuote {
            market_id: market.market_id,
            ts: cm.ts,
            price: cm.price,
            bid: None,
            ask: None,
            volume_24h: cm.volume_24h,
            vs_currency: cm.vs_currency,
            source: "coingecko".to_string(),
        })
    }

    async fn fetch_ohlc(
        &self,
        market: &MarketQuery,
        days: u32,
    ) -> Result<Vec<OhlcCandle>, ProviderError> {
        let coin_id = market.coin_id.as_deref().ok_or_else(|| {
            ProviderError::Other(anyhow::anyhow!("coin_id required for CoinGecko OHLC"))
        })?;

        // Tier-limited endpoint degrades (REQ-PROV-014): Demo cannot use /ohlc/range.
        // Use day-bucketed endpoint for all tiers; surface limitation via OhlcCandle interval.
        // (Analyst+ range endpoint is a future enhancement: OR-PROV-2)

        // Pacer: acquire slot before outbound HTTP (REQ-PROV-040)
        self.local_throttle.acquire().await;
        pacer::acquire_slot(&self.pool, "coingecko")
            .await
            .map_err(ProviderError::Pacer)?;

        match self
            .client
            .fetch_ohlc(coin_id, &market.vs_currency, days, market.market_id)
            .await
        {
            Ok(c) => Ok(c),
            Err(ProviderError::RateLimited) => {
                let cooldown_ms = crate::config::pacer_cooldown_ms("coingecko");
                let _ = pacer::signal_cooldown(&self.pool, "coingecko", cooldown_ms).await;
                Err(ProviderError::RateLimited)
            }
            Err(e) => Err(e),
        }
    }

    async fn fetch_coin_metadata(&self, coin_id: &str) -> Result<CoinMeta, ProviderError> {
        self.local_throttle.acquire().await;
        pacer::acquire_slot(&self.pool, "coingecko")
            .await
            .map_err(ProviderError::Pacer)?;

        match self.client.fetch_coin_detail(coin_id).await {
            Ok(detail) => Ok(normalise_coin_detail(detail)),
            Err(ProviderError::RateLimited) => {
                let cooldown_ms = crate::config::pacer_cooldown_ms("coingecko");
                let _ = pacer::signal_cooldown(&self.pool, "coingecko", cooldown_ms).await;
                Err(ProviderError::RateLimited)
            }
            Err(e) => Err(e),
        }
    }

    async fn fetch_coin_market(
        &self,
        coin_id: &str,
        vs_currency: &str,
    ) -> Result<CoinMarket, ProviderError> {
        self.local_throttle.acquire().await;
        pacer::acquire_slot(&self.pool, "coingecko")
            .await
            .map_err(ProviderError::Pacer)?;

        let markets = match self.client.fetch_markets(&[coin_id], vs_currency).await {
            Ok(m) => m,
            Err(ProviderError::RateLimited) => {
                let cooldown_ms = crate::config::pacer_cooldown_ms("coingecko");
                let _ = pacer::signal_cooldown(&self.pool, "coingecko", cooldown_ms).await;
                return Err(ProviderError::RateLimited);
            }
            Err(e) => return Err(e),
        };

        markets
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::Parse(format!("no market data for {coin_id}")))
    }

    async fn fetch_derivatives(&self, market: &MarketQuery) -> Result<DerivTick, ProviderError> {
        self.local_throttle.acquire().await;
        pacer::acquire_slot(&self.pool, "coingecko")
            .await
            .map_err(ProviderError::Pacer)?;

        let tickers = match self.client.fetch_derivatives_tickers().await {
            Ok(t) => t,
            Err(ProviderError::RateLimited) => {
                let cooldown_ms = crate::config::pacer_cooldown_ms("coingecko");
                let _ = pacer::signal_cooldown(&self.pool, "coingecko", cooldown_ms).await;
                return Err(ProviderError::RateLimited);
            }
            Err(e) => return Err(e),
        };

        // Match ticker by base symbol (case-insensitive prefix match)
        let base_upper = market.base.to_uppercase();
        let ticker = tickers
            .iter()
            .find(|t| {
                t.symbol
                    .as_deref()
                    .map(|s| s.to_uppercase().starts_with(&base_upper))
                    .unwrap_or(false)
            })
            .ok_or_else(|| {
                ProviderError::Parse(format!("no derivatives ticker for {}", market.base))
            })?;

        normalise_deriv_ticker(ticker, market.market_id)
    }

    async fn search_coins(
        &self,
        q: &str,
        cap: usize,
    ) -> Result<Vec<CoinSearchResult>, ProviderError> {
        self.client.search_coins(q, cap).await
    }

    async fn search_markets(
        &self,
        q: &str,
        cap: usize,
    ) -> Result<Vec<MarketSearchResult>, ProviderError> {
        self.client.search_markets(q, cap).await
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use serde_json::json;

    // ── Scenario 5 (REQ-PROV-011): Tier switches base URL and key header ──────

    #[test]
    fn demo_tier_uses_demo_url_and_header() {
        let cfg = CoinGeckoConfig {
            base_url: "https://api.coingecko.com".to_string(),
            api_key: None,
            tier: "demo".to_string(),
        };
        let client = CoinGeckoClient::new(cfg);
        assert_eq!(client.base_url(), "https://api.coingecko.com");
        assert_eq!(client.key_header_name(), "x-cg-demo-api-key");
        assert!(client.is_demo());
    }

    #[test]
    fn pro_tier_uses_pro_url_and_header() {
        let cfg = CoinGeckoConfig {
            base_url: "https://pro-api.coingecko.com".to_string(),
            api_key: Some("test-key".to_string()),
            tier: "pro".to_string(),
        };
        let client = CoinGeckoClient::new(cfg);
        assert_eq!(client.base_url(), "https://pro-api.coingecko.com");
        assert_eq!(client.key_header_name(), "x-cg-pro-api-key");
        assert!(!client.is_demo());
    }

    #[test]
    fn config_from_env_demo_defaults() {
        // Env not set → demo defaults
        if std::env::var("COINGECKO_TIER").is_err() {
            let cfg = CoinGeckoConfig::from_env();
            assert_eq!(cfg.tier, "demo");
            assert_eq!(cfg.base_url, "https://api.coingecko.com");
        }
    }

    // ── Scenario 6 (REQ-PROV-012): Numeric precision — tiny price + huge supply ──

    #[test]
    fn markets_normalise_tiny_price_to_exact_decimal() {
        // 0.00000000001234 is 1.234e-11 — f64 cannot represent this exactly
        let json_str = r#"[{
            "id": "shib",
            "current_price": 0.00000000001234,
            "market_cap": null,
            "fully_diluted_valuation": null,
            "circulating_supply": null,
            "total_supply": null,
            "max_supply": null,
            "total_volume": null,
            "last_updated": null
        }]"#;

        let items: Vec<CgMarketItem> = serde_json::from_str(json_str).expect("parse");
        let market =
            normalise_market_item(items.into_iter().next().unwrap(), "usd").expect("normalise");

        // With serde_json/arbitrary_precision, the JSON number "0.00000000001234"
        // is preserved as a string and parsed exactly to Decimal.
        let expected = Decimal::from_str("0.00000000001234").unwrap();
        assert_eq!(
            market.price, expected,
            "tiny price must parse to exact Decimal, no f64 loss"
        );
    }

    #[test]
    fn markets_normalise_huge_supply_to_exact_decimal() {
        // 589000000000000 = 5.89e14 (SHIB-like supply)
        let json_str = r#"[{
            "id": "shib",
            "current_price": 0.00000000001234,
            "market_cap": null,
            "fully_diluted_valuation": null,
            "circulating_supply": 589000000000000,
            "total_supply": 589000000000000,
            "max_supply": null,
            "total_volume": null,
            "last_updated": null
        }]"#;

        let items: Vec<CgMarketItem> = serde_json::from_str(json_str).expect("parse");
        let market =
            normalise_market_item(items.into_iter().next().unwrap(), "usd").expect("normalise");

        let expected = Decimal::from_str("589000000000000").unwrap();
        assert_eq!(
            market.circulating_supply,
            Some(expected),
            "huge supply must parse to exact Decimal"
        );
        assert_eq!(market.total_supply, Some(expected));
    }

    #[test]
    fn markets_normalise_all_optional_fields_as_none_when_null() {
        let json_str = r#"[{
            "id": "testcoin",
            "current_price": 100.0,
            "market_cap": null,
            "fully_diluted_valuation": null,
            "circulating_supply": null,
            "total_supply": null,
            "max_supply": null,
            "total_volume": null,
            "last_updated": null
        }]"#;

        let items: Vec<CgMarketItem> = serde_json::from_str(json_str).expect("parse");
        let market =
            normalise_market_item(items.into_iter().next().unwrap(), "usd").expect("normalise");

        assert!(market.market_cap.is_none());
        assert!(market.fully_diluted_valuation.is_none());
        assert!(market.circulating_supply.is_none());
        assert!(market.total_supply.is_none());
    }

    #[test]
    fn markets_source_is_coingecko() {
        let json_str = r#"[{
            "id": "bitcoin",
            "current_price": 95000.0,
            "market_cap": null,
            "fully_diluted_valuation": null,
            "circulating_supply": null,
            "total_supply": null,
            "max_supply": null,
            "total_volume": null,
            "last_updated": null
        }]"#;

        let items: Vec<CgMarketItem> = serde_json::from_str(json_str).unwrap();
        let market = normalise_market_item(items.into_iter().next().unwrap(), "usd").unwrap();
        assert_eq!(market.source, "coingecko");
    }

    // ── Scenario 7 (REQ-PROV-013/031): OHLC has no volume → None, source tagged ──

    #[test]
    fn ohlc_normalise_produces_volume_none_and_source_coingecko() {
        // CoinGecko returns [timestamp_ms, open, high, low, close] — 5 elements, no volume
        let fixture = json!([
            [1719820000000i64, 94000.0, 96000.0, 93000.0, 95000.0],
            [1719823600000i64, 95000.0, 95500.0, 94500.0, 95200.0]
        ]);

        let arr = fixture.as_array().unwrap();
        let candles: Vec<OhlcCandle> = arr
            .iter()
            .map(|v| normalise_ohlc_item(v, 42, "usd"))
            .collect::<Result<_, _>>()
            .expect("normalise");

        assert_eq!(candles.len(), 2);

        for c in &candles {
            // Volume MUST be None — not 0, not Some(0) (REQ-PROV-013)
            assert!(
                c.volume.is_none(),
                "CoinGecko OHLC must have volume=None, got {:?}",
                c.volume
            );
            // Source MUST be "coingecko"
            assert_eq!(c.source, "coingecko", "source must be 'coingecko'");
            // OHLC values must be Decimal
            assert_eq!(c.market_id, 42);
        }
    }

    #[test]
    fn ohlc_normalise_open_high_low_close_as_decimal() {
        let item = json!([1719820000000i64, 94000.5, 96000.25, 93000.75, 95000.1]);
        let candle = normalise_ohlc_item(&item, 1, "usd").expect("normalise");

        assert_eq!(candle.open, Decimal::from_str("94000.5").unwrap());
        assert_eq!(candle.high, Decimal::from_str("96000.25").unwrap());
        assert_eq!(candle.low, Decimal::from_str("93000.75").unwrap());
        assert_eq!(candle.close, Decimal::from_str("95000.1").unwrap());
    }

    // ── Scenario 8 (REQ-PROV-014): Demo tier degrades, does not error ────────

    #[test]
    fn demo_tier_does_not_support_ohlc_range() {
        let cfg = CoinGeckoConfig {
            base_url: "https://api.coingecko.com".to_string(),
            api_key: None,
            tier: "demo".to_string(),
        };
        assert!(
            !cfg.supports_ohlc_range(),
            "Demo tier must NOT support /ohlc/range (Analyst+ endpoint)"
        );
    }

    #[test]
    fn analyst_tier_supports_ohlc_range() {
        let cfg = CoinGeckoConfig {
            base_url: "https://pro-api.coingecko.com".to_string(),
            api_key: Some("key".to_string()),
            tier: "analyst".to_string(),
        };
        assert!(
            cfg.supports_ohlc_range(),
            "Analyst tier must support /ohlc/range"
        );
    }

    // ── Scenario 14 (REQ-PROV-032): timestamps normalised to UTC ─────────────

    #[test]
    fn ts_from_ms_converts_epoch_millis_to_utc() {
        // 1719820000000 ms = 2024-07-01 05:06:40 UTC
        let ts = ts_from_ms(1_719_820_000_000).expect("ts_from_ms");
        assert_eq!(ts.timestamp(), 1_719_820_000);
        // Verify timezone is UTC
        let expected = DateTime::from_timestamp(1_719_820_000, 0).unwrap();
        assert_eq!(ts, expected);
    }

    #[test]
    fn ts_from_secs_converts_epoch_seconds_to_utc() {
        let ts = ts_from_secs(1_719_820_000).expect("ts_from_secs");
        assert_eq!(ts.timestamp(), 1_719_820_000);
        let expected = DateTime::from_timestamp(1_719_820_000, 0).unwrap();
        assert_eq!(ts, expected);
    }

    #[test]
    fn ohlc_item_timestamp_parsed_to_utc() {
        let item = json!([1719820000000i64, 100.0, 110.0, 90.0, 105.0]);
        let candle = normalise_ohlc_item(&item, 1, "usd").unwrap();
        assert_eq!(candle.ts.timestamp(), 1_719_820_000);
    }

    // ── Derivatives normalisation ─────────────────────────────────────────────

    #[test]
    fn derivatives_ticker_normalises_all_fields() {
        let ticker = CgDerivTicker {
            market: Some("Binance".to_string()),
            symbol: Some("BTC-USDT".to_string()),
            price: Some(serde_json::Number::from_f64(95000.5).unwrap()),
            contract_type: Some("perpetual".to_string()),
            index: Some(serde_json::Number::from_f64(94980.0).unwrap()),
            basis: Some(serde_json::Number::from_f64(20.5).unwrap()),
            funding_rate: Some(serde_json::Number::from_f64(0.0001).unwrap()),
            open_interest: Some(serde_json::Number::from_f64(500_000_000.0).unwrap()),
            volume_24h: Some(serde_json::Number::from_f64(10_000_000_000.0).unwrap()),
            last_traded_at: Some(1_719_820_000),
        };

        let deriv = normalise_deriv_ticker(&ticker, 99).expect("normalise");
        assert_eq!(deriv.market_id, 99);
        assert_eq!(deriv.source, "coingecko");
        assert_eq!(deriv.venue.as_deref(), Some("Binance"));
        assert_eq!(deriv.contract_type.as_deref(), Some("perpetual"));
        assert!(deriv.funding_rate.is_some());
        assert!(deriv.mark_price.is_some());
        assert!(deriv.index_price.is_some());
        assert!(deriv.basis.is_some());
    }

    // ── CoinMeta normalisation ────────────────────────────────────────────────

    #[test]
    fn coin_detail_normalises_to_coin_meta() {
        let detail = CgCoinDetail {
            id: "bitcoin".to_string(),
            symbol: "btc".to_string(),
            name: "Bitcoin".to_string(),
            categories: Some(vec!["Cryptocurrency".to_string()]),
            description: Some(CgDescription {
                en: Some("Peer-to-peer electronic cash".to_string()),
            }),
            links: Some(json!({"homepage": ["https://bitcoin.org"]})),
            platforms: Some(json!({})),
            market_data: Some(CgMarketData {
                max_supply: Some(serde_json::Number::from(21_000_000u64)),
            }),
            genesis_date: Some("2009-01-03".to_string()),
        };

        let meta = normalise_coin_detail(detail);
        assert_eq!(meta.coin_id, "bitcoin");
        assert_eq!(meta.symbol, "btc");
        assert_eq!(meta.max_supply, Some(dec!(21000000)));
        assert_eq!(meta.genesis_date, NaiveDate::from_str("2009-01-03").ok());
        assert_eq!(meta.homepage.as_deref(), Some("https://bitcoin.org"));
    }

    // ── HTTP tests via wiremock (offline, no real network) ────────────────────

    #[tokio::test]
    async fn http_markets_endpoint_parses_response() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        let body = json!([{
            "id": "bitcoin",
            "current_price": 95000.5,
            "market_cap": 1880000000000.0,
            "fully_diluted_valuation": null,
            "circulating_supply": 19700000.0,
            "total_supply": 21000000.0,
            "max_supply": 21000000.0,
            "total_volume": 50000000000.0,
            "last_updated": "2024-07-01T12:00:00.000Z"
        }]);

        Mock::given(method("GET"))
            .and(path("/api/v3/coins/markets"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;

        let cfg = CoinGeckoConfig {
            base_url: server.uri(),
            api_key: None,
            tier: "demo".to_string(),
        };
        let client = CoinGeckoClient::new(cfg);
        let markets = client
            .fetch_markets(&["bitcoin"], "usd")
            .await
            .expect("fetch");

        assert_eq!(markets.len(), 1);
        assert_eq!(markets[0].coin_id, "bitcoin");
        assert_eq!(markets[0].price, Decimal::from_str("95000.5").unwrap());
        assert_eq!(markets[0].source, "coingecko");
    }

    #[tokio::test]
    async fn http_ohlc_endpoint_parses_response() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        let body = json!([
            [1719820000000i64, 94000.0, 96000.0, 93000.0, 95000.0],
            [1719820200000i64, 95000.0, 95500.0, 94500.0, 95200.0]
        ]);

        Mock::given(method("GET"))
            .and(path("/api/v3/coins/bitcoin/ohlc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;

        let cfg = CoinGeckoConfig {
            base_url: server.uri(),
            api_key: None,
            tier: "demo".to_string(),
        };
        let client = CoinGeckoClient::new(cfg);
        let candles = client
            .fetch_ohlc("bitcoin", "usd", 7, 1)
            .await
            .expect("fetch");

        assert_eq!(candles.len(), 2);
        assert!(candles.iter().all(|c| c.volume.is_none()));
        assert!(candles.iter().all(|c| c.source == "coingecko"));
    }

    #[tokio::test]
    async fn http_429_returns_rate_limited_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v3/coins/markets"))
            .respond_with(ResponseTemplate::new(429).set_body_string("Too Many Requests"))
            .mount(&server)
            .await;

        let cfg = CoinGeckoConfig {
            base_url: server.uri(),
            api_key: None,
            tier: "demo".to_string(),
        };
        let client = CoinGeckoClient::new(cfg);
        let result = client.fetch_markets(&["bitcoin"], "usd").await;

        assert!(
            matches!(result, Err(ProviderError::RateLimited)),
            "HTTP 429 must return ProviderError::RateLimited"
        );
    }

    #[tokio::test]
    async fn http_derivatives_tickers_parses_response() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        let body = json!([{
            "market": "Binance",
            "symbol": "BTC-USDT",
            "index_id": "BTC",
            "price": 95000.5,
            "price_percentage_change_24h": 0.5,
            "contract_type": "perpetual",
            "index": 94980.0,
            "basis": 20.5,
            "spread": 0.5,
            "funding_rate": 0.0001,
            "open_interest": 500000000.0,
            "volume_24h": 10000000000.0,
            "last_traded_at": 1719820000,
            "expired_at": null
        }]);

        Mock::given(method("GET"))
            .and(path("/api/v3/derivatives/tickers"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;

        let cfg = CoinGeckoConfig {
            base_url: server.uri(),
            api_key: None,
            tier: "demo".to_string(),
        };
        let client = CoinGeckoClient::new(cfg);
        let tickers = client.fetch_derivatives_tickers().await.expect("fetch");

        assert_eq!(tickers.len(), 1);
        assert_eq!(tickers[0].market.as_deref(), Some("Binance"));
    }

    // Scenario 5: key header appears in request (wiremock request inspection)
    #[tokio::test]
    async fn demo_request_sends_demo_key_header() {
        use wiremock::matchers::{header_exists, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v3/coins/markets"))
            .and(header_exists("x-cg-demo-api-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;

        let cfg = CoinGeckoConfig {
            base_url: server.uri(),
            api_key: Some("test-demo-key".to_string()),
            tier: "demo".to_string(),
        };
        let client = CoinGeckoClient::new(cfg);
        let result = client.fetch_markets(&[], "usd").await;
        // The mock only responds if the header is present — if it fails, header was absent
        assert!(result.is_ok(), "demo key header must be sent");
    }

    #[tokio::test]
    async fn pro_request_sends_pro_key_header() {
        use wiremock::matchers::{header_exists, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v3/coins/markets"))
            .and(header_exists("x-cg-pro-api-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;

        let cfg = CoinGeckoConfig {
            base_url: server.uri(),
            api_key: Some("test-pro-key".to_string()),
            tier: "pro".to_string(),
        };
        let client = CoinGeckoClient::new(cfg);
        let result = client.fetch_markets(&[], "usd").await;
        assert!(result.is_ok(), "pro key header must be sent");
    }

    // ── Scenario 16 (REQ-PROV-005): search_coins sends demo key and parses coins array ──

    #[tokio::test]
    async fn search_coins_sends_demo_key_header_and_parses_response() {
        use wiremock::matchers::{header_exists, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        let body = json!({
            "coins": [
                {"id": "bitcoin", "symbol": "btc", "name": "Bitcoin", "market_cap_rank": 1},
                {"id": "bitcoin-cash", "symbol": "bch", "name": "Bitcoin Cash", "market_cap_rank": 19}
            ],
            "exchanges": [],
            "icos": [],
            "categories": [],
            "nfts": []
        });

        Mock::given(method("GET"))
            .and(path("/api/v3/search"))
            .and(query_param("query", "bitcoin"))
            .and(header_exists("x-cg-demo-api-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;

        let cfg = CoinGeckoConfig {
            base_url: server.uri(),
            api_key: Some("test-demo-key".to_string()),
            tier: "demo".to_string(),
        };
        let client = CoinGeckoClient::new(cfg);
        let results = client
            .search_coins("bitcoin", 10)
            .await
            .expect("search_coins");

        assert_eq!(results.len(), 2, "expected 2 coin results");
        assert_eq!(results[0].coin_id, "bitcoin");
        assert_eq!(results[0].symbol, "btc");
        assert_eq!(results[0].name, "Bitcoin");
        assert_eq!(results[1].coin_id, "bitcoin-cash");
    }

    #[tokio::test]
    async fn search_coins_empty_query_returns_empty_without_http_call() {
        // No wiremock server — any real HTTP call would fail with connection refused.
        let cfg = CoinGeckoConfig {
            base_url: "http://127.0.0.1:1".to_string(), // unreachable
            api_key: None,
            tier: "demo".to_string(),
        };
        let client = CoinGeckoClient::new(cfg);
        let results = client.search_coins("", 10).await.expect("empty q");
        assert!(results.is_empty(), "empty query must return empty vec without HTTP call");
    }

    #[tokio::test]
    async fn search_coins_degrades_to_empty_on_non_success() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v3/search"))
            .respond_with(ResponseTemplate::new(429).set_body_string("Too Many Requests"))
            .mount(&server)
            .await;

        let cfg = CoinGeckoConfig {
            base_url: server.uri(),
            api_key: None,
            tier: "demo".to_string(),
        };
        let client = CoinGeckoClient::new(cfg);
        let results = client
            .search_coins("bitcoin", 10)
            .await
            .expect("should degrade, not error");

        assert!(
            results.is_empty(),
            "non-success upstream must degrade to empty (REQ-PROV-005)"
        );
    }

    #[tokio::test]
    async fn search_coins_respects_cap() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        let body = json!({
            "coins": [
                {"id": "bitcoin", "symbol": "btc", "name": "Bitcoin"},
                {"id": "bitcoin-cash", "symbol": "bch", "name": "Bitcoin Cash"},
                {"id": "bitcoin-sv", "symbol": "bsv", "name": "Bitcoin SV"}
            ],
            "exchanges": []
        });

        Mock::given(method("GET"))
            .and(path("/api/v3/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;

        let cfg = CoinGeckoConfig {
            base_url: server.uri(),
            api_key: None,
            tier: "demo".to_string(),
        };
        let client = CoinGeckoClient::new(cfg);
        let results = client.search_coins("bitcoin", 2).await.expect("search");

        assert_eq!(results.len(), 2, "cap=2 must truncate to 2 results");
    }

    // ── Scenario 17 (REQ-PROV-005): search_markets sends demo key and parses exchanges array ──

    #[tokio::test]
    async fn search_markets_sends_demo_key_header_and_parses_response() {
        use wiremock::matchers::{header_exists, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        let body = json!({
            "coins": [],
            "exchanges": [
                {
                    "id": "binance",
                    "name": "Binance",
                    "market": {"identifier": "binance"},
                    "base": "BTC",
                    "target": "USDT",
                    "market_type": "spot",
                    "trade_url": "https://binance.com"
                }
            ],
            "icos": [],
            "categories": [],
            "nfts": []
        });

        Mock::given(method("GET"))
            .and(path("/api/v3/search"))
            .and(query_param("query", "btc"))
            .and(header_exists("x-cg-demo-api-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;

        let cfg = CoinGeckoConfig {
            base_url: server.uri(),
            api_key: Some("test-demo-key".to_string()),
            tier: "demo".to_string(),
        };
        let client = CoinGeckoClient::new(cfg);
        let results = client
            .search_markets("btc", 10)
            .await
            .expect("search_markets");

        assert_eq!(results.len(), 1, "expected 1 market result");
        assert_eq!(results[0].base, "BTC");
        assert_eq!(results[0].quote, "USDT");
        assert_eq!(results[0].venue.as_deref(), Some("binance"));
    }

    #[tokio::test]
    async fn search_markets_empty_query_returns_empty_without_http_call() {
        // No wiremock server — any real HTTP call would fail with connection refused.
        let cfg = CoinGeckoConfig {
            base_url: "http://127.0.0.1:1".to_string(), // unreachable
            api_key: None,
            tier: "demo".to_string(),
        };
        let client = CoinGeckoClient::new(cfg);
        let results = client.search_markets("", 10).await.expect("empty q");
        assert!(results.is_empty(), "empty query must return empty vec without HTTP call");
    }

    #[tokio::test]
    async fn search_markets_degrades_to_empty_on_non_success() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v3/search"))
            .respond_with(ResponseTemplate::new(429).set_body_string("Too Many Requests"))
            .mount(&server)
            .await;

        let cfg = CoinGeckoConfig {
            base_url: server.uri(),
            api_key: None,
            tier: "demo".to_string(),
        };
        let client = CoinGeckoClient::new(cfg);
        let results = client
            .search_markets("btc", 10)
            .await
            .expect("should degrade, not error");

        assert!(
            results.is_empty(),
            "non-success upstream must degrade to empty (REQ-PROV-005)"
        );
    }

    #[tokio::test]
    async fn search_markets_respects_cap() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        let body = json!({
            "exchanges": [
                {"base": "BTC", "target": "USDT", "market": {"identifier": "binance"}},
                {"base": "BTC", "target": "USDC", "market": {"identifier": "coinbase"}},
                {"base": "BTC", "target": "EUR",  "market": {"identifier": "kraken"}}
            ]
        });

        Mock::given(method("GET"))
            .and(path("/api/v3/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;

        let cfg = CoinGeckoConfig {
            base_url: server.uri(),
            api_key: None,
            tier: "demo".to_string(),
        };
        let client = CoinGeckoClient::new(cfg);
        let results = client.search_markets("btc", 2).await.expect("search");

        assert_eq!(results.len(), 2, "cap=2 must truncate to 2 results");
    }

    // Live API smoke test (gated — requires real CoinGecko key)
    #[tokio::test]
    #[ignore]
    async fn live_coingecko_bitcoin_markets() {
        let cfg = CoinGeckoConfig::from_env();
        let client = CoinGeckoClient::new(cfg);
        let markets = client
            .fetch_markets(&["bitcoin"], "usd")
            .await
            .expect("live CoinGecko fetch");
        assert!(!markets.is_empty());
        assert!(markets[0].price > rust_decimal_macros::dec!(0));
    }
}
