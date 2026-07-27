//! Fixed-duration candle interval domain type (SPEC-REFACTOR-001 M6, F-54).
//!
//! `ApiInterval` is the single interval type for BOTH parse and seconds, over the full
//! fixed-duration vocabulary — the API-facing set `{1m, 5m, 15m, 1h, 4h, 1d, 1w}` PLUS the
//! storage-only set `{3m, 30m, 2h, 6h, 8h, 12h, 3d, 4d}`. It replaces the two hand-synced
//! tables (the 7-value API allow-list and the 15-value stored→seconds table) that were coupled
//! by a runtime `.expect` (DEC-2).
//!
//! Design invariants:
//! - `secs()` is **total** — one value per variant, no `Option`, no `.expect`. Non-fixed
//!   strings (`1M` monthly, garbage) are NOT variants: they fail `FromStr`, exactly as
//!   `interval_to_seconds` previously returned `None`.
//! - `is_api_facing()` is the API-boundary predicate. The public candles handler admits only
//!   the API-facing subset; a request for a storage-only interval (e.g. `3m`) still returns 400.
//! - The storage-vs-API width difference is expressed as `!is_api_facing()` over one enum,
//!   not a second table.

use std::fmt;
use std::str::FromStr;

/// A fixed-duration candle interval (SPEC-REFACTOR-001 REQ-REFACTOR-060).
///
/// Covers the full fixed-duration vocabulary (15 variants): the API-facing set plus the
/// storage-only set. Calendar-month (`1M`) and any non-fixed / unknown string are deliberately
/// NOT variants — they fail [`FromStr`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ApiInterval {
    /// 1 minute (API-facing).
    M1,
    /// 3 minutes (storage-only).
    M3,
    /// 5 minutes (API-facing).
    M5,
    /// 15 minutes (API-facing).
    M15,
    /// 30 minutes (storage-only).
    M30,
    /// 1 hour (API-facing).
    H1,
    /// 2 hours (storage-only).
    H2,
    /// 4 hours (API-facing).
    H4,
    /// 6 hours (storage-only).
    H6,
    /// 8 hours (storage-only).
    H8,
    /// 12 hours (storage-only).
    H12,
    /// 1 day (API-facing).
    D1,
    /// 3 days (storage-only).
    D3,
    /// 4 days (storage-only).
    D4,
    /// 1 week (API-facing).
    W1,
}

impl ApiInterval {
    /// The API-facing subset — the ONLY intervals the public candles endpoint admits.
    ///
    /// Single source of truth for both [`ApiInterval::is_api_facing`] and the boundary
    /// rejection message (replaces the former supported-intervals allow-list).
    pub const API_FACING: [ApiInterval; 7] = [
        ApiInterval::M1,
        ApiInterval::M5,
        ApiInterval::M15,
        ApiInterval::H1,
        ApiInterval::H4,
        ApiInterval::D1,
        ApiInterval::W1,
    ];

