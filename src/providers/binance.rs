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
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde_json::Value;
use sqlx::PgPool;
use std::str::FromStr;
use std::sync::Arc;

use super::transport;
use crate::pacer::LocalThrottle;

const BINANCE_BASE_URL: &str = "https://api.binance.com";

/// Binance REST API client (thin wrapper, klines endpoint only for SPEC-PROV-001 scope).
pub struct BinanceClient {
    client: reqwest::Client,
    base_url: String,
}

impl BinanceClient {
    pub fn new(base_url: Option<String>) -> Self {
        let client = transport::build_client();
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

        transport::get_json::<Vec<Value>>(resp, "klines").await
    }

    /// `GET /api/v3/klines?symbol={symbol}&interval={interval}&startTime={ms}&endTime={ms}&limit={limit}`
    ///
    /// Date-range-bounded variant used for historical backfill (SPEC-SCHED-001). Binance
    /// returns candles ascending from `start_ms`, capped at `limit` (max 1000 per call) —
    /// callers page forward across multiple calls for windows wider than the page limit.
    pub async fn fetch_klines_range(
        &self,
        symbol: &str,
        interval: &str,
        start_ms: i64,
        end_ms: i64,
        limit: u32,
    ) -> Result<Vec<Value>, ProviderError> {
        let limit_str = limit.to_string();
        let start_str = start_ms.to_string();
        let end_str = end_ms.to_string();
        let resp = self
            .client
            .get(format!("{}/api/v3/klines", self.base_url))
            .query(&[
                ("symbol", symbol),
                ("interval", interval),
                ("startTime", &start_str),
                ("endTime", &end_str),
                ("limit", &limit_str),
            ])
            .send()
            .await?;

        transport::get_json::<Vec<Value>>(resp, "klines range").await
    }

    /// `GET /api/v3/ticker/24hr?symbol={symbol}` — 24-hour rolling-window statistics for a
    /// single symbol (SPEC-PROV-003 F-22). Returns the raw JSON object; callers normalise
    /// it via [`normalise_ticker_24hr`]. This is the honest source for spot price
    /// (`lastPrice`), real 24-hour `volume`, and bid/ask — replacing the 1m-kline
    /// approximation (REQ-PROV-072/073).
    pub async fn fetch_ticker_24hr(&self, symbol: &str) -> Result<Value, ProviderError> {
        let resp = self
            .client
            .get(format!("{}/api/v3/ticker/24hr", self.base_url))
            .query(&[("symbol", symbol)])
            .send()
            .await?;

        transport::get_json::<Value>(resp, "ticker 24hr").await
    }
}

/// Normalise a Binance `GET /api/v3/ticker/24hr` payload into a `SpotQuote` (F-22).
///
/// The spot `price` is the ticker's `lastPrice` (deliberately NOT a 1m-kline close),
/// `volume_24h` is the real 24-hour base-asset `volume`, and bid/ask come from
/// `bidPrice`/`askPrice` — all from the SAME payload. A 1-minute kline volume is never
/// stored in `volume_24h` (REQ-PROV-072/073). All numeric fields are JSON strings parsed
/// exactly to `Decimal` (no `f64`). The timestamp is the ticker `closeTime` (ms), falling
/// back to `Utc::now()` when absent.
fn normalise_ticker_24hr(
    v: &Value,
    market_id: i64,
    vs_currency: &str,
) -> Result<SpotQuote, ProviderError> {
    let field = |name: &'static str| -> Result<&Value, ProviderError> {
        v.get(name)
            .ok_or_else(|| ProviderError::Parse(format!("24hr ticker missing '{name}'")))
    };

    // Required: spot price is the ticker lastPrice (REQ-PROV-072).
    let price = parse_string_decimal(field("lastPrice")?, "lastPrice")?;
    // Real 24-hour base-asset volume — never a 1m kline volume (REQ-PROV-073).
    let volume_24h = Some(parse_string_decimal(field("volume")?, "volume")?);
    let bid = Some(parse_string_decimal(field("bidPrice")?, "bidPrice")?);
    let ask = Some(parse_string_decimal(field("askPrice")?, "askPrice")?);

    let ts = v
        .get("closeTime")
        .and_then(|c| c.as_i64())
        .and_then(DateTime::from_timestamp_millis)
        .unwrap_or_else(Utc::now);

    Ok(SpotQuote {
        market_id,
        ts,
        price,
        bid,
        ask,
        volume_24h,
        vs_currency: vs_currency.to_string(),
        source: "binance".to_string(),
    })
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

