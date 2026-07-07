//! Bitstamp exchange provider (SPEC-PROV-001 — deep historical OHLC).
//!
//! Bitstamp is the fallback source for **pre-2017 daily candles** that no other
//! provider in the chain can serve: Binance's BTC/USDT klines begin 2017-08-17,
//! Coinbase 2015, Kraken 2013 — but Bitstamp's public OHLC API exposes BTC/USD
//! daily candles back to **2011-08-18**, needs no API key, and includes volume.
//!
//! Coverage caveat (verified against the live API): only the **daily** (`1d`, step
//! 86400) granularity reaches back to 2011; intraday steps (`5m`, `1h`, …) begin
//! ~2013. The deep-history backfill therefore targets `1d` specifically (see
//! `collectors::backfill::enqueue_deep_history_backfills`).
//!
//! Endpoint: `GET /api/v2/ohlc/{pair}/?step={secs}&limit={n}&start={unix}&end={unix}`
//! (public, no auth). Response: `{"data": {"pair": "...", "ohlc": [{timestamp, open,
//! high, low, close, volume}, …]}}`, ascending by timestamp.

use super::{
    Capability, CoinMarket, CoinMeta, CoinSearchResult, DerivTick, MarketQuery, MarketSearchResult,
    OhlcCandle, Provider, ProviderError, SpotQuote,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use sqlx::PgPool;
use std::str::FromStr;
use std::sync::Arc;

use crate::pacer::{self, LocalThrottle};

const BITSTAMP_BASE_URL: &str = "https://www.bitstamp.net";

/// Bitstamp's per-call OHLC page cap (`limit` maximum).
const BITSTAMP_PAGE_LIMIT: u32 = 1000;

// ── Wire types ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct OhlcEnvelope {
    data: OhlcData,
}

#[derive(Debug, Deserialize)]
struct OhlcData {
    ohlc: Vec<OhlcRow>,
}

/// One Bitstamp OHLC row — all numeric fields are JSON strings (Unix-second timestamp).
#[derive(Debug, Deserialize)]
struct OhlcRow {
    timestamp: String,
    open: String,
    high: String,
    low: String,
    close: String,
    volume: String,
}

// ── HTTP client ─────────────────────────────────────────────────────────────────

/// Thin Bitstamp REST wrapper (OHLC endpoint only for SPEC-PROV-001 scope).
pub struct BitstampClient {
    client: reqwest::Client,
    base_url: String,
}

impl BitstampClient {
    pub fn new(base_url: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .gzip(true)
            .build()
            .expect("reqwest client");
        Self {
            client,
            base_url: base_url.unwrap_or_else(|| BITSTAMP_BASE_URL.to_string()),
        }
    }

    /// `GET /api/v2/ohlc/{pair}/?step={step_secs}&limit={limit}[&start=][&end=]`.
    ///
    /// `start`/`end` are Unix seconds; when both `None` the endpoint returns the most
    /// recent `limit` candles. Rows come back ascending by timestamp.
    async fn fetch_ohlc(
        &self,
        pair: &str,
        step_secs: i64,
        limit: u32,
        start: Option<i64>,
        end: Option<i64>,
    ) -> Result<Vec<OhlcRow>, ProviderError> {
        let step_str = step_secs.to_string();
        let limit_str = limit.to_string();
        let mut query: Vec<(&str, String)> = vec![("step", step_str), ("limit", limit_str)];
        if let Some(s) = start {
            query.push(("start", s.to_string()));
        }
        if let Some(e) = end {
            query.push(("end", e.to_string()));
        }

        let resp = self
            .client
            .get(format!("{}/api/v2/ohlc/{pair}/", self.base_url))
            .query(&query)
            .send()
            .await?;

        let status = resp.status().as_u16();
        if status == 429 {
            return Err(ProviderError::RateLimited);
        }
        // An unknown pair returns 404 — a permanent "no data" for this market, not a
        // transient failure; surface it as an empty page so the chain falls through.
        if status == 404 {
            return Ok(vec![]);
        }
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Http { status, body });
        }

        let envelope = resp
            .json::<OhlcEnvelope>()
            .await
            .map_err(|e| ProviderError::Parse(format!("bitstamp ohlc parse error: {e}")))?;
        Ok(envelope.data.ohlc)
    }
}

