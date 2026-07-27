//! No database required — verifies `docs/alarms.md` stays in lockstep with the code
//! condition catalogue (SPEC-ALARM-001 REQ-ALARM-070; the OPTIONAL OR-ALARM-7 parity
//! check). Mirrors the project's existing no-DB static-file conventions (see
//! `migration_files.rs`).

use crypto_collector::alarm::catalog;
use crypto_collector::alarm::registry::HealthRegistry;
use crypto_collector::providers::{AttemptRecord, Capability, ProviderOutcome};

/// Every one of the 14 fingerprint slugs the code can raise must have a matching
/// `### \`{slug}\`` heading in `docs/alarms.md` (OR-ALARM-7).
#[test]
fn docs_alarms_has_heading_for_every_condition_slug() {
    let docs = std::fs::read_to_string("docs/alarms.md").expect("docs/alarms.md must exist");
    for slug in catalog::all_condition_slugs() {
        let heading = format!("### `{slug}`");
        assert!(
            docs.contains(&heading),
            "docs/alarms.md missing entry for `{slug}` (expected heading `{heading}`)"
        );
    }
}

/// Every one of the 14 `code` values the code can raise must appear in `docs/alarms.md`
/// (OR-ALARM-7).
#[test]
fn docs_alarms_has_code_for_every_condition() {
    let docs = std::fs::read_to_string("docs/alarms.md").expect("docs/alarms.md must exist");
    for code in catalog::all_condition_codes() {
        assert!(docs.contains(code), "docs/alarms.md missing code `{code}`");
    }
}

// ── observe_chain_records doc/behavior parity (SPEC-OBS-002 REQ-ALARM-081 / F-48) ──
//
// OR-OBS2-3 resolution (recorded in progress.md §E.2): the CODE is authoritative. The
// corrected doc states `observe_chain_records` derives ONLY the chain-outcome signal
// (all-failed vs any-success among attempted records) and does NOT update the per-provider
// network-failure streak — that is owned by the concrete-error call sites in
// `chain_fetch_ohlc`, gated on `ProviderError::Network`, and the streak counts ONLY Network
// failures (non-Network 5xx do NOT count, REQ-ALARM-020). These tests pin that contract so
// the doc claim is structurally verified, not merely asserted in prose.

fn rec(provider: &str, outcome: ProviderOutcome) -> AttemptRecord {
    AttemptRecord {
        provider: provider.to_string(),
        capability: Capability::Ohlc,
        outcome,
    }
}

#[test]
fn observe_chain_records_derives_chain_outcome_but_not_provider_streak() {
    let reg = HealthRegistry::new();
    // All-failed batch: the chain-outcome signal flips (behavior the doc says it DOES).
    reg.observe_chain_records(&[
        rec("binance", ProviderOutcome::Failure),
        rec("coinbase", ProviderOutcome::Failure),
    ]);
    assert!(
        reg.all_providers_down(),
        "observe_chain_records must derive the chain-outcome signal (all-failed)"
    );
    // The per-provider network-failure streaks are UNTOUCHED (behavior the doc says it does
    // NOT do): observe_chain_records cannot see the concrete ProviderError, so it must not
    // attribute a network failure — the streak counts ONLY ProviderError::Network failures
    // (REQ-ALARM-020), recorded at the concrete-error call sites.
    assert_eq!(
        reg.provider_snapshot("binance")
            .consecutive_network_failures,
        0,
        "observe_chain_records must NOT bump the per-provider network-failure streak"
    );
    assert_eq!(
        reg.provider_snapshot("coinbase")
            .consecutive_network_failures,
        0
    );
}

#[test]
fn observe_chain_records_any_success_clears_chain_outcome() {
    let reg = HealthRegistry::new();
    reg.record_chain_all_failed();
    // A batch with any success clears the chain-outcome signal (doc parity).
    reg.observe_chain_records(&[
        rec("binance", ProviderOutcome::Failure),
        rec("coinbase", ProviderOutcome::Success),
    ]);
    assert!(
        !reg.all_providers_down(),
        "observe_chain_records must clear the chain-outcome signal on any success"
    );
}

/// The overview block documents the feature gate, the TTL self-clearing model, and
/// best-effort delivery (REQ-ALARM-070).
#[test]
fn docs_alarms_overview_covers_required_topics() {
    let docs = std::fs::read_to_string("docs/alarms.md").expect("docs/alarms.md must exist");
    assert!(
        docs.contains("ALARM_CENTER_URL"),
        "must document the feature gate"
    );
    assert!(
        docs.contains("timeoutSeconds"),
        "must document the TTL auto-clear mechanism"
    );
    assert!(
        docs.contains("fast-clear"),
        "must document the fast-clear path"
    );
    assert!(
        docs.contains("crypto-collector:{condition-slug}"),
        "must document the fingerprint scheme"
    );
}
