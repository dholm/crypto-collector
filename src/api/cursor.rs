//! Keyset cursor encode/decode for `/v1` list endpoints (SPEC-API-001 REQ-API-070/071).
//!
//! An opaque base64url-no-pad JSON blob encodes the ordering-key tuple of the last
//! returned row. This is O(1)-deep and stable under concurrent appends on partitioned
//! append-heavy tables — unlike OFFSET which skips/duplicates rows (research §4.6).
//!
//! Key types per endpoint group:
//!
//! | Endpoint                              | Key type      | Ordering      |
//! |---------------------------------------|---------------|---------------|
//! | `GET /v1/coins`                       | `CoinListKey` | `coin_id ASC` |
//! | `GET /v1/coins/{coin_id}/quotes`      | `TsKey`       | `ts DESC`     |
//! | `GET /v1/coins/{coin_id}/candles`     | `TsKey`       | `ts DESC`     |
//! | `GET /v1/coins/{coin_id}/market`      | `TsKey`       | `ts DESC`     |
//!
// @MX:ANCHOR: [AUTO] encode_keyset_cursor / decode_keyset_cursor — every /v1 list endpoint depends on this contract
// @MX:REASON: All list endpoint families use these helpers. Changing the encoding (base64url no-pad JSON)
//             or the key type shapes breaks every existing cursor token held by callers. Keyset, not OFFSET,
//             for stability over partitioned append-heavy tables (REQ-API-070/071).
// @MX:SPEC: SPEC-API-001 REQ-API-070 REQ-API-071 SPEC-API-002

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ── Keyset key types ──────────────────────────────────────────────────────────

/// Keyset position for `GET /v1/coins` — ordered `coin_id ASC`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoinListKey {
    pub coin_id: String,
}

/// Generic timestamp key for all time-series list endpoints — ordered `ts DESC`.
///
/// Used by coin quotes, coin candles, and coin market snapshots.
/// The resource-specific filter (coin_id, vs_currency, interval, etc.) is kept in the
/// WHERE clause, NOT in the cursor, so the cursor remains compact and stable across
/// re-registrations (REQ-API-070).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TsKey {
    /// Timestamp of the last row returned (exclusive lower bound on next page, DESC ordering).
    pub ts: DateTime<Utc>,
}

/// Keyset position for `GET /v1/coins/{coin_id}/cycle-overlay` (SPEC-CYCLE-001 REQ-CYCLE-051).
///
/// Ordered `(cycle_number ASC, days_since_halving ASC)`. The resource-specific filter
/// (coin_id, vs_currency, optional cycle_number) is kept in the WHERE clause, not the cursor.
///
// @MX:ANCHOR: [AUTO] CycleOverlayKey — the cycle-overlay route's pagination contract.
// @MX:REASON: fan_in >= 3: cycle_overlay handler, encode/decode round-trip tests, acceptance
//             Scenario 14. Reuses the generic encode_keyset_cursor/decode_keyset_cursor helpers
//             unchanged (REQ-CYCLE-051); the ordering key shape must stay in sync with the
//             cycle_overlay_points read route's ORDER BY (cycle_number ASC, days_since_halving ASC).
// @MX:SPEC: SPEC-CYCLE-001 REQ-CYCLE-051
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CycleOverlayKey {
    pub cycle_number: i32,
    pub days_since_halving: i32,
}

// ── Encode / decode ───────────────────────────────────────────────────────────

/// Encode any serializable keyset key as an opaque base64url (no-pad) cursor string.
///
/// Panics only if `T` is not serializable — that would be a compile-time bug.
pub fn encode_keyset_cursor<T: Serialize>(key: &T) -> String {
    use base64::Engine;
    let json = serde_json::to_string(key).expect("keyset key must be serializable");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json.as_bytes())
}

/// Decode an opaque cursor string back into a keyset key.
///
/// Returns `Err` on any decode failure so the handler can return 400 (REQ-API-071).
pub fn decode_keyset_cursor<T: for<'de> Deserialize<'de>>(cursor: &str) -> anyhow::Result<T> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| anyhow::anyhow!("invalid cursor: bad base64 encoding"))?;
    serde_json::from_slice(&bytes).map_err(|e| anyhow::anyhow!("invalid cursor: {e}"))
}

// ── Limit validation ──────────────────────────────────────────────────────────

