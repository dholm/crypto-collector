//! No database required — verifies `docs/alarms.md` stays in lockstep with the code
//! condition catalogue (SPEC-ALARM-001 REQ-ALARM-070; the OPTIONAL OR-ALARM-7 parity
//! check). Mirrors the project's existing no-DB static-file conventions (see
//! `migration_files.rs`).

use crypto_collector::alarm::catalog;

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