/// Snap an interval in seconds to the nearest Binance kline interval string.
///
/// Binance natively supports these kline intervals (in seconds):
/// 1m=60, 3m=180, 5m=300, 15m=900, 30m=1800, 1h=3600, 2h=7200, 4h=14400,
/// 6h=21600, 8h=28800, 12h=43200, 1d=86400, 3d=259200, 1w=604800, 1M=2592000.
///
/// Snapping uses nearest-neighbour by absolute distance (linear, not log-scale).
///
/// Returns a `(snapped_secs, interval_name)` pair (mirroring Bitstamp's
/// `snap_to_bitstamp_step` `(i64, &'static str)` shape) so callers can compute a lookback
/// limit against the interval that was actually requested, not the raw input (F-25).
pub(crate) fn secs_to_kline_interval(interval_secs: i64) -> (i64, &'static str) {
    const INTERVALS: &[(i64, &str)] = &[
        (60, "1m"),
        (180, "3m"),
        (300, "5m"),
        (900, "15m"),
        (1_800, "30m"),
        (3_600, "1h"),
        (7_200, "2h"),
        (14_400, "4h"),
        (21_600, "6h"),
        (28_800, "8h"),
        (43_200, "12h"),
        (86_400, "1d"),
        (259_200, "3d"),
        (604_800, "1w"),
        (2_592_000, "1M"),
    ];
    INTERVALS
        .iter()
        .min_by_key(|(s, _)| (interval_secs - s).abs())
        .map(|(s, name)| (*s, *name))
        .unwrap_or((3_600, "1h"))
}

