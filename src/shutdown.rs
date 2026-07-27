//! Graceful-shutdown helpers shared by long-running `select!` loops (SPEC-OBS-002).

/// Decide whether a `select!` shutdown arm should break out of its loop.
///
/// The long-running relay/sweep loops (PG listener, alarm reconciler, live poller) all
/// share one shutdown-arm shape: `res = <rx>.changed() => { if <this> { break } }`. The
/// arm breaks when the watch sender was dropped (`changed()` → `Err`, i.e.
/// `changed_err == true`) OR when the current shutdown value is `true`
/// (`currently_shutting_down == true`). Breaking on a dropped sender is what avoids
/// busy-spinning on the immediately-ready `Err` (REQ-OBS-068 / REQ-SCHED-065.3 / F-47).
///
/// Extracting the decision into one pure fn lets the break condition be verified by a
/// behavioral truth-table test rather than a brittle source-text scan.
pub fn shutdown_arm_should_break(changed_err: bool, currently_shutting_down: bool) -> bool {
    changed_err || currently_shutting_down
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC-OBS-068 (behavioral): the shared shutdown-arm break decision. A live sender with
    /// the shutdown value still `false` keeps looping; a dropped sender (`changed()` → Err)
    /// OR an observed shutdown value breaks — the exact truth table the three `select!` arms
    /// rely on (REQ-OBS-068 / F-47).
    #[test]
    fn shutdown_arm_should_break_truth_table() {
        // (changed_err, currently_shutting_down) → expected break?
        assert!(!shutdown_arm_should_break(false, false)); // running, sender alive → keep going
        assert!(shutdown_arm_should_break(false, true)); // shutdown value observed → break
        assert!(shutdown_arm_should_break(true, false)); // sender dropped → break (no busy-spin)
        assert!(shutdown_arm_should_break(true, true)); // both → break
    }
}
