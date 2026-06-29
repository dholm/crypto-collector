## SPEC-API-002 Progress

- Started: 2026-06-29
- Harness: standard
- Methodology: TDD (RED-GREEN-REFACTOR)
- Language: Rust / moai-lang-rust
- Execution mode: Full Pipeline (files: ~20, domains: backend+database+api+collectors)
- Branch strategy: commit to main (no feature branches)
- Phase 1 complete: strategy analysis done, tasks T-001 to T-009 + T-A + T-B decomposed
- Scope decision: T-A and T-B (collector re-base) in scope; 0011 not to be deployed until T-A complete
- Key corrections from analysis: 3-column migration 0010, add LIVE_POLL_MIN/MAX_INTERVAL_SECS to config.rs, reuse TsKey cursor