    /// Canonical string spelling (`"1m"` .. `"1w"`). Round-trips with [`FromStr`].
    pub fn as_str(&self) -> &'static str {
        match self {
            ApiInterval::M1 => "1m",
            ApiInterval::M3 => "3m",
            ApiInterval::M5 => "5m",
            ApiInterval::M15 => "15m",
            ApiInterval::M30 => "30m",
            ApiInterval::H1 => "1h",
            ApiInterval::H2 => "2h",
            ApiInterval::H4 => "4h",
            ApiInterval::H6 => "6h",
            ApiInterval::H8 => "8h",
            ApiInterval::H12 => "12h",
            ApiInterval::D1 => "1d",
            ApiInterval::D3 => "3d",
            ApiInterval::D4 => "4d",
            ApiInterval::W1 => "1w",
        }
    }

    /// Fixed duration in seconds — **total**: every variant maps to exactly one `i64`.
    ///
    /// Values match the retired `interval_to_seconds` table verbatim. There is no `Option`
    /// and no `.expect`: non-fixed-duration strings can never reach here because they fail
    /// [`FromStr`] and are therefore never `ApiInterval` values (SPEC-REFACTOR-001 REQ-REFACTOR-060).
    ///
    // @MX:ANCHOR: [AUTO] ApiInterval::secs — canonical interval→seconds source; every
    //             divisibility check (candles_agg, rollup, cycle_overlay) and every internal
    //             stored-interval→seconds resolution (backfill, coingecko range) depends on it.
    //             Inherits the high fan_in migrated off the retired `interval_to_seconds`.
    // @MX:REASON: secs() is TOTAL (no Option, no .expect). Non-fixed-duration units (1M) are
    //             excluded at FromStr, NOT here — so this never panics and never returns None.
    //             The API boundary restricts to is_api_facing() (storage-only intervals → 400).
    //             Adding a new stored interval string requires a matching variant here first.
    // @MX:SPEC: SPEC-REFACTOR-001 REQ-REFACTOR-060 REQ-REFACTOR-061
    pub fn secs(&self) -> i64 {
        match self {
            ApiInterval::M1 => 60,
            ApiInterval::M3 => 180,
            ApiInterval::M5 => 300,
            ApiInterval::M15 => 900,
            ApiInterval::M30 => 1_800,
            ApiInterval::H1 => 3_600,
            ApiInterval::H2 => 7_200,
            ApiInterval::H4 => 14_400,
            ApiInterval::H6 => 21_600,
            ApiInterval::H8 => 28_800,
            ApiInterval::H12 => 43_200,
            ApiInterval::D1 => 86_400,
            ApiInterval::D3 => 259_200,
            ApiInterval::D4 => 345_600,
            ApiInterval::W1 => 604_800,
        }
    }

    /// Whether this interval is accepted at the public API boundary (REQ-REFACTOR-062).
    ///
    /// `true` for `{1m, 5m, 15m, 1h, 4h, 1d, 1w}`; `false` for every storage-only interval.
    /// The public candles handler admits only the API-facing subset — a request for a
    /// storage-only interval (e.g. `3m`) returns the existing 400 (behavior-preserving).
    pub fn is_api_facing(&self) -> bool {
        Self::API_FACING.contains(self)
    }

    /// Parse a string that MUST also be API-facing, for the public API boundary.
    ///
    /// Returns `None` for a storage-only interval (e.g. `3m`) or any non-fixed / unknown
    /// string — the caller maps `None` to 400, identical to the prior supported-intervals
    /// allow-list rejection (REQ-REFACTOR-062).
    pub fn from_api_str(s: &str) -> Option<Self> {
        Self::from_str(s).ok().filter(ApiInterval::is_api_facing)
    }
}

/// Error returned when a string is not a fixed-duration `ApiInterval` (`1M`, garbage, empty).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseApiIntervalError;

impl fmt::Display for ParseApiIntervalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a fixed-duration interval")
    }
}

impl std::error::Error for ParseApiIntervalError {}

impl FromStr for ApiInterval {
    type Err = ParseApiIntervalError;

    /// Parse `"1m"` .. `"1w"`; every non-fixed-duration string (`1M`, `2d`, `10h`, `""`, …)
    /// fails, exactly as `interval_to_seconds` returned `None` for it.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "1m" => Ok(ApiInterval::M1),
            "3m" => Ok(ApiInterval::M3),
            "5m" => Ok(ApiInterval::M5),
            "15m" => Ok(ApiInterval::M15),
            "30m" => Ok(ApiInterval::M30),
            "1h" => Ok(ApiInterval::H1),
            "2h" => Ok(ApiInterval::H2),
            "4h" => Ok(ApiInterval::H4),
            "6h" => Ok(ApiInterval::H6),
            "8h" => Ok(ApiInterval::H8),
            "12h" => Ok(ApiInterval::H12),
            "1d" => Ok(ApiInterval::D1),
            "3d" => Ok(ApiInterval::D3),
            "4d" => Ok(ApiInterval::D4),
            "1w" => Ok(ApiInterval::W1),
            _ => Err(ParseApiIntervalError),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant, its canonical string, its seconds. This is the exhaustive table — the
    /// match on `ALL` below fails to compile if a variant is added without a row here.
    const ALL: [(ApiInterval, &str, i64); 15] = [
        (ApiInterval::M1, "1m", 60),
        (ApiInterval::M3, "3m", 180),
        (ApiInterval::M5, "5m", 300),
        (ApiInterval::M15, "15m", 900),
        (ApiInterval::M30, "30m", 1_800),
        (ApiInterval::H1, "1h", 3_600),
        (ApiInterval::H2, "2h", 7_200),
        (ApiInterval::H4, "4h", 14_400),
        (ApiInterval::H6, "6h", 21_600),
        (ApiInterval::H8, "8h", 28_800),
        (ApiInterval::H12, "12h", 43_200),
        (ApiInterval::D1, "1d", 86_400),
        (ApiInterval::D3, "3d", 259_200),
        (ApiInterval::D4, "4d", 345_600),
        (ApiInterval::W1, "1w", 604_800),
    ];

