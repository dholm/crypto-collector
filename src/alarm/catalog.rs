//! Pure fingerprint / component / severity / code mapping for the alarm condition
//! catalogue (SPEC-ALARM-001 Milestone 3, REQ-ALARM-003/004).
//!
//! No DB, no HTTP — every function here is a pure function of a [`Condition`] value,
//! unit-testable without any I/O, mirroring `pacer::pacer_decision`'s pure-core pattern.
//!
//! Keep this file and `docs/alarms.md` in lockstep (REQ-ALARM-070): any change to a
//! fingerprint/code/severity/component here must be mirrored there.

use serde::Serialize;
use std::collections::BTreeMap;

/// Alarm severity, serialized to match the Alarm Center OpenAPI `Severity` enum
/// (`Info|Warning|Error|Critical`; this service never raises `Info`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Severity {
    Warning,
    Error,
    Critical,
}

/// The full alarm condition catalogue (13 conditions; backfill contributes 2
/// fingerprints, for 14 total). `sourceService` is always `"crypto-collector"`
/// (REQ-ALARM-003) and is attached by [`AlarmClient`](super::AlarmClient), not stored
/// here.
///
/// @MX:ANCHOR: The condition→fingerprint/component/severity/code mapping is the single
/// source of truth the reconciler (Batch 2/3) and `docs/alarms.md` both derive from.
/// @MX:REASON: Every downstream consumer (reconciler desired-state, operator docs)
/// depends on this mapping never drifting from the Condition Catalogue in spec.md.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    /// REQ-ALARM-020 — catalogue row 1.
    ProviderUnreachable { provider: String },
    /// REQ-ALARM-021 — catalogue row 2.
    ProviderRateLimited { provider: String },
    /// REQ-ALARM-022 — catalogue row 3.
    AllProvidersDown,
    /// REQ-ALARM-023 — catalogue row 4.
    ProviderCreditExhausted { provider: String },
    /// REQ-ALARM-030 — catalogue row 5.
    DbUnreachable,
    /// REQ-ALARM-031 — catalogue row 6.
    MissingPacerRow { provider: String },
    /// REQ-ALARM-032 — catalogue row 7.
    CollectionQueueFailures,
    /// REQ-ALARM-033 — catalogue row 8a.
    BackfillFailed,
    /// REQ-ALARM-033 — catalogue row 8b.
    BackfillStalled,
    /// REQ-ALARM-034 — catalogue row 9.
    WorkerCrashLooping { worker: String },
    /// REQ-ALARM-035 — catalogue row 10.
    StartupConfigError,
    /// REQ-ALARM-040 — catalogue row 11.
    CoinsStalled,
    /// REQ-ALARM-041 — catalogue row 12.
    DbPoolExhausted,
    /// REQ-ALARM-042 — catalogue row 13.
    DbUpsertFailures,
}

/// The `raise` payload's condition-derived fields (everything except `sourceService`,
/// which [`AlarmClient`](super::AlarmClient) attaches, and `timeoutSeconds`, which the
/// client always sets from `config::alarm_ttl_secs()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlarmSpec {
    pub fingerprint: String,
    pub component: &'static str,
    pub severity: Severity,
    pub code: &'static str,
    pub title: String,
    pub description: String,
    pub labels: BTreeMap<String, String>,
    pub details: BTreeMap<String, String>,
}

/// Deterministic fingerprint under the scheme
/// `crypto-collector:{condition-slug}[:{provider-or-component}]` (REQ-ALARM-004).
pub fn fingerprint(condition: &Condition) -> String {
    match condition {
        Condition::ProviderUnreachable { provider } => {
            format!("crypto-collector:provider-unreachable:{provider}")
        }
        Condition::ProviderRateLimited { provider } => {
            format!("crypto-collector:provider-rate-limited:{provider}")
        }
        Condition::AllProvidersDown => "crypto-collector:all-providers-down".to_string(),
        Condition::ProviderCreditExhausted { provider } => {
            format!("crypto-collector:provider-credit-exhausted:{provider}")
        }
        Condition::DbUnreachable => "crypto-collector:db-unreachable".to_string(),
        Condition::MissingPacerRow { provider } => {
            format!("crypto-collector:missing-pacer-row:{provider}")
        }
        Condition::CollectionQueueFailures => {
            "crypto-collector:collection-queue-failures".to_string()
        }
        Condition::BackfillFailed => "crypto-collector:backfill-failed".to_string(),
        Condition::BackfillStalled => "crypto-collector:backfill-stalled".to_string(),
        Condition::WorkerCrashLooping { worker } => {
            format!("crypto-collector:worker-crash-looping:{worker}")
        }
        Condition::StartupConfigError => "crypto-collector:startup-config-error".to_string(),
        Condition::CoinsStalled => "crypto-collector:coins-stalled".to_string(),
        Condition::DbPoolExhausted => "crypto-collector:db-pool-exhausted".to_string(),
        Condition::DbUpsertFailures => "crypto-collector:db-upsert-failures".to_string(),
    }
}

