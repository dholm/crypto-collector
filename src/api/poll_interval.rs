//! Per-coin live-poll interval parsing and normalization (SPEC-API-002 REQ-API-112/113/114).
//!
//! Accepted input format: duration strings like `"5m"`, `"1h"`, `"30s"`, `"1h30m"`, `"1h30m15s"`.
//! Units: `h` (hours), `m` (minutes), `s` (seconds). Must appear in order (h before m before s).
//!
//! Validation: the parsed duration must fall within `[effective_floor, max_secs]` where
//! `effective_floor = max(live_poll_min_interval_secs, live_quote_poll_interval_secs as u64)`.
//! A 422 (not 400) is returned for bound violations (REQ-API-114).
//!
//! DB round-trip: stored as PG `INTERVAL` via `"<N> seconds"` string; read back via
//! `live_poll_interval::TEXT AS live_poll_interval`; normalized via `normalize_pg_interval`.

use super::{ApiError, ApiResult};

// ── Parsing ───────────────────────────────────────────────────────────────────

/// Parse a duration string ("5m", "1h30m", "30s") into total seconds.
///
/// Returns `Err` with a human-readable message on invalid input.
fn parse_hms(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("duration string is empty".to_string());
    }
    let mut remaining = s;
    let mut total_secs: u64 = 0;
    let mut parsed_any = false;

    while !remaining.is_empty() {
        // Extract digit run.
        let digit_end = remaining
            .find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| format!("expected time unit (h/m/s) after digits in '{s}'"))?;

        if digit_end == 0 {
            return Err(format!(
                "expected digit at start of remaining '{remaining}' in '{s}'"
            ));
        }

        let n: u64 = remaining[..digit_end]
            .parse()
            .map_err(|_| format!("number overflow in '{s}'"))?;

        // Single-character unit.
        let unit_char = remaining[digit_end..].chars().next().unwrap();
        match unit_char {
            'h' => total_secs = total_secs.saturating_add(n.saturating_mul(3600)),
            'm' => total_secs = total_secs.saturating_add(n.saturating_mul(60)),
            's' => total_secs = total_secs.saturating_add(n),
            other => {
                return Err(format!(
                    "unknown time unit '{other}' in '{s}': use h, m, or s"
                ))
            }
        }

        remaining = &remaining[digit_end + 1..];
        parsed_any = true;
    }

    if !parsed_any {
        return Err(format!("no duration components found in '{s}'"));
    }

    Ok(total_secs)
}

/// Parse and bounds-check a per-coin live_poll_interval string (REQ-API-113/114).
///
/// Returns a `std::time::Duration` on success.
/// Returns `ApiError::UnprocessableEntity` (422) for parse errors or bound violations.
///
/// Bounds:
/// - `effective_floor = max(min_secs, global_tick_secs as u64)`
/// - `max_secs` from `LIVE_POLL_MAX_INTERVAL_SECS` (default 3600).
///
/// Both bounds violations return 422 per REQ-API-114.
pub fn parse_live_poll_duration(
    s: &str,
    min_secs: u64,
    max_secs: u64,
    global_tick_secs: u64,
) -> ApiResult<std::time::Duration> {
    let total_secs = parse_hms(s)
        .map_err(|e| ApiError::UnprocessableEntity(format!("invalid live_poll_interval: {e}")))?;

    let effective_floor = min_secs.max(global_tick_secs);

    if total_secs < effective_floor {
        return Err(ApiError::UnprocessableEntity(format!(
            "live_poll_interval {s} ({total_secs}s) is below the effective floor of {effective_floor}s \
             (max of min={min_secs}s, global_tick={global_tick_secs}s) (REQ-API-114)"
        )));
    }

    if total_secs > max_secs {
        return Err(ApiError::UnprocessableEntity(format!(
            "live_poll_interval {s} ({total_secs}s) exceeds max of {max_secs}s (REQ-API-114)"
        )));
    }

    Ok(std::time::Duration::from_secs(total_secs))
}

// ── Formatting ────────────────────────────────────────────────────────────────

