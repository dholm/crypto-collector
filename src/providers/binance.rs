//! Binance exchange provider (SPEC-PROV-001 — Scenario 9).
//!
//! Implements kline/candlestick normalization with full `Decimal` volume.
//! Binance returns spot and OHLC with volume; no coin metadata or derivatives endpoints here.
//!
//! Endpoint used: `GET /api/v3/klines` (public, no auth for spot/OHLC).
//! Research §2.3 D5: "Binance is the second provider in the fallback chain for OHLC."

use super::{
    Capability, CoinMarket, CoinMeta, CoinSearchResult, DerivTick, MarketQuery, MarketSearchResult,
    OhlcCandle, Provider, ProviderError, SpotQuote,
};
use async_trait::async_trait;
use chrono::DateTime;
use rust_decimal::Decimal;
use serde_json::Value;
use sqlx::PgPool;
use std::str::FromStr;
use std::sync::Arc;

use crate::pacer::{self, LocalThrottle};

const BINANCE_BASE_URL: &str = "https://api.binance.com";

/// Binance REST API client (thin wrapper, klines endpoint only for SPEC-PROV-001 scope).
pub struct BinanceClient {
    client: reqwest::Client,
    base_url: String,
}

impl BinanceClient {
    pub fn new(base_url: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .gzip(true)
            .build()
            .expect("reqwest client");
        Self {
            client,
            base_url: base_url.unwrap_or_else(|| BINANCE_BASE_URL.to_string()),
        }
    }

    /// `GET /api/v3/klines?symbol={symbol}&interval={interval}&limit={limit}`
    ///
    /// Returns OHLCV candles in Binance wire format (12-element arrays).
    pub async fn fetch_klines(
        &self,
        symbol: &str,
        interval: &str,
        limit: u32,
    ) -> Result<Vec<Value>, ProviderError> {
        let limit_str = limit.to_string();
        let resp = self
            .client
            .get(format!("{}/api/v3/klines", self.base_url))
            .query(&[
                ("symbol", symbol),
                ("interval", interval),
                ("limit", &limit_str),
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

        resp.json::<Vec<Value>>()
            .await
            .map_err(|e| ProviderError::Parse(format!("klines parse error: {e}")))
    }
}

/// Normalise one Binance kline (12-element array) into `OhlcCandle`.
///
/// Binance kline array layout (research §2.3):
/// ```text
/// [0]  open_time (ms)
/// [1]  open (string)
/// [2]  high (string)
/// [3]  low (string)
/// [4]  close (string)
/// [5]  volume (string)  ← non-null, always present
/// [6]  close_time (ms)
/// [7]  quote_asset_volume (string)
/// [8]  number_of_trades
/// [9]  taker_buy_base_asset_volume (string)
/// [10] taker_buy_quote_asset_volume (string)
/// [11] unused (string)
/// ```
///
/// Volume is `Some(Decimal)` — Binance always provides volume (REQ-PROV-016).
pub fn normalise_kline(
    v: &Value,
    market_id: i64,
    interval: &str,
    vs_currency: &str,
) -> Result<OhlcCandle, ProviderError> {
    let arr = v
        .as_array()
        .ok_or_else(|| ProviderError::Parse("kline must be array".to_string()))?;

    if arr.len() < 6 {
        return Err(ProviderError::Parse(format!(
            "kline must have at least 6 elements, got {}",
            arr.len()
        )));
    }

    let open_time_ms = arr[0]
        .as_i64()
        .ok_or_else(|| ProviderError::Parse("kline open_time must be integer".to_string()))?;

    let ts = DateTime::from_timestamp_millis(open_time_ms)
        .ok_or_else(|| ProviderError::Parse(format!("invalid epoch ms: {open_time_ms}")))?;

    let open = parse_string_decimal(&arr[1], "open")?;
    let high = parse_string_decimal(&arr[2], "high")?;
    let low = parse_string_decimal(&arr[3], "low")?;
    let close = parse_string_decimal(&arr[4], "close")?;
    // Volume is always present for Binance klines (REQ-PROV-016)
    let volume = parse_string_decimal(&arr[5], "volume").map(Some)?;

    Ok(OhlcCandle {
        market_id,
        interval: interval.to_string(),
        ts,
        open,
        high,
        low,
        close,
        volume,
        vs_currency: vs_currency.to_string(),
        source: "binance".to_string(),
    })
}

/// Parse a JSON string value as `Decimal`.
fn parse_string_decimal(v: &Value, name: &str) -> Result<Decimal, ProviderError> {
    match v {
        Value::String(s) => Decimal::from_str(s)
            .map_err(|e| ProviderError::Parse(format!("kline {name} parse '{s}': {e}"))),
        Value::Number(n) => {
            let s = n.to_string();
            Decimal::from_str(&s)
                .map_err(|e| ProviderError::Parse(format!("kline {name} parse '{s}': {e}")))
        }
        _ => Err(ProviderError::Parse(format!(
            "kline {name} must be string or number, got {v:?}"
        ))),
    }
}

/// Convert a `days` count to the appropriate Binance kline interval.
///
/// Heuristic: mirrors CoinGecko's auto-bucket approach — use 1d candles for periods ≥ 1 day.
fn days_to_kline_interval(days: u32) -> &'static str {
    match days {
        0..=1 => "1h",
        2..=7 => "4h",
        _ => "1d",
    }
}

// ── BinanceProvider ───────────────────────────────────────────────────────────

/// Binance exchange `Provider` implementation.
///
/// Supports: `Spot` (via latest kline close price), `Ohlc` (via klines).
/// Does NOT support: `CoinMetadata`, `CoinMarket`, `Derivatives` — returns `NotSupported`.
pub struct BinanceProvider {
    client: BinanceClient,
    pool: PgPool,
    local_throttle: Arc<LocalThrottle>,
}

impl BinanceProvider {
    pub fn new(base_url: Option<String>, pool: PgPool) -> Self {
        let local_throttle = Arc::new(LocalThrottle::new(100)); // 100ms min gap (REQ-PROV-017)
        Self {
            client: BinanceClient::new(base_url),
            pool,
            local_throttle,
        }
    }