/// Number of klines that fit in `days` of lookback at the SNAPPED interval, clamped to the
/// Binance per-call maximum (1000). Dividing by the snapped seconds (NOT the raw requested
/// seconds) keeps the lookback correct for a between-band `interval_secs` (F-25,
/// REQ-PROV-079).
fn kline_limit(days: u32, snapped_secs: i64) -> u32 {
    ((days as i64 * 86_400) / snapped_secs.max(1)).clamp(1, 1000) as u32
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
        matches!(
            cap,
            Capability::Spot | Capability::Ohlc | Capability::OhlcRange
        )
    }

    // @MX:NOTE: [AUTO] Binance spot price (lastPrice), volume_24h, and bid/ask all come from
    //           GET /api/v3/ticker/24hr — never a 1m kline (F-22). The price source moved off
    //           the 1m-kline close to the ticker lastPrice deliberately (REQ-PROV-072); a 1m
    //           volume is never stored in the 24h field (REQ-PROV-073).
    // @MX:SPEC: SPEC-PROV-003 REQ-PROV-072 REQ-PROV-073
    async fn fetch_spot(&self, market: &MarketQuery) -> Result<SpotQuote, ProviderError> {
        // F-22: source the spot price (lastPrice), the real 24-hour volume, and bid/ask from
        // the 24hr ticker — routed through the shared paced()/get_json() frame. The old path
        // read a single 1m kline and stored its 1-minute volume in volume_24h (~3 orders of
        // magnitude low) while taking the price from the 1m close.
        let symbol = Self::ticker_symbol(market);

        let ticker = transport::paced(&self.pool, &self.local_throttle, "binance", || {
            self.client.fetch_ticker_24hr(&symbol)
        })
        .await?;

        normalise_ticker_24hr(&ticker, market.market_id, &market.vs_currency)
    }

    async fn fetch_ohlc(
        &self,
        market: &MarketQuery,
        days: u32,
        interval_secs: i64,
    ) -> Result<Vec<OhlcCandle>, ProviderError> {
        let symbol = Self::ticker_symbol(market);
        // F-25: the limit divides by the SNAPPED seconds returned by the snap, not the raw
        // interval_secs — so a between-band input gets the right lookback (REQ-PROV-079).
        let (snapped_secs, interval) = secs_to_kline_interval(interval_secs);
        let limit = kline_limit(days, snapped_secs);

        let klines = transport::paced(&self.pool, &self.local_throttle, "binance", || {
            self.client.fetch_klines(&symbol, interval, limit)
        })
        .await?;

        klines
            .iter()
            .map(|v| normalise_kline(v, market.market_id, interval, &market.vs_currency))
            .collect()
    }

    /// Fetch one page of candles at-or-after `start` and before `end` (REQ backfill).
    ///
    /// Binance's `startTime`/`endTime` are inclusive-ish on the open_time axis; the
    /// server returns up to `limit` ascending klines starting from `start`. A single
    /// call may not cover the whole `[start, end)` window if it exceeds 1000 candles —
    /// the backfill worker's cursor-advance loop pages forward across repeated calls.
    async fn fetch_ohlc_range(
        &self,
        market: &MarketQuery,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        interval_secs: i64,
    ) -> Result<Vec<OhlcCandle>, ProviderError> {
        let symbol = Self::ticker_symbol(market);
        let (_snapped_secs, interval) = secs_to_kline_interval(interval_secs);

        let klines = transport::paced(&self.pool, &self.local_throttle, "binance", || {
            self.client.fetch_klines_range(
                &symbol,
                interval,
                start.timestamp_millis(),
                end.timestamp_millis(),
                1000,
            )
        })
        .await?;

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

    // ── Scenario 4 (REQ-PROV-072/073): honest Binance spot from the 24hr ticker (F-22) ──

    /// Binance `GET /api/v3/ticker/24hr` single-symbol fixture (all numeric fields are
    /// JSON strings, matching the real API).
    fn btc_24hr_ticker_fixture() -> Value {
        json!({
            "symbol": "BTCUSDT",
            "priceChange": "1200.00",
            "priceChangePercent": "1.28",
            "weightedAvgPrice": "94800.00",
            "lastPrice": "95000.10",
            "bidPrice": "94999.50",
            "bidQty": "1.5",
            "askPrice": "95000.70",
            "askQty": "2.1",
            "openPrice": "93800.10",
            "highPrice": "96000.00",
            "lowPrice": "93000.00",
            "volume": "1234567.89",
            "quoteVolume": "117000000000.00",
            "openTime": 1719733600000i64,
            "closeTime": 1719820000000i64,
            "count": 850000
        })
    }

    #[test]
    fn ticker_24hr_normalises_price_from_last_price_and_real_volume_bid_ask() {
        let fixture = btc_24hr_ticker_fixture();
        let quote = normalise_ticker_24hr(&fixture, 7, "usdt").expect("normalise");

        // Price is the ticker lastPrice — NOT a 1m-kline close (REQ-PROV-072).
        assert_eq!(quote.price, dec!(95000.10));
        // Volume_24h is the real 24-hour base-asset volume — never a 1m kline volume
        // (REQ-PROV-073).
        assert_eq!(quote.volume_24h, Some(dec!(1234567.89)));
        // Bid/ask populated from the same payload (REQ-PROV-072).
        assert_eq!(quote.bid, Some(dec!(94999.50)));
        assert_eq!(quote.ask, Some(dec!(95000.70)));
        assert_eq!(quote.source, "binance");
        assert_eq!(quote.vs_currency, "usdt");
        // Timestamp from the ticker closeTime (ms).
        assert_eq!(quote.ts.timestamp(), 1_719_820_000);
    }

    #[test]
    fn ticker_24hr_missing_last_price_hard_fails() {
        // The required spot price (lastPrice) must hard-fail when absent.
        let mut fixture = btc_24hr_ticker_fixture();
        fixture.as_object_mut().unwrap().remove("lastPrice");
        let result = normalise_ticker_24hr(&fixture, 1, "usdt");
        assert!(matches!(result, Err(ProviderError::Parse(_))));
    }

    #[tokio::test]
    async fn http_ticker_24hr_sends_symbol_param_and_parses() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = btc_24hr_ticker_fixture();

        Mock::given(method("GET"))
            .and(path("/api/v3/ticker/24hr"))
            .and(query_param("symbol", "BTCUSDT"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;

        let client = BinanceClient::new(Some(server.uri()));
        let ticker = client
            .fetch_ticker_24hr("BTCUSDT")
            .await
            .expect("fetch_ticker_24hr");
        let quote = normalise_ticker_24hr(&ticker, 1, "usdt").expect("normalise");
        assert_eq!(quote.price, dec!(95000.10));
        assert_eq!(quote.volume_24h, Some(dec!(1234567.89)));
        assert!(quote.bid.is_some() && quote.ask.is_some());
    }

    /// Provider-level `fetch_spot` end-to-end goes through `pacer::acquire_slot` (a real DB
    /// round-trip), so it is DB-gated (`#[ignore]`, run with `DATABASE_URL=... --ignored`),
    /// mirroring `fetch_ohlc_range_normalises_candles_from_provider`. The no-DB coverage of
    /// the price/volume/bid/ask contract lives in the pure + client wiremock tests above.
    #[tokio::test]
    #[ignore]
    async fn fetch_spot_uses_24hr_ticker_price_volume_and_bid_ask() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/ticker/24hr"))
            .respond_with(ResponseTemplate::new(200).set_body_json(btc_24hr_ticker_fixture()))
            .mount(&server)
            .await;

        let pool =
            sqlx::PgPool::connect_lazy("postgres://postgres@localhost/crypto_collector_test")
                .expect("lazy pool");
        let provider = BinanceProvider::new(Some(server.uri()), pool);

        let market = MarketQuery {
            market_id: 7,
            coin_id: Some("bitcoin".to_string()),
            base: "BTC".to_string(),
            quote: "USDT".to_string(),
            venue: None,
            vs_currency: "usdt".to_string(),
        };
        let quote = provider.fetch_spot(&market).await.expect("fetch_spot");
        // Price is the ticker lastPrice, not a 1m-kline close.
        assert_eq!(quote.price, dec!(95000.10));
        assert_eq!(quote.volume_24h, Some(dec!(1234567.89)));
        assert!(quote.bid.is_some() && quote.ask.is_some());
    }

    // ── secs_to_kline_interval: snap to nearest Binance interval ─────────────

    #[test]
    fn snap_exact_intervals_unchanged() {
        assert_eq!(secs_to_kline_interval(60), (60, "1m"));
        assert_eq!(secs_to_kline_interval(3_600), (3_600, "1h"));
        assert_eq!(secs_to_kline_interval(14_400), (14_400, "4h"));
        assert_eq!(secs_to_kline_interval(86_400), (86_400, "1d"));
    }

    #[test]
    fn snap_default_poll_interval_60s_to_1m() {
        // Default global interval (60 s) → nearest Binance interval is 1m
        assert_eq!(secs_to_kline_interval(60), (60, "1m"));
    }

    #[test]
    fn snap_midpoint_between_1m_and_3m_to_lower() {
        // midpoint (60+180)/2 = 120 → distance to 1m is 60, distance to 3m is 60 → ties to 1m
        assert_eq!(secs_to_kline_interval(120), (60, "1m"));
    }

    #[test]
    fn snap_above_midpoint_advances_to_next_interval() {
        // 121 s → closer to 3m (180) than 1m (60) → 3m
        assert_eq!(secs_to_kline_interval(121), (180, "3m"));
    }

    #[test]
    fn snap_large_value_gives_monthly() {
        // 1M = 2592000 s
        assert_eq!(secs_to_kline_interval(2_592_000), (2_592_000, "1M"));
        assert_eq!(secs_to_kline_interval(10_000_000), (2_592_000, "1M"));
    }

    /// Scenario 8 (REQ-PROV-079): a between-band `interval_secs` snaps to a different kline
    /// interval, and the request `limit` divides by the SNAPPED seconds, not the raw input.
    #[test]
    fn snap_between_band_limit_divides_by_snapped_secs_not_raw() {
        // 8000 s snaps to 2h (7200): |8000-7200|=800 < |8000-14400|=6400.
        let (snapped, name) = secs_to_kline_interval(8_000);
        assert_eq!((snapped, name), (7_200, "2h"));
        // limit divides by the SNAPPED 7200 (floor(86400/7200)=12), NOT the raw 8000
        // (floor(86400/8000)=10) — the F-25 fix (REQ-PROV-079).
        assert_eq!(kline_limit(1, snapped), 12);
        assert_ne!(kline_limit(1, snapped), kline_limit(1, 8_000));
    }

    #[test]
    fn kline_limit_clamps_to_binance_max_and_floor() {
        // Clamped to the Binance per-call max of 1000.
        assert_eq!(kline_limit(3650, 86_400), 1000);
        // At least 1 even for a tiny window.
        assert_eq!(kline_limit(0, 86_400), 1);
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

    // ── fetch_klines_range: startTime/endTime/limit param correctness ────────

    #[tokio::test]
    async fn http_klines_range_sends_start_end_limit_params_and_parses() {
        use wiremock::matchers::{method, path, query_param};
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
            .and(query_param("symbol", "BTCUSDT"))
            .and(query_param("interval", "1d"))
            .and(query_param("startTime", "1719820000000"))
            .and(query_param("endTime", "1719910000000"))
            .and(query_param("limit", "1000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;

        let client = BinanceClient::new(Some(server.uri()));
        let klines = client
            .fetch_klines_range("BTCUSDT", "1d", 1_719_820_000_000, 1_719_910_000_000, 1000)
            .await
            .expect("fetch range");
        assert_eq!(klines.len(), 2);

        let candles: Vec<OhlcCandle> = klines
            .iter()
            .map(|v| normalise_kline(v, 1, "1d", "usdt"))
            .collect::<Result<_, _>>()
            .expect("normalise all");
        assert!(candles[0].ts < candles[1].ts);
        assert!(candles.iter().all(|c| c.volume.is_some()));
    }

    #[tokio::test]
    async fn http_klines_range_429_returns_rate_limited() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v3/klines"))
            .respond_with(ResponseTemplate::new(429).set_body_string("Too Many Requests"))
            .mount(&server)
            .await;

        let client = BinanceClient::new(Some(server.uri()));
        let result = client.fetch_klines_range("BTCUSDT", "1d", 0, 1, 1000).await;
        assert!(matches!(result, Err(ProviderError::RateLimited)));
    }

    #[tokio::test]
    async fn binance_supports_ohlc_range() {
        let pool =
            sqlx::PgPool::connect_lazy("postgres://postgres@localhost/crypto_collector_test")
                .expect("lazy pool");
        let provider = BinanceProvider::new(None, pool);
        assert!(provider.supports(Capability::OhlcRange));
    }

    // Requires a live PostgreSQL instance: Provider::fetch_ohlc_range goes through
    // `pacer::acquire_slot`, which performs a real DB round-trip. Opt-in via
    // `DATABASE_URL=... cargo test -- --ignored`, consistent with the db_integration
    // convention; lower-level param/normalisation coverage lives in the wiremock-only
    // `http_klines_range_*` tests above, which need no DB.
    #[tokio::test]
    #[ignore]
    async fn fetch_ohlc_range_normalises_candles_from_provider() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        let body = json!([[
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
        ]]);

        Mock::given(method("GET"))
            .and(path("/api/v3/klines"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;

        let pool =
            sqlx::PgPool::connect_lazy("postgres://postgres@localhost/crypto_collector_test")
                .expect("lazy pool");
        let provider = BinanceProvider::new(Some(server.uri()), pool);

        let market = MarketQuery {
            market_id: 7,
            coin_id: Some("bitcoin".to_string()),
            base: "BTC".to_string(),
            quote: "USDT".to_string(),
            venue: None,
            vs_currency: "usdt".to_string(),
        };

        let start = chrono::DateTime::from_timestamp_millis(1_719_820_000_000).unwrap();
        let end = chrono::DateTime::from_timestamp_millis(1_719_910_000_000).unwrap();
        let candles = provider
            .fetch_ohlc_range(&market, start, end, 86_400)
            .await
            .expect("fetch_ohlc_range");

        assert_eq!(candles.len(), 1);
        assert_eq!(candles[0].source, "binance");
        assert!(candles[0].volume.is_some());
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