/// Default page size (OR-API-3 resolved).
pub const DEFAULT_LIMIT: i64 = 100;
/// Maximum page size (OR-API-3 resolved).
pub const MAX_LIMIT: i64 = 1000;

/// Validate and bound a caller-supplied `limit`, returning 400 on failure.
///
/// Returns `DEFAULT_LIMIT` when `limit` is `None`.
pub fn validate_limit(limit: Option<i64>) -> anyhow::Result<i64> {
    match limit {
        None => Ok(DEFAULT_LIMIT),
        Some(n) if !(1..=MAX_LIMIT).contains(&n) => {
            Err(anyhow::anyhow!("limit must be between 1 and {MAX_LIMIT}"))
        }
        Some(n) => Ok(n),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap()
    }

    // Scenario 10 (REQ-API-070): cursor round-trips — CoinListKey
    #[test]
    fn coin_list_key_roundtrip() {
        let key = CoinListKey {
            coin_id: "bitcoin".into(),
        };
        let encoded = encode_keyset_cursor(&key);
        let decoded: CoinListKey = decode_keyset_cursor(&encoded).unwrap();
        assert_eq!(decoded, key);
    }

    // Scenario 10: cursor round-trips — TsKey (coin quotes / candles / coin market)
    #[test]
    fn ts_key_roundtrip() {
        let key = TsKey { ts: ts() };
        let encoded = encode_keyset_cursor(&key);
        let decoded: TsKey = decode_keyset_cursor(&encoded).unwrap();
        assert_eq!(decoded, key);
    }

    // Scenario 14 (REQ-CYCLE-051): cursor round-trips — CycleOverlayKey
    #[test]
    fn cycle_overlay_key_roundtrip() {
        let key = CycleOverlayKey {
            cycle_number: 3,
            days_since_halving: 200,
        };
        let encoded = encode_keyset_cursor(&key);
        let decoded: CycleOverlayKey = decode_keyset_cursor(&encoded).unwrap();
        assert_eq!(decoded, key);
    }

    // Scenario 10 (REQ-API-071): invalid base64 → error
    #[test]
    fn decode_invalid_base64_returns_error() {
        let result = decode_keyset_cursor::<TsKey>("not!!valid!!base64");
        assert!(result.is_err(), "invalid base64 must return error");
    }

    // Scenario 10 (REQ-API-071): valid base64, wrong JSON structure → error
    #[test]
    fn decode_wrong_json_structure_returns_error() {
        use base64::Engine;
        let bad_json =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"wrong_field": 42}"#);
        let result = decode_keyset_cursor::<TsKey>(&bad_json);
        assert!(result.is_err(), "wrong JSON structure must return error");
    }

    // Cursor is opaque base64url (no padding chars '=')
    #[test]
    fn encoded_cursor_has_no_padding() {
        let key = TsKey { ts: ts() };
        let encoded = encode_keyset_cursor(&key);
        assert!(
            !encoded.contains('='),
            "cursor must not contain base64 padding"
        );
    }

    // Different keys produce different cursors
    #[test]
    fn different_coin_ids_produce_different_cursors() {
        let k1 = CoinListKey {
            coin_id: "bitcoin".into(),
        };
        let k2 = CoinListKey {
            coin_id: "ethereum".into(),
        };
        assert_ne!(encode_keyset_cursor(&k1), encode_keyset_cursor(&k2));
    }

    // Scenario 11 (REQ-API-072): validate_limit
    #[test]
    fn validate_limit_none_returns_default() {
        assert_eq!(validate_limit(None).unwrap(), DEFAULT_LIMIT);
    }

    #[test]
    fn validate_limit_valid_value_passes() {
        assert_eq!(validate_limit(Some(50)).unwrap(), 50);
        assert_eq!(validate_limit(Some(1)).unwrap(), 1);
        assert_eq!(validate_limit(Some(MAX_LIMIT)).unwrap(), MAX_LIMIT);
    }

    #[test]
    fn validate_limit_zero_returns_error() {
        assert!(validate_limit(Some(0)).is_err());
    }

    #[test]
    fn validate_limit_above_max_returns_error() {
        assert!(validate_limit(Some(MAX_LIMIT + 1)).is_err());
    }

    #[test]
    fn validate_limit_negative_returns_error() {
        assert!(validate_limit(Some(-1)).is_err());
    }
}
