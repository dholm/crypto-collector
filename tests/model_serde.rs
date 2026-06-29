//! Unconditional tests — no database required.
//!
//! Verifies model serialization/deserialization and rust_decimal precision round-trips.
//! These tests run as part of `cargo test` without DATABASE_URL.

use chrono::Utc;
use crypto_collector::models::{
    coin::{CoinMarketSnapshot, TrackedCoin},
    queue::{BackfillChunk, BackfillJob, CollectionQueueItem, UpstreamRequestPacer},
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::str::FromStr;

// ── Decimal precision round-trips (REQ-DB-040) ───────────────────────────────

#[test]
fn decimal_tiny_price_round_trip() {
    // Micro-cap tokens can have prices as small as 1e-11 (research §1.5)
    let price = dec!(0.00000000001);
    let serialized = price.to_string();
    let deserialized: Decimal = serialized.parse().expect("should parse back");
    assert_eq!(
        price, deserialized,
        "tiny price must survive string round-trip"
    );
}

#[test]
fn decimal_large_supply_round_trip() {
    // SHIB-like supply: ~5.89 × 10^14 (research §1.5)
    let supply = Decimal::from_str("589735030408323").expect("valid decimal");
    let serialized = supply.to_string();
    let deserialized: Decimal = serialized.parse().expect("should parse back");
    assert_eq!(
        supply, deserialized,
        "large supply must survive string round-trip"
    );
}

#[test]
fn decimal_market_cap_product_exact() {
    // market_cap = price × supply must stay exact for reconciliation
    let price = dec!(0.00001); // $0.00001 per token
    let supply = Decimal::from_str("589735030408323").expect("valid decimal");
    let market_cap = price * supply;
    assert!(market_cap > Decimal::ZERO, "market cap must be positive");
    // Round-trip via string
    let serialized = market_cap.to_string();
    let deserialized: Decimal = serialized.parse().expect("should parse back");
    assert_eq!(
        market_cap, deserialized,
        "market cap product must survive string round-trip"
    );
}

#[test]
fn decimal_funding_rate_negative() {
    // Funding rates can be negative (research §1.4)
    let rate = dec!(-0.0001);
    assert!(rate < Decimal::ZERO);
    let serialized = rate.to_string();
    let deserialized: Decimal = serialized.parse().expect("should parse back");
    assert_eq!(rate, deserialized);
}

#[test]
fn decimal_zero_is_exact() {
    let zero = Decimal::ZERO;
    let serialized = zero.to_string();
    let deserialized: Decimal = serialized.parse().expect("should parse back");
    assert_eq!(zero, deserialized);
}

// ── Model serialization / deserialization ────────────────────────────────────

#[test]
fn tracked_coin_serializes_with_correct_fields() {
    let coin = TrackedCoin {
        coin_id: "bitcoin".to_string(),
        symbol: "BTC".to_string(),
        name: "Bitcoin".to_string(),
        status: "active".to_string(),
        registered_at: Utc::now(),
        last_collected_at: None,
        error: None,
        live_poll_interval: None,
    };
    let json = serde_json::to_string(&coin).expect("should serialize");
    assert!(json.contains("\"coin_id\":\"bitcoin\""), "coin_id in JSON");
    assert!(json.contains("\"symbol\":\"BTC\""), "symbol in JSON");
    assert!(json.contains("\"status\":\"active\""), "status in JSON");
}

#[test]
fn tracked_coin_deserializes_from_json() {
    let json = r#"{
        "coin_id": "ethereum",
        "symbol": "ETH",
        "name": "Ethereum",
        "status": "active",
        "registered_at": "2024-01-01T00:00:00Z",
        "last_collected_at": null,
        "error": null
    }"#;
    let coin: TrackedCoin = serde_json::from_str(json).expect("should deserialize");
    assert_eq!(coin.coin_id, "ethereum");
    assert_eq!(coin.symbol, "ETH");
    assert_eq!(coin.status, "active");
}

#[test]
fn coin_market_snapshot_has_aggregate_fields() {
    // REQ-DB-012/022: aggregates are time-series, not revisions
    let snapshot = CoinMarketSnapshot {
        coin_id: "bitcoin".to_string(),
        vs_currency: "usd".to_string(),
        ts: Utc::now(),
        price: dec!(42000.0),
        market_cap: Some(dec!(800000000000.0)),
        fully_diluted_valuation: Some(dec!(882000000000.0)),
        circulating_supply: Some(dec!(19000000.0)),
        total_supply: Some(dec!(21000000.0)),
        volume_24h: Some(dec!(28000000000.0)),
        source: "coingecko".to_string(),
    };
    let json = serde_json::to_string(&snapshot).expect("should serialize");
    assert!(json.contains("market_cap"), "market_cap in snapshot JSON");
    assert!(
        json.contains("circulating_supply"),
        "circulating_supply in snapshot JSON"
    );
    assert!(
        json.contains("fully_diluted_valuation"),
        "fdv in snapshot JSON"
    );
}

#[test]
fn collection_queue_item_status_fields() {
    let item = CollectionQueueItem {
        id: 1,
        target_kind: "market".to_string(),
        target_id: "1".to_string(),
        kind: "spot".to_string(),
        status: "pending".to_string(),
        claimed_by: None,
        lease_expires_at: None,
        heartbeat_at: None,
        attempts: 0,
        last_error: None,
        enqueued_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let json = serde_json::to_string(&item).expect("should serialize");
    assert!(
        json.contains("\"status\":\"pending\""),
        "status in queue JSON"
    );
    assert!(json.contains("\"attempts\":0"), "attempts in queue JSON");
}

#[test]
fn backfill_job_fields() {
    let job = BackfillJob {
        id: 42,
        coin_id: "bitcoin".to_string(),
        dataset: "candles:1h".to_string(),
        status: "pending".to_string(),
        requested_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let json = serde_json::to_string(&job).expect("should serialize");
    assert!(
        json.contains("\"dataset\":\"candles:1h\""),
        "dataset in backfill job JSON"
    );
}

#[test]
fn backfill_chunk_cursor_is_optional() {
    // Cursor is nullable: resume marker starts as NULL
    let chunk = BackfillChunk {
        id: 1,
        job_id: 42,
        coin_id: "bitcoin".to_string(),
        dataset: "candles:1h".to_string(),
        interval: Some("1h".to_string()),
        range_start: None,
        range_end: None,
        cursor: None,
        status: "pending".to_string(),
        claimed_by: None,
        lease_expires_at: None,
        heartbeat_at: None,
        attempts: 0,
        last_error: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let json = serde_json::to_string(&chunk).expect("should serialize");
    assert!(
        json.contains("\"cursor\":null"),
        "cursor must be null when not set"
    );
}

#[test]
fn upstream_request_pacer_credit_limit_nullable() {
    // NULL credit_limit = unlimited (e.g. Binance has no monthly cap)
    let pacer = UpstreamRequestPacer {
        provider: "binance".to_string(),
        next_allowed_at: Utc::now(),
        min_gap_ms: 100,
        cooldown_until: None,
        credit_window_start: Utc::now(),
        credits_used: 0,
        credit_limit: None,
        updated_at: Utc::now(),
    };
    let json = serde_json::to_string(&pacer).expect("should serialize");
    assert!(
        json.contains("\"credit_limit\":null"),
        "NULL credit_limit must serialize as null (unlimited): {json}"
    );
}