/// Normalise one Bitstamp OHLC row into an `OhlcCandle` with the canonical `interval`
/// string (e.g. `"1d"`) so it lands in the same `coin_candles` partition as other
/// providers' candles. Volume is always present (`Some`).
fn normalise_row(
    row: &OhlcRow,
    market_id: i64,
    interval: &str,
    vs_currency: &str,
) -> Result<OhlcCandle, ProviderError> {
    let secs = row.timestamp.parse::<i64>().map_err(|e| {
        ProviderError::Parse(format!("bitstamp timestamp '{}': {e}", row.timestamp))
    })?;
    let ts = DateTime::from_timestamp(secs, 0)
        .ok_or_else(|| ProviderError::Parse(format!("invalid epoch secs: {secs}")))?;

    Ok(OhlcCandle {
        market_id,
        interval: interval.to_string(),
        ts,
        open: parse_decimal(&row.open, "open")?,
        high: parse_decimal(&row.high, "high")?,
        low: parse_decimal(&row.low, "low")?,
        close: parse_decimal(&row.close, "close")?,
        volume: Some(parse_decimal(&row.volume, "volume")?),
        vs_currency: vs_currency.to_string(),
        source: "bitstamp".to_string(),
    })
}

fn parse_decimal(s: &str, name: &str) -> Result<Decimal, ProviderError> {
    Decimal::from_str(s).map_err(|e| ProviderError::Parse(format!("bitstamp {name} '{s}': {e}")))
}

/// Bitstamp's supported `step` values (seconds) paired with our canonical interval
/// strings. Bitstamp does NOT support `8h`, `4d`, or `1w`.
const BITSTAMP_STEPS: &[(i64, &str)] = &[
    (60, "1m"),
    (180, "3m"),
    (300, "5m"),
    (900, "15m"),
    (1_800, "30m"),
    (3_600, "1h"),
    (7_200, "2h"),
    (14_400, "4h"),
    (21_600, "6h"),
    (43_200, "12h"),
    (86_400, "1d"),
    (259_200, "3d"),
];

/// Snap a requested interval (seconds) to the nearest Bitstamp step, returning the
/// step in seconds and the canonical interval string to stamp on each candle.
///
/// Nearest wins; ties break toward the finer (smaller) step. The common backfill
/// intervals — `1d` (86400) and `5m` (300) — map exactly.
fn snap_to_bitstamp_step(interval_secs: i64) -> (i64, &'static str) {
    let target = interval_secs.max(1);
    let mut best = BITSTAMP_STEPS[0];
    let mut best_dist = (target - best.0).abs();
    for &(step, name) in &BITSTAMP_STEPS[1..] {
        let dist = (target - step).abs();
        if dist < best_dist {
            best = (step, name);
            best_dist = dist;
        }
    }
    (best.0, best.1)
}

// ── Provider ─────────────────────────────────────────────────────────────────────

/// Bitstamp exchange `Provider` — deep historical OHLC source (`Ohlc`, `OhlcRange`).
///
/// Does NOT support `Spot`, `CoinMetadata`, `CoinMarket`, `Derivatives` — those stay
/// with the primary providers; Bitstamp exists in the chain purely to serve historical
/// candles (especially the pre-2017 daily window) that other providers cannot.
pub struct BitstampProvider {
    client: BitstampClient,
    pool: PgPool,
    local_throttle: Arc<LocalThrottle>,
}

impl BitstampProvider {
    pub fn new(base_url: Option<String>, pool: PgPool) -> Self {
        Self {
            client: BitstampClient::new(base_url),
            pool,
            // 100 ms min gap between calls (matches Binance's local throttle).
            local_throttle: Arc::new(LocalThrottle::new(100)),
        }
    }

    /// Bitstamp pair symbol: base asset + fiat quote, lowercase, no separator, using
    /// the `vs_currency` (`usd`) rather than the Binance-style `USDT` quote — Bitstamp
    /// lists fiat pairs like `btcusd`.
    fn pair_symbol(market: &MarketQuery) -> String {
        format!(
            "{}{}",
            market.base.to_lowercase(),
            market.vs_currency.to_lowercase()
        )
    }

