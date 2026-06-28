use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// Durable work queue entry. Claimed atomically via `FOR UPDATE SKIP LOCKED`.
///
/// Status machine: pending → claimed → running → done | failed.
/// Lease + heartbeat pattern: a crashed replica's `lease_expires_at` expires and the row
/// becomes re-claimable. `attempts` bounds retries before permanent failure.
///
/// @MX:NOTE: [AUTO] collection_queue claim indexes (REQ-DB-032/036):
///   Pending path:     btree(enqueued_at)    WHERE status = 'pending'
///   Lease-expired:    btree(lease_expires_at) WHERE status IN ('claimed','running')
///   Both serve the `SELECT ... FOR UPDATE SKIP LOCKED LIMIT 1` claim query pattern.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct CollectionQueueItem {
    pub id: i64,
    /// 'coin' or 'market'
    pub target_kind: String,
    /// coin_id (for coin targets) or market_id as text (for market targets)
    pub target_id: String,
    /// 'spot' | 'candles' | 'metadata' | 'market' | 'derivatives'
    pub kind: String,
    /// 'pending' | 'claimed' | 'running' | 'done' | 'failed'
    pub status: String,
    pub claimed_by: Option<String>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub enqueued_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Historical backfill job. One job per `(market_id, dataset)`.
/// `UNIQUE (market_id, dataset)` makes enqueue idempotent (REQ-DB-033).
/// A job fans out into `BackfillChunk`s which are the claimable work units.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct BackfillJob {
    pub id: i64,
    pub market_id: i64,
    /// e.g. "candles:1h", "candles:1d"
    pub dataset: String,
    pub status: String,
    pub requested_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Claimable backfill work unit. Crash-resumable via lease + heartbeat + cursor.
///
/// `cursor` is the durable resume marker: the last successfully persisted point within
/// the chunk's `[range_start, range_end)` window. A crashed replica re-claims from `cursor`.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct BackfillChunk {
    pub id: i64,
    pub job_id: i64,
    pub market_id: i64,
    pub dataset: String,
    /// Candle interval (e.g. "1h"), NULL for non-candle datasets.
    pub interval: Option<String>,
    /// Start of this chunk's time window (NULL = whole-dataset single-fetch).
    pub range_start: Option<DateTime<Utc>>,
    /// Exclusive end of this chunk's time window.
    pub range_end: Option<DateTime<Utc>>,
    /// Durable resume marker: last successfully persisted `ts` within this chunk.
    pub cursor: Option<DateTime<Utc>>,
    pub status: String,
    pub claimed_by: Option<String>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Per-provider, credit-aware outbound rate pacer.
///
/// One row per provider. All three workers (live_poller, collection_queue, backfill) consume
/// this table for fleet-wide egress coordination — none redefines it (research §4.4, REQ-DB-034).
///
/// An outbound call acquires a slot by atomically advancing `next_allowed_at` and
/// incrementing `credits_used`, honoring both the per-minute gap and the monthly credit budget.
///
/// The row is seeded on startup for all four known providers so consumers can run
/// `UPDATE … RETURNING` without a prior `INSERT` (REQ-DB-035).
///
/// @MX:NOTE: [AUTO] upstream_request_pacer — mandatory shared egress infrastructure.
///   Consumed by: live_poller (SPEC-SCHED-001), collection_queue worker, backfill worker.
///   credit_limit = NULL means unlimited (e.g. Binance has no monthly cap).
///   The per-provider design supersedes ticker-collector's single-row yf_request_pacer.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct UpstreamRequestPacer {
    pub provider: String,
    pub next_allowed_at: DateTime<Utc>,
    /// Minimum gap in milliseconds between consecutive outbound calls to this provider.
    pub min_gap_ms: i32,
    /// Set when the provider returns 429; all replicas pause until this instant.
    pub cooldown_until: Option<DateTime<Utc>>,
    /// Start of the current monthly credit window.
    pub credit_window_start: DateTime<Utc>,
    /// Credits used in the current window.
    pub credits_used: i64,
    /// Monthly credit cap; NULL = unlimited (e.g. Binance).
    pub credit_limit: Option<i64>,
    pub updated_at: DateTime<Utc>,
}