/// Convert a `Duration` to a human-readable interval string ("5m", "1h30m", "30s").
pub fn duration_to_string(d: std::time::Duration) -> String {
    let total_secs = d.as_secs();
    if total_secs == 0 {
        return "0s".to_string();
    }
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    let mut result = String::new();
    if h > 0 {
        result.push_str(&format!("{h}h"));
    }
    if m > 0 {
        result.push_str(&format!("{m}m"));
    }
    if s > 0 {
        result.push_str(&format!("{s}s"));
    }
    result
}

/// Convert a `Duration` to a PostgreSQL INTERVAL literal accepted by sqlx bind.
///
/// Example: `300 seconds`, `3600 seconds`.
pub fn duration_to_pg_interval(d: std::time::Duration) -> String {
    format!("{} seconds", d.as_secs())
}

/// Convert a PostgreSQL INTERVAL text (or a human-readable duration string) to seconds.
///
/// Accepts both PG wire format `"HH:MM:SS"` and the normalized human-readable form
/// (`"5m"`, `"1h30m"`, `"30s"`).  Returns `None` when the input cannot be parsed.
///
/// Used by collectors to convert `TrackedCoin.live_poll_interval` (returned as TEXT
/// via `::TEXT` cast) into a seconds count for snapping to provider granularities.
pub(crate) fn pg_interval_to_secs(s: &str) -> Option<i64> {
    // Try the HH:MM:SS path first (PG wire format).
    if let Some(normalized) = normalize_pg_interval(s) {
        return parse_hms(&normalized).ok().map(|v| v as i64);
    }
    // Fallback: already a human-readable string ("5m", "1h30m", etc.).
    parse_hms(s).ok().map(|v| v as i64)
}