    async fn acquire(&self) -> Result<(), ProviderError> {
        self.local_throttle.acquire().await;
        pacer::acquire_slot(&self.pool, "bitstamp")
            .await
            .map_err(ProviderError::Pacer)
    }

    /// Map a rate-limit error into a fleet-wide cooldown signal, mirroring Binance.
    async fn signal_rate_limit(&self) {
        let cooldown_ms = crate::config::pacer_cooldown_ms("bitstamp");
        let _ = pacer::signal_cooldown(&self.pool, "bitstamp", cooldown_ms).await;
    }
}

#[async_trait]
impl Provider for BitstampProvider {
    fn name(&self) -> &str {
        "bitstamp"
    }

    fn supports(&self, cap: Capability) -> bool {
        matches!(cap, Capability::Ohlc | Capability::OhlcRange)
    }

    async fn fetch_spot(&self, _market: &MarketQuery) -> Result<SpotQuote, ProviderError> {
        Err(ProviderError::NotSupported(Capability::Spot))
    }

    async fn fetch_ohlc(
        &self,
        market: &MarketQuery,
        days: u32,
        interval_secs: i64,
    ) -> Result<Vec<OhlcCandle>, ProviderError> {
        let pair = Self::pair_symbol(market);
        let (step, interval) = snap_to_bitstamp_step(interval_secs);
        let limit = (((days as i64) * 86_400 / step).max(1)).min(BITSTAMP_PAGE_LIMIT as i64) as u32;

        self.acquire().await?;
        let rows = match self.client.fetch_ohlc(&pair, step, limit, None, None).await {
            Ok(r) => r,
            Err(ProviderError::RateLimited) => {
                self.signal_rate_limit().await;
                return Err(ProviderError::RateLimited);
            }
            Err(e) => return Err(e),
        };

        rows.iter()
            .map(|r| normalise_row(r, market.market_id, interval, &market.vs_currency))
            .collect()
    }

    /// Fetch one page of candles in `[start, end)` (REQ backfill). Bitstamp returns up
    /// to `limit` (1000) ascending candles from `start`; the backfill worker's
    /// cursor-advance loop pages forward across calls for wider windows.
    async fn fetch_ohlc_range(
        &self,
        market: &MarketQuery,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        interval_secs: i64,
    ) -> Result<Vec<OhlcCandle>, ProviderError> {
        let pair = Self::pair_symbol(market);
        let (step, interval) = snap_to_bitstamp_step(interval_secs);

        self.acquire().await?;
        let rows = match self
            .client
            .fetch_ohlc(
                &pair,
                step,
                BITSTAMP_PAGE_LIMIT,
                Some(start.timestamp()),
                Some(end.timestamp()),
            )
            .await
        {
            Ok(r) => r,
            Err(ProviderError::RateLimited) => {
                self.signal_rate_limit().await;
                return Err(ProviderError::RateLimited);
            }
            Err(e) => return Err(e),
        };

        rows.iter()
            .map(|r| normalise_row(r, market.market_id, interval, &market.vs_currency))
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
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://postgres@localhost/crypto_collector_test")
            .expect("lazy pool")
    }

    fn market() -> MarketQuery {
        MarketQuery {
            market_id: 7,
            coin_id: Some("bitcoin".to_string()),
            base: "BTC".to_string(),
            quote: "USDT".to_string(),
            venue: None,
            vs_currency: "usd".to_string(),
        }
    }

    #[tokio::test]
    async fn name_is_bitstamp() {
        let p = BitstampProvider::new(None, test_pool());
        assert_eq!(p.name(), "bitstamp");
    }

    #[tokio::test]
    async fn supports_only_ohlc_capabilities() {
        let p = BitstampProvider::new(None, test_pool());
        assert!(p.supports(Capability::Ohlc));
        assert!(p.supports(Capability::OhlcRange));
        assert!(!p.supports(Capability::Spot));
        assert!(!p.supports(Capability::CoinMetadata));
        assert!(!p.supports(Capability::CoinMarket));
        assert!(!p.supports(Capability::Derivatives));
    }