/// The alarm `component` field per the Condition Catalogue.
pub fn component(condition: &Condition) -> &'static str {
    match condition {
        Condition::ProviderUnreachable { .. } => "providers",
        Condition::ProviderRateLimited { .. } => "pacer",
        Condition::AllProvidersDown => "providers",
        Condition::ProviderCreditExhausted { .. } => "pacer",
        Condition::DbUnreachable => "db",
        Condition::MissingPacerRow { .. } => "pacer",
        Condition::CollectionQueueFailures => "collection_queue",
        Condition::BackfillFailed => "backfill",
        Condition::BackfillStalled => "backfill",
        Condition::WorkerCrashLooping { .. } => "collectors",
        Condition::StartupConfigError => "config",
        Condition::CoinsStalled => "live_poller",
        Condition::DbPoolExhausted => "db",
        Condition::DbUpsertFailures => "db",
    }
}

/// The alarm `severity` per the Condition Catalogue.
pub fn severity(condition: &Condition) -> Severity {
    match condition {
        Condition::ProviderUnreachable { .. } => Severity::Warning,
        Condition::ProviderRateLimited { .. } => Severity::Warning,
        Condition::AllProvidersDown => Severity::Critical,
        Condition::ProviderCreditExhausted { .. } => Severity::Error,
        Condition::DbUnreachable => Severity::Critical,
        Condition::MissingPacerRow { .. } => Severity::Error,
        Condition::CollectionQueueFailures => Severity::Warning,
        Condition::BackfillFailed => Severity::Warning,
        Condition::BackfillStalled => Severity::Warning,
        Condition::WorkerCrashLooping { .. } => Severity::Error,
        Condition::StartupConfigError => Severity::Critical,
        Condition::CoinsStalled => Severity::Warning,
        Condition::DbPoolExhausted => Severity::Error,
        Condition::DbUpsertFailures => Severity::Error,
    }
}

/// The alarm `code` per the Condition Catalogue.
pub fn code(condition: &Condition) -> &'static str {
    match condition {
        Condition::ProviderUnreachable { .. } => "PROVIDER_UNREACHABLE",
        Condition::ProviderRateLimited { .. } => "PROVIDER_RATE_LIMITED",
        Condition::AllProvidersDown => "ALL_PROVIDERS_DOWN",
        Condition::ProviderCreditExhausted { .. } => "PROVIDER_CREDIT_EXHAUSTED",
        Condition::DbUnreachable => "DB_UNREACHABLE",
        Condition::MissingPacerRow { .. } => "MISSING_PACER_ROW",
        Condition::CollectionQueueFailures => "COLLECTION_QUEUE_FAILURES",
        Condition::BackfillFailed => "BACKFILL_FAILED",
        Condition::BackfillStalled => "BACKFILL_STALLED",
        Condition::WorkerCrashLooping { .. } => "WORKER_CRASH_LOOPING",
        Condition::StartupConfigError => "STARTUP_CONFIG_ERROR",
        Condition::CoinsStalled => "COINS_STALLED",
        Condition::DbPoolExhausted => "DB_POOL_EXHAUSTED",
        Condition::DbUpsertFailures => "DB_UPSERT_FAILURES",
    }
}