/// Normalize a PostgreSQL INTERVAL text representation to a human-readable string.
///
/// PostgreSQL returns intervals as `"HH:MM:SS"` in the default `postgres` interval style
/// (e.g. `"00:05:00"` for 5 minutes). This function converts that to `"5m"`, `"1h30m"`, etc.
///
/// Returns `None` when the input cannot be parsed (e.g. intervals with days/months).
pub fn normalize_pg_interval(pg_text: &str) -> Option<String> {
    let s = pg_text.trim();

    // Standard HH:MM:SS format returned by PostgreSQL for pure time intervals.
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() == 3 {
        let h: u64 = parts[0].trim_start_matches('-').parse().ok()?;
        let m: u64 = parts[1].parse().ok()?;
        // Handle fractional seconds (e.g. "00:05:00.000000").
        let s_str = parts[2].split('.').next()?;
        let sec: u64 = s_str.parse().ok()?;

        if h == 0 && m == 0 && sec == 0 {
            return Some("0s".to_string());
        }

        let total = h * 3600 + m * 60 + sec;
        return Some(duration_to_string(std::time::Duration::from_secs(total)));
    }

    // Fallback: try parsing as plain seconds (e.g. "300").
    if let Ok(total_secs) = s.parse::<u64>() {
        return Some(duration_to_string(std::time::Duration::from_secs(
            total_secs,
        )));
    }

    None
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_hms ─────────────────────────────────────────────────────────────

    #[test]
    fn parse_5m_returns_300() {
        assert_eq!(parse_hms("5m").unwrap(), 300);
    }

    #[test]
    fn parse_1h_returns_3600() {
        assert_eq!(parse_hms("1h").unwrap(), 3600);
    }

    #[test]
    fn parse_30s_returns_30() {
        assert_eq!(parse_hms("30s").unwrap(), 30);
    }

    #[test]
    fn parse_1h30m_returns_5400() {
        assert_eq!(parse_hms("1h30m").unwrap(), 5400);
    }

    #[test]
    fn parse_1h30m15s_returns_5415() {
        assert_eq!(parse_hms("1h30m15s").unwrap(), 5415);
    }

    #[test]
    fn parse_empty_is_error() {
        assert!(parse_hms("").is_err());
    }

    #[test]
    fn parse_invalid_unit_is_error() {
        assert!(parse_hms("5x").is_err());
    }

    #[test]
    fn parse_no_unit_is_error() {
        assert!(parse_hms("300").is_err());
    }

    // ── parse_live_poll_duration ──────────────────────────────────────────────

    #[test]
    fn valid_5m_passes_bounds() {
        // min=5, max=3600, global_tick=60 → effective_floor = max(5,60) = 60
        // 5m = 300s >= 60s → OK
        let d = parse_live_poll_duration("5m", 5, 3600, 60).unwrap();
        assert_eq!(d.as_secs(), 300);
    }

    #[test]
    fn below_effective_floor_returns_422() {
        // effective_floor = max(5,60) = 60; "30s" = 30s < 60 → 422
        let err = parse_live_poll_duration("30s", 5, 3600, 60).unwrap_err();
        assert!(matches!(err, ApiError::UnprocessableEntity(_)));
    }

    #[test]
    fn above_max_returns_422() {
        // max=3600; "2h" = 7200s > 3600 → 422
        let err = parse_live_poll_duration("2h", 5, 3600, 60).unwrap_err();
        assert!(matches!(err, ApiError::UnprocessableEntity(_)));
    }

    #[test]
    fn exactly_at_floor_passes() {
        // effective_floor = max(60,60) = 60; "1m" = 60 >= 60 → OK
        let d = parse_live_poll_duration("1m", 5, 3600, 60).unwrap();
        assert_eq!(d.as_secs(), 60);
    }

    #[test]
    fn exactly_at_max_passes() {
        let d = parse_live_poll_duration("1h", 5, 3600, 60).unwrap();
        assert_eq!(d.as_secs(), 3600);
    }

    // ── duration_to_string ────────────────────────────────────────────────────

    #[test]
    fn duration_300s_to_string_is_5m() {
        assert_eq!(
            duration_to_string(std::time::Duration::from_secs(300)),
            "5m"
        );
    }

    #[test]
    fn duration_3600s_to_string_is_1h() {
        assert_eq!(
            duration_to_string(std::time::Duration::from_secs(3600)),
            "1h"
        );
    }

    #[test]
    fn duration_5415s_to_string_is_1h30m15s() {
        assert_eq!(
            duration_to_string(std::time::Duration::from_secs(5415)),
            "1h30m15s"
        );
    }

    // ── normalize_pg_interval ─────────────────────────────────────────────────

    #[test]
    fn normalize_00_05_00_is_5m() {
        assert_eq!(normalize_pg_interval("00:05:00"), Some("5m".to_string()));
    }

    #[test]
    fn normalize_01_30_00_is_1h30m() {
        assert_eq!(normalize_pg_interval("01:30:00"), Some("1h30m".to_string()));
    }

    #[test]
    fn normalize_00_01_00_is_1m() {
        assert_eq!(normalize_pg_interval("00:01:00"), Some("1m".to_string()));
    }

    #[test]
    fn normalize_with_fractional_seconds() {
        assert_eq!(
            normalize_pg_interval("00:05:00.000000"),
            Some("5m".to_string())
        );
    }

    // ── duration_to_pg_interval ───────────────────────────────────────────────

    #[test]
    fn duration_to_pg_interval_300s() {
        assert_eq!(
            duration_to_pg_interval(std::time::Duration::from_secs(300)),
            "300 seconds"
        );
    }

    // ── pg_interval_to_secs ───────────────────────────────────────────────────

    #[test]
    fn pg_interval_hh_mm_ss_parsed_to_secs() {
        // PG INTERVAL wire format "00:05:00" = 5 minutes = 300 s
        assert_eq!(pg_interval_to_secs("00:05:00"), Some(300));
        // "01:30:00" = 90 minutes = 5400 s
        assert_eq!(pg_interval_to_secs("01:30:00"), Some(5_400));
        // "00:01:00" = 60 s
        assert_eq!(pg_interval_to_secs("00:01:00"), Some(60));
    }

    #[test]
    fn pg_interval_human_readable_parsed_to_secs() {
        // Already-normalized strings also accepted
        assert_eq!(pg_interval_to_secs("5m"), Some(300));
        assert_eq!(pg_interval_to_secs("1h"), Some(3_600));
        assert_eq!(pg_interval_to_secs("1h30m"), Some(5_400));
    }

    #[test]
    fn pg_interval_fractional_seconds_ignored() {
        assert_eq!(pg_interval_to_secs("00:05:00.000000"), Some(300));
    }

    #[test]
    fn pg_interval_invalid_returns_none() {
        assert_eq!(pg_interval_to_secs("not-a-duration"), None);
        assert_eq!(pg_interval_to_secs(""), None);
    }
}