    #[test]
    fn pair_symbol_uses_vs_currency_not_quote() {
        // MarketQuery.quote is "USDT" (Binance convention) but Bitstamp lists fiat
        // pairs — the pair must be built from vs_currency ("usd") → "btcusd".
        assert_eq!(BitstampProvider::pair_symbol(&market()), "btcusd");
    }

    #[test]
    fn snap_maps_common_intervals_exactly() {
        assert_eq!(snap_to_bitstamp_step(86_400), (86_400, "1d"));
        assert_eq!(snap_to_bitstamp_step(300), (300, "5m"));
        assert_eq!(snap_to_bitstamp_step(3_600), (3_600, "1h"));
    }

    #[test]
    fn snap_unsupported_intervals_go_to_nearest_step() {
        // 8h (28800) unsupported → nearest is 6h (21600) vs 12h (43200): 6h is closer.
        assert_eq!(snap_to_bitstamp_step(28_800), (21_600, "6h"));
        // 1w (604800) unsupported → clamps to the coarsest step, 3d.
        assert_eq!(snap_to_bitstamp_step(604_800), (259_200, "3d"));
        // Sub-minute → finest step 1m.
        assert_eq!(snap_to_bitstamp_step(30), (60, "1m"));
    }

    #[test]
    fn normalise_row_produces_decimal_candle_with_volume() {
        let row = OhlcRow {
            timestamp: "1313625600".to_string(),
            open: "10.90".to_string(),
            high: "11.50".to_string(),
            low: "10.10".to_string(),
            close: "11.00".to_string(),
            volume: "123.45".to_string(),
        };
        let c = normalise_row(&row, 7, "1d", "usd").expect("normalise");
        assert_eq!(c.interval, "1d");
        assert_eq!(c.open, dec!(10.90));
        assert_eq!(c.close, dec!(11.00));
        assert_eq!(c.volume, Some(dec!(123.45)));
        assert_eq!(c.vs_currency, "usd");
        assert_eq!(c.source, "bitstamp");
        assert_eq!(c.ts, DateTime::from_timestamp(1_313_625_600, 0).unwrap());
    }

    #[tokio::test]
    async fn http_range_sends_step_start_end_and_parses_ascending() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "data": {
                "pair": "BTC/USD",
                "ohlc": [
                    {"timestamp":"1313625600","open":"10.90","high":"11.5","low":"10.1","close":"11.0","volume":"1.5"},
                    {"timestamp":"1313712000","open":"11.0","high":"12.0","low":"10.8","close":"11.85","volume":"2.0"}
                ]
            }
        });
        Mock::given(method("GET"))
            .and(path("/api/v2/ohlc/btcusd/"))
            .and(query_param("step", "86400"))
            .and(query_param("start", "1313625600"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let p = BitstampProvider::new(Some(server.uri()), test_pool());
        let start = DateTime::from_timestamp(1_313_625_600, 0).unwrap();
        let end = DateTime::from_timestamp(1_320_000_000, 0).unwrap();
        // Bypass the pacer (no DB) by calling the client directly through the provider's
        // range method is not possible without a slot; assert the client instead.
        let rows = p
            .client
            .fetch_ohlc(
                "btcusd",
                86_400,
                1000,
                Some(start.timestamp()),
                Some(end.timestamp()),
            )
            .await
            .expect("fetch");
        assert_eq!(rows.len(), 2);
        let candles: Vec<OhlcCandle> = rows
            .iter()
            .map(|r| normalise_row(r, 7, "1d", "usd").unwrap())
            .collect();
        assert!(candles[0].ts < candles[1].ts, "ascending by timestamp");
        assert_eq!(candles[1].close, dec!(11.85));
    }

    #[tokio::test]
    async fn unknown_pair_404_is_empty_not_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v2/ohlc/zzzusd/"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let p = BitstampProvider::new(Some(server.uri()), test_pool());
        let rows = p
            .client
            .fetch_ohlc("zzzusd", 86_400, 10, Some(1_313_625_600), None)
            .await
            .expect("404 must degrade to empty, not error");
        assert!(rows.is_empty());
    }
}