    /// Build the Binance ticker symbol from base+quote (e.g. "BTC" + "USDT" → "BTCUSDT").
    fn ticker_symbol(market: &MarketQuery) -> String {
        format!(
            "{}{}",
            market.base.to_uppercase(),
            market.quote.to_uppercase()
        )
    }
}

#[async_trait]
impl Provider for BinanceProvider {
    fn name(&self) -> &str {
        "binance"
    }

    fn supports(&self, cap: Capability) -> bool {
        matches!(cap, Capability::Spot | Capability::Ohlc)
    }

    async fn fetch_spot(&self, market: &MarketQuery) -> Result<SpotQuote, ProviderError> {
        // Fetch the single latest 1m kline and use close price as spot
        let symbol = Self::ticker_symbol(market);

        self.local_throttle.acquire().await;
        pacer::acquire_slot(&self.pool, "binance")
            .await
            .map_err(ProviderError::Pacer)?;

        let klines = match self.client.fetch_klines(&symbol, "1m", 1).await {
            Ok(k) => k,
            Err(ProviderError::RateLimited) => {
                let cooldown_ms = crate::config::pacer_cooldown_ms("binance");
                let _ = pacer::signal_cooldown(&self.pool, "binance", cooldown_ms).await;
                return Err(ProviderError::RateLimited);
            }
            Err(e) => return Err(e),
        };

        let kline = klines
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::Parse("no klines returned for spot".to_string()))?;

        let candle = normalise_kline(&kline, market.market_id, "1m", &market.vs_currency)?;

        Ok(SpotQuote {
            market_id: market.market_id,
            ts: candle.ts,
            price: candle.close,
            bid: None,
            ask: None,
            volume_24h: candle.volume, // 1m volume is not 24h but best approximation from kline
            vs_currency: candle.vs_currency,
            source: "binance".to_string(),
        })
    }

    async fn fetch_ohlc(
        &self,
        market: &MarketQuery,
        days: u32,
    ) -> Result<Vec<OhlcCandle>, ProviderError> {
        let symbol = Self::ticker_symbol(market);
        let interval = days_to_kline_interval(days);
        let limit = (days * 24).clamp(1, 1000); // Binance kline limit: min 1, max 1000

        self.local_throttle.acquire().await;
        pacer::acquire_slot(&self.pool, "binance")
            .await
            .map_err(ProviderError::Pacer)?;

        let klines = match self.client.fetch_klines(&symbol, interval, limit).await {
            Ok(k) => k,
            Err(ProviderError::RateLimited) => {
                let cooldown_ms = crate::config::pacer_cooldown_ms("binance");
                let _ = pacer::signal_cooldown(&self.pool, "binance", cooldown_ms).await;
                return Err(ProviderError::RateLimited);
            }
            Err(e) => return Err(e),
        };

        klines
            .iter()
            .map(|v| normalise_kline(v, market.market_id, interval, &market.vs_currency))
            .collect()
    }