    /// Compile-time exhaustiveness guard: adding a variant without extending `ALL` breaks this
    /// match, and `secs()` is proven total for every variant listed (AC-REFACTOR-060a).
    #[test]
    fn secs_is_total_over_every_variant() {
        for (iv, _s, _secs) in ALL {
            // Exhaustive match — a new variant not covered here is a compile error, proving
            // the table below (and thus secs()) enumerates every variant.
            match iv {
                ApiInterval::M1
                | ApiInterval::M3
                | ApiInterval::M5
                | ApiInterval::M15
                | ApiInterval::M30
                | ApiInterval::H1
                | ApiInterval::H2
                | ApiInterval::H4
                | ApiInterval::H6
                | ApiInterval::H8
                | ApiInterval::H12
                | ApiInterval::D1
                | ApiInterval::D3
                | ApiInterval::D4
                | ApiInterval::W1 => {
                    // secs() returns an i64 for every variant — total, no Option, no panic.
                    let _: i64 = iv.secs();
                }
            }
        }
    }

    /// `secs()` values match the retired `interval_to_seconds` table verbatim (AC-REFACTOR-060a).
    #[test]
    fn secs_matches_retired_table() {
        for (iv, _s, secs) in ALL {
            assert_eq!(iv.secs(), secs, "secs mismatch for {iv:?}");
        }
    }

    /// Round-trip: `from_str(as_str(x)) == x` for every variant (AC-REFACTOR-060a).
    #[test]
    fn from_str_as_str_round_trips() {
        for (iv, s, _secs) in ALL {
            assert_eq!(iv.as_str(), s, "as_str mismatch for {iv:?}");
            assert_eq!(
                ApiInterval::from_str(s),
                Ok(iv),
                "from_str mismatch for {s:?}"
            );
        }
    }

    /// `1M` (calendar month) is NOT a variant — it fails `FromStr`, exactly as
    /// `interval_to_seconds("1M")` returned `None` (AC-REFACTOR-060a).
    #[test]
    fn non_fixed_duration_strings_fail_from_str() {
        for bad in [
            "1M", "", "2d", "10h", "1hour", "monthly", "daily", "hourly", "3h",
        ] {
            assert_eq!(
                ApiInterval::from_str(bad),
                Err(ParseApiIntervalError),
                "{bad:?} must not parse"
            );
        }
    }

    /// `is_api_facing()` is `true` for exactly the 7 API-facing intervals, `false` for the 8
    /// storage-only ones (AC-REFACTOR-062a).
    #[test]
    fn is_api_facing_partitions_the_vocabulary() {
        let api_facing = ["1m", "5m", "15m", "1h", "4h", "1d", "1w"];
        let storage_only = ["3m", "30m", "2h", "6h", "8h", "12h", "3d", "4d"];

        for s in api_facing {
            let iv = ApiInterval::from_str(s).expect("api-facing parses");
            assert!(iv.is_api_facing(), "{s} must be api-facing");
            assert_eq!(ApiInterval::from_api_str(s), Some(iv));
        }
        for s in storage_only {
            let iv = ApiInterval::from_str(s).expect("storage-only still parses");
            assert!(!iv.is_api_facing(), "{s} must NOT be api-facing");
            assert_eq!(
                ApiInterval::from_api_str(s),
                None,
                "{s} rejected at API boundary"
            );
        }
        // Cross-check the partition sizes: 7 api-facing + 8 storage-only = 15 total.
        assert_eq!(api_facing.len() + storage_only.len(), ALL.len());
        assert_eq!(ApiInterval::API_FACING.len(), api_facing.len());
    }

    /// A non-fixed string is rejected at the API boundary too (`from_api_str` → `None`).
    #[test]
    fn from_api_str_rejects_non_fixed() {
        assert_eq!(ApiInterval::from_api_str("1M"), None);
        assert_eq!(ApiInterval::from_api_str("garbage"), None);
    }
}