/// Human title/description pair for the alarm payload.
fn title_and_description(condition: &Condition) -> (String, String) {
    match condition {
        Condition::ProviderUnreachable { provider } => (
            format!("Provider unreachable: {provider}"),
            format!(
                "{provider} has produced only network failures with no success for the \
                 sustained-unreachable threshold."
            ),
        ),
        Condition::ProviderRateLimited { provider } => (
            format!("Provider rate-limited: {provider}"),
            format!("{provider}'s fleet-wide pacer cooldown is currently active."),
        ),
        Condition::AllProvidersDown => (
            "All providers in fallback chain failed".to_string(),
            "The most recent provider chain attempt recorded a failure from every \
             provider for a requested capability."
                .to_string(),
        ),
        Condition::ProviderCreditExhausted { provider } => (
            format!("Provider credit exhausted: {provider}"),
            format!("{provider}'s upstream request credit budget is exhausted."),
        ),
        Condition::DbUnreachable => (
            "Database unreachable".to_string(),
            "The readiness DB-ping (SELECT 1) has failed for the sustained-unreachable \
             threshold."
                .to_string(),
        ),
        Condition::MissingPacerRow { provider } => (
            format!("Missing pacer row: {provider}"),
            format!("{provider} is configured but has no upstream_request_pacer row."),
        ),
        Condition::CollectionQueueFailures => (
            "Recent collection-queue failures".to_string(),
            "The windowed count of failed collection_queue rows has reached the \
             configured threshold."
                .to_string(),
        ),
        Condition::BackfillFailed => (
            "Recent backfill-chunk failures".to_string(),
            "The windowed count of failed backfill_chunks rows has reached the \
             configured threshold."
                .to_string(),
        ),
        Condition::BackfillStalled => (
            "Backfill stalled".to_string(),
            "Pending backfill chunks exist but none have advanced for the configured \
             stall threshold."
                .to_string(),
        ),
        Condition::WorkerCrashLooping { worker } => (
            format!("Worker crash-looping: {worker}"),
            format!(
                "{worker} has restarted at least the crash-loop threshold number of \
                 times within the crash-loop window."
            ),
        ),
        Condition::StartupConfigError => (
            "Startup configuration error".to_string(),
            "build_chain failed fast at startup due to an unknown provider name.".to_string(),
        ),
        Condition::CoinsStalled => (
            "Tracked coins not advancing".to_string(),
            "The number of stale tracked coins has reached the configured threshold.".to_string(),
        ),
        Condition::DbPoolExhausted => (
            "Database pool exhausted".to_string(),
            "The database connection pool has had zero idle connections at full size \
             for the configured saturation threshold."
                .to_string(),
        ),
        Condition::DbUpsertFailures => (
            "Sustained database upsert-failure streak".to_string(),
            "The consecutive database upsert-failure streak has reached the configured \
             threshold."
                .to_string(),
        ),
    }
}

/// The 14 fixed fingerprint slugs in the Condition Catalogue (13 conditions; `backfill`
/// contributes 2 — `backfill-failed` and `backfill-stalled`). Used by the OPTIONAL
/// docs-parity check (OR-ALARM-7, REQ-ALARM-070) to assert `docs/alarms.md` carries an
/// entry for every fingerprint the code can raise, without needing a live provider/worker
/// name to instantiate a templated [`Condition`] variant.
pub fn all_condition_slugs() -> &'static [&'static str] {
    &[
        "provider-unreachable",
        "provider-rate-limited",
        "all-providers-down",
        "provider-credit-exhausted",
        "db-unreachable",
        "missing-pacer-row",
        "collection-queue-failures",
        "backfill-failed",
        "backfill-stalled",
        "worker-crash-looping",
        "startup-config-error",
        "coins-stalled",
        "db-pool-exhausted",
        "db-upsert-failures",
    ]
}

/// The `code` value for every fixed condition slug (same 14 entries, same order as
/// [`all_condition_slugs`]). Used by the OPTIONAL docs-parity check (OR-ALARM-7).
pub fn all_condition_codes() -> &'static [&'static str] {
    &[
        "PROVIDER_UNREACHABLE",
        "PROVIDER_RATE_LIMITED",
        "ALL_PROVIDERS_DOWN",
        "PROVIDER_CREDIT_EXHAUSTED",
        "DB_UNREACHABLE",
        "MISSING_PACER_ROW",
        "COLLECTION_QUEUE_FAILURES",
        "BACKFILL_FAILED",
        "BACKFILL_STALLED",
        "WORKER_CRASH_LOOPING",
        "STARTUP_CONFIG_ERROR",
        "COINS_STALLED",
        "DB_POOL_EXHAUSTED",
        "DB_UPSERT_FAILURES",
    ]
}