    async fn fetch_coin_metadata(&self, _coin_id: &str) -> Result<CoinMeta, ProviderError> {
        Err(ProviderError::NotSupported(Capability::CoinMetadata))
    }

    async fn fetch_coin_market(
        &self,
        _coin_id: &str,
        _vs_currency: &str,
    ) -> Result<CoinMarket, ProviderError> {
        Err(ProviderError::NotSupported(Capability::CoinMarket))
    }

    async fn fetch_derivatives(&self, _market: &MarketQuery) -> Result<DerivTick, ProviderError> {
        Err(ProviderError::NotSupported(Capability::Derivatives))
    }

    async fn search_coins(
        &self,
        _q: &str,
        _cap: usize,
    ) -> Result<Vec<CoinSearchResult>, ProviderError> {
        Ok(vec![])
    }

    async fn fetch_coin_tickers(
        &self,
        _coin_id: &str,
        _cap: usize,
    ) -> Result<Vec<MarketSearchResult>, ProviderError> {
        Ok(vec![])
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use serde_json::json;

    // ── Scenario 9 (REQ-PROV-016): Kline normalisation with Decimal volume ───

    /// Binance kline fixture matching the 12-element format from the API.
    fn btc_kline_fixture() -> Value {
        json!([
            1719820000000i64, // [0] open_time ms
            "94000.50",       // [1] open
            "96000.25",       // [2] high
            "93000.75",       // [3] low
            "95000.10",       // [4] close
            "1234.5678",      // [5] volume (base asset)
            1719823599999i64, // [6] close_time ms
            "117000000.00",   // [7] quote asset volume
            85432,            // [8] number of trades
            "617.89",         // [9] taker buy base
            "58500000.00",    // [10] taker buy quote
            "0"               // [11] unused
        ])
    }

    #[test]
    fn kline_normalises_all_ohlcv_fields_as_decimal() {
        let fixture = btc_kline_fixture();
        let candle = normalise_kline(&fixture, 42, "1d", "usdt").expect("normalise");

        assert_eq!(candle.market_id, 42);
        assert_eq!(candle.interval, "1d");
        assert_eq!(candle.open, dec!(94000.50));
        assert_eq!(candle.high, dec!(96000.25));
        assert_eq!(candle.low, dec!(93000.75));
        assert_eq!(candle.close, dec!(95000.10));
        assert_eq!(candle.source, "binance");
        assert_eq!(candle.vs_currency, "usdt");
    }

    #[test]
    fn kline_volume_is_some_decimal() {
        let fixture = btc_kline_fixture();
        let candle = normalise_kline(&fixture, 1, "1d", "usdt").expect("normalise");

        // Binance ALWAYS has volume — must be Some, never None (REQ-PROV-016)
        assert!(
            candle.volume.is_some(),
            "Binance kline volume must be Some(Decimal)"
        );
        assert_eq!(candle.volume, Some(dec!(1234.5678)));
    }

    #[test]
    fn kline_source_is_binance() {
        let fixture = btc_kline_fixture();
        let candle = normalise_kline(&fixture, 1, "1d", "usdt").expect("normalise");
        assert_eq!(candle.source, "binance");
    }

    #[test]
    fn kline_timestamp_from_open_time_ms() {
        let fixture = btc_kline_fixture();
        let candle = normalise_kline(&fixture, 1, "1d", "usdt").expect("normalise");
        // 1719820000000 ms → timestamp 1719820000 s
        assert_eq!(candle.ts.timestamp(), 1_719_820_000);
    }

    #[test]
    fn kline_too_short_returns_parse_error() {
        let short = json!([1719820000000i64, "100.0", "110.0"]);
        let result = normalise_kline(&short, 1, "1d", "usdt");
        assert!(
            matches!(result, Err(ProviderError::Parse(_))),
            "short kline must return Parse error"
        );
    }

    #[test]
    fn kline_non_array_returns_parse_error() {
        let not_array = json!({"open": "100.0"});
        let result = normalise_kline(&not_array, 1, "1d", "usdt");
        assert!(matches!(result, Err(ProviderError::Parse(_))));
    }

    #[test]
    fn kline_invalid_decimal_returns_parse_error() {
        let bad = json!([
            1719820000000i64,
            "not-a-number", // invalid open
            "96000.25",
            "93000.75",
            "95000.10",
            "1234.5678"
        ]);
        let result = normalise_kline(&bad, 1, "1d", "usdt");
        assert!(matches!(result, Err(ProviderError::Parse(_))));
    }

    // ── Multiple candles ──────────────────────────────────────────────────────

    #[test]
    fn multiple_klines_normalise_to_candle_vec() {
        let klines = json!([
            [
                1719820000000i64,
                "94000.50",
                "96000.25",
                "93000.75",
                "95000.10",
                "1234.56",
                0,
                "",
                0,
                "",
                "",
                ""
            ],
            [
                1719906400000i64,
                "95000.10",
                "97000.00",
                "94500.00",
                "96800.00",
                "2345.67",
                0,
                "",
                0,
                "",
                "",
                ""
            ]
        ]);

        let arr = klines.as_array().unwrap();
        let candles: Vec<OhlcCandle> = arr
            .iter()
            .map(|v| normalise_kline(v, 10, "1d", "usdt"))
            .collect::<Result<_, _>>()
            .expect("normalise all");

        assert_eq!(candles.len(), 2);
        // timestamps are ascending
        assert!(candles[0].ts < candles[1].ts);
        // volumes always Some
        assert!(candles.iter().all(|c| c.volume.is_some()));
    }

    // ── Provider trait: supports() ────────────────────────────────────────────

    #[tokio::test]
    async fn binance_supports_spot_and_ohlc() {
        let pool =
            sqlx::PgPool::connect_lazy("postgres://postgres@localhost/crypto_collector_test")
                .expect("lazy pool");
        let provider = BinanceProvider::new(None, pool);

        assert!(provider.supports(Capability::Spot));
        assert!(provider.supports(Capability::Ohlc));
    }

    #[tokio::test]
    async fn binance_does_not_support_coin_metadata_or_derivatives() {
        let pool =
            sqlx::PgPool::connect_lazy("postgres://postgres@localhost/crypto_collector_test")
                .expect("lazy pool");
        let provider = BinanceProvider::new(None, pool);

        assert!(!provider.supports(Capability::CoinMetadata));
        assert!(!provider.supports(Capability::CoinMarket));
        assert!(!provider.supports(Capability::Derivatives));
    }

    #[tokio::test]
    async fn binance_name_is_binance() {
        let pool =
            sqlx::PgPool::connect_lazy("postgres://postgres@localhost/crypto_collector_test")
                .expect("lazy pool");
        let provider = BinanceProvider::new(None, pool);
        assert_eq!(provider.name(), "binance");
    }

    // ── HTTP tests via wiremock ───────────────────────────────────────────────

    #[tokio::test]
    async fn http_klines_parses_two_candles() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        let body = json!([
            [
                1719820000000i64,
                "94000.50",
                "96000.25",
                "93000.75",
                "95000.10",
                "1234.56",
                1719823599999i64,
                "117000000.00",
                85432,
                "617.89",
                "58500000.00",
                "0"
            ],
            [
                1719906400000i64,
                "95000.10",
                "97000.00",
                "94500.00",
                "96800.00",
                "2345.67",
                1719909999999i64,
                "226000000.00",
                92000,
                "1170.00",
                "113000000.00",
                "0"
            ]
        ]);

        Mock::given(method("GET"))
            .and(path("/api/v3/klines"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;

        let client = BinanceClient::new(Some(server.uri()));
        let klines = client
            .fetch_klines("BTCUSDT", "1d", 2)
            .await
            .expect("fetch");
        assert_eq!(klines.len(), 2);

        let candle = normalise_kline(&klines[0], 1, "1d", "usdt").expect("normalise");
        assert_eq!(candle.open, dec!(94000.50));
        assert!(candle.volume.is_some());
    }

    #[tokio::test]
    async fn http_429_returns_rate_limited() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v3/klines"))
            .respond_with(ResponseTemplate::new(429).set_body_string("Too Many Requests"))
            .mount(&server)
            .await;

        let client = BinanceClient::new(Some(server.uri()));
        let result = client.fetch_klines("BTCUSDT", "1d", 10).await;
        assert!(matches!(result, Err(ProviderError::RateLimited)));
    }

    // Live smoke test (gated)
    #[tokio::test]
    #[ignore]
    async fn live_binance_btcusdt_klines() {
        let client = BinanceClient::new(None);
        let klines = client
            .fetch_klines("BTCUSDT", "1d", 5)
            .await
            .expect("live klines");
        assert_eq!(klines.len(), 5);
        let candle = normalise_kline(&klines[0], 1, "1d", "usdt").expect("normalise");
        assert!(candle.close > dec!(0));
    }
}
