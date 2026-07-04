//! Materialised Bitcoin halving-cycle overlay model (SPEC-CYCLE-001).

use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// One materialised cycle-overlay point (SPEC-CYCLE-001 REQ-CYCLE-040).
///
/// PK: `(coin_id, vs_currency, cycle_number, days_since_halving)`. Recomputed as a full
/// idempotent rebuild on the periodic collector tick (REQ-CYCLE-041/042); the in-progress
/// cycle's values are provisional and MAY change between recomputes (REQ-CYCLE-034).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FromRow)]
pub struct CycleOverlayPoint {
    pub coin_id: String,
    pub vs_currency: String,
    pub cycle_number: i32,
    pub halving_date: NaiveDate,
    pub days_since_halving: i32,
    /// The daily candle's date (D7: the `1d` close's day).
    pub ts: NaiveDate,
    /// Raw daily `1d` close (D7).
    pub price: Decimal,
    /// `price / price_on_halving_anchor` — the anchor day normalises to `1.0` (D2, D8).
    pub norm_halving: Decimal,
    /// `price / cycle_low_price` — the cycle-low day normalises to `1.0` (D2, D7).
    pub norm_cycle_low: Decimal,
    /// `true` when the halving-day anchor was forward-searched because the exact
    /// halving-date candle was absent (D8, REQ-CYCLE-032).
    pub halving_baseline_approximate: bool,
    /// `true` when this point is a forward projection of the last completed cycle's shape
    /// onto the current cycle, rather than a real observed daily candle (REQ-CYCLE-060).
    pub projected: bool,
}