/// Build the full [`AlarmSpec`] for a condition: fingerprint, component, severity, code,
/// title, description. `labels`/`details` start empty — callers (reconciler, Batch 2/3)
/// populate condition-specific context (e.g. stalled-coin count, capability name).
pub fn to_alarm_spec(condition: &Condition) -> AlarmSpec {
    let (title, description) = title_and_description(condition);
    AlarmSpec {
        fingerprint: fingerprint(condition),
        component: component(condition),
        severity: severity(condition),
        code: code(condition),
        title,
        description,
        labels: BTreeMap::new(),
        details: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── REQ-ALARM-003/004, catalogue rows 1–13 (Scenario 3) ────────────────────

    #[test]
    fn provider_unreachable_mapping() {
        let c = Condition::ProviderUnreachable {
            provider: "binance".to_string(),
        };
        assert_eq!(
            fingerprint(&c),
            "crypto-collector:provider-unreachable:binance"
        );
        assert_eq!(component(&c), "providers");
        assert_eq!(severity(&c), Severity::Warning);
        assert_eq!(code(&c), "PROVIDER_UNREACHABLE");
    }

    #[test]
    fn provider_rate_limited_mapping() {
        let c = Condition::ProviderRateLimited {
            provider: "coingecko".to_string(),
        };
        assert_eq!(
            fingerprint(&c),
            "crypto-collector:provider-rate-limited:coingecko"
        );
        assert_eq!(component(&c), "pacer");
        assert_eq!(severity(&c), Severity::Warning);
        assert_eq!(code(&c), "PROVIDER_RATE_LIMITED");
    }

    #[test]
    fn all_providers_down_mapping() {
        let c = Condition::AllProvidersDown;
        assert_eq!(fingerprint(&c), "crypto-collector:all-providers-down");
        assert_eq!(component(&c), "providers");
        assert_eq!(severity(&c), Severity::Critical);
        assert_eq!(code(&c), "ALL_PROVIDERS_DOWN");
    }

    #[test]
    fn provider_credit_exhausted_mapping() {
        let c = Condition::ProviderCreditExhausted {
            provider: "coingecko".to_string(),
        };
        assert_eq!(
            fingerprint(&c),
            "crypto-collector:provider-credit-exhausted:coingecko"
        );
        assert_eq!(component(&c), "pacer");
        assert_eq!(severity(&c), Severity::Error);
        assert_eq!(code(&c), "PROVIDER_CREDIT_EXHAUSTED");
    }

    #[test]
    fn db_unreachable_mapping() {
        let c = Condition::DbUnreachable;
        assert_eq!(fingerprint(&c), "crypto-collector:db-unreachable");
        assert_eq!(component(&c), "db");
        assert_eq!(severity(&c), Severity::Critical);
        assert_eq!(code(&c), "DB_UNREACHABLE");
    }

    #[test]
    fn missing_pacer_row_mapping() {
        let c = Condition::MissingPacerRow {
            provider: "kraken".to_string(),
        };
        assert_eq!(fingerprint(&c), "crypto-collector:missing-pacer-row:kraken");
        assert_eq!(component(&c), "pacer");
        assert_eq!(severity(&c), Severity::Error);
        assert_eq!(code(&c), "MISSING_PACER_ROW");
    }

    #[test]
    fn collection_queue_failures_mapping() {
        let c = Condition::CollectionQueueFailures;
        assert_eq!(
            fingerprint(&c),
            "crypto-collector:collection-queue-failures"
        );
        assert_eq!(component(&c), "collection_queue");
        assert_eq!(severity(&c), Severity::Warning);
        assert_eq!(code(&c), "COLLECTION_QUEUE_FAILURES");
    }

    #[test]
    fn backfill_failed_mapping() {
        let c = Condition::BackfillFailed;
        assert_eq!(fingerprint(&c), "crypto-collector:backfill-failed");
        assert_eq!(component(&c), "backfill");
        assert_eq!(severity(&c), Severity::Warning);
        assert_eq!(code(&c), "BACKFILL_FAILED");
    }

    #[test]
    fn backfill_stalled_mapping() {
        let c = Condition::BackfillStalled;
        assert_eq!(fingerprint(&c), "crypto-collector:backfill-stalled");
        assert_eq!(component(&c), "backfill");
        assert_eq!(severity(&c), Severity::Warning);
        assert_eq!(code(&c), "BACKFILL_STALLED");
    }

    #[test]
    fn worker_crash_looping_mapping() {
        let c = Condition::WorkerCrashLooping {
            worker: "backfill".to_string(),
        };
        assert_eq!(
            fingerprint(&c),
            "crypto-collector:worker-crash-looping:backfill"
        );
        assert_eq!(component(&c), "collectors");
        assert_eq!(severity(&c), Severity::Error);
        assert_eq!(code(&c), "WORKER_CRASH_LOOPING");
    }

    #[test]
    fn startup_config_error_mapping() {
        let c = Condition::StartupConfigError;
        assert_eq!(fingerprint(&c), "crypto-collector:startup-config-error");
        assert_eq!(component(&c), "config");
        assert_eq!(severity(&c), Severity::Critical);
        assert_eq!(code(&c), "STARTUP_CONFIG_ERROR");
    }

    #[test]
    fn coins_stalled_mapping() {
        let c = Condition::CoinsStalled;
        assert_eq!(fingerprint(&c), "crypto-collector:coins-stalled");
        assert_eq!(component(&c), "live_poller");
        assert_eq!(severity(&c), Severity::Warning);
        assert_eq!(code(&c), "COINS_STALLED");
    }

    #[test]
    fn db_pool_exhausted_mapping() {
        let c = Condition::DbPoolExhausted;
        assert_eq!(fingerprint(&c), "crypto-collector:db-pool-exhausted");
        assert_eq!(component(&c), "db");
        assert_eq!(severity(&c), Severity::Error);
        assert_eq!(code(&c), "DB_POOL_EXHAUSTED");
    }

    #[test]
    fn db_upsert_failures_mapping() {
        let c = Condition::DbUpsertFailures;
        assert_eq!(fingerprint(&c), "crypto-collector:db-upsert-failures");
        assert_eq!(component(&c), "db");
        assert_eq!(severity(&c), Severity::Error);
        assert_eq!(code(&c), "DB_UPSERT_FAILURES");
    }

    #[test]
    fn fingerprint_is_deterministic_across_calls() {
        // Same condition-and-target always maps to the same fingerprint (REQ-ALARM-004).
        let c1 = Condition::ProviderUnreachable {
            provider: "binance".to_string(),
        };
        let c2 = Condition::ProviderUnreachable {
            provider: "binance".to_string(),
        };
        assert_eq!(fingerprint(&c1), fingerprint(&c2));
    }

    #[test]
    fn templated_fingerprints_differ_by_target() {
        let a = Condition::ProviderUnreachable {
            provider: "binance".to_string(),
        };
        let b = Condition::ProviderUnreachable {
            provider: "coinbase".to_string(),
        };
        assert_ne!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn to_alarm_spec_matches_field_functions() {
        let c = Condition::AllProvidersDown;
        let spec = to_alarm_spec(&c);
        assert_eq!(spec.fingerprint, fingerprint(&c));
        assert_eq!(spec.component, component(&c));
        assert_eq!(spec.severity, severity(&c));
        assert_eq!(spec.code, code(&c));
        assert!(!spec.title.is_empty());
        assert!(!spec.description.is_empty());
        assert!(spec.labels.is_empty());
        assert!(spec.details.is_empty());
    }

    // ── OR-ALARM-7: docs-parity source lists (structural, no I/O) ───────────────

    #[test]
    fn all_condition_slugs_and_codes_have_matching_length_and_no_duplicates() {
        let slugs = all_condition_slugs();
        let codes = all_condition_codes();
        assert_eq!(
            slugs.len(),
            14,
            "14 fingerprints total (13 conditions + backfill x2)"
        );
        assert_eq!(codes.len(), slugs.len());

        let mut sorted_slugs = slugs.to_vec();
        sorted_slugs.sort();
        sorted_slugs.dedup();
        assert_eq!(sorted_slugs.len(), slugs.len(), "no duplicate slugs");

        let mut sorted_codes = codes.to_vec();
        sorted_codes.sort();
        sorted_codes.dedup();
        assert_eq!(sorted_codes.len(), codes.len(), "no duplicate codes");
    }

    #[test]
    fn severity_serializes_to_contract_strings() {
        // Alarm Center OpenAPI Severity enum: Info|Warning|Error|Critical.
        assert_eq!(
            serde_json::to_string(&Severity::Warning).unwrap(),
            "\"Warning\""
        );
        assert_eq!(
            serde_json::to_string(&Severity::Error).unwrap(),
            "\"Error\""
        );
        assert_eq!(
            serde_json::to_string(&Severity::Critical).unwrap(),
            "\"Critical\""
        );
    }
}
