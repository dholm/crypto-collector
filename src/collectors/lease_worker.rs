//! Shared lease-queue worker scaffold (SPEC-REFACTOR-001 M3, F-53b).
//!
//! `collection_queue` and `backfill` both drive the identical claim → heartbeat →
//! work → complete/release lifecycle over a `FOR UPDATE SKIP LOCKED` lease queue. That
//! scaffolding was duplicated verbatim (down to the guarded shutdown `select!` arms and the
//! spawned heartbeat task). This module holds the **single** parameterized implementation
//! (REQ-REFACTOR-030); each worker supplies only what genuinely differs — its item type, its
//! claim/heartbeat SQL wrappers, its work step, and its terminal-transition (finalize) step —
//! via closures.
//!
//! # Post-Phase-1 semantics preserved
//!
//! The scaffold keeps the corrected shutdown-arm behavior (a dropped watch sender breaks the
//! loop rather than busy-spinning — [`crate::shutdown::shutdown_arm_should_break`],
//! REQ-SCHED-065.3) and threads each worker's transient/permanent/soft-skip classification
//! through the injected `work` + `finalize` closures unchanged (REQ-REFACTOR-032).
//!
//! # Watch-based heartbeat stop (REQ-REFACTOR-031)
//!
//! The spawned heartbeat is stopped via a `tokio::sync::watch` signal ([`HeartbeatHandle::stop`])
//! rather than `abort()`, so the heartbeat task terminates gracefully at its next `select!`
//! boundary instead of being cancelled mid-poll.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration as StdDuration;

use anyhow::Result;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{error, info};

use crate::shutdown::shutdown_arm_should_break;

/// A boxed `Send` future — the closure-return shape used across the lease-queue scaffold.
/// Factored into an alias so the closure signatures stay under `clippy::type_complexity`.
pub type LeaseFut<O> = Pin<Box<dyn Future<Output = O> + Send>>;

/// A claimed lease-queue row the scaffold can heartbeat and process. Implemented by
/// `ClaimedQueueItem` (collection_queue) and `ClaimedChunk` (backfill).
pub trait LeaseItem {
    /// The row's primary key, used to key the heartbeat renewal.
    fn lease_id(&self) -> i64;
}

/// Outcome of one heartbeat renewal attempt, classified so the shared heartbeat loop can
/// decide whether to keep beating, stop (the fence fired), or keep beating despite a
/// transient error — mirroring the pre-refactor `Ok(true)` / `Ok(false)` / `Err` arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatStep {
    /// Lease renewed — keep beating.
    Renewed,
    /// The `claimed_by` fence fired (another replica owns the lease) — stop the heartbeat.
    FencedOut,
    /// A transient DB error (already logged by the beat closure) — keep beating.
    Errored,
}

/// Whether the scaffold should pause (`idle_sleep`) before claiming the next item. A pause
/// follows backpressure / a retryable-failure release so a provider cooldown does not become
/// a tight loop against the DB (REQ-SCHED-062); success and permanent-fail proceed immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseCycleOutcome {
    /// Proceed straight to the next claim.
    Continue,
    /// Pause for `idle_sleep` before the next claim.
    PauseBeforeNextClaim,
}

/// Handle to a spawned heartbeat task, stopped via a watch signal rather than `abort()`.
pub struct HeartbeatHandle {
    stop_tx: watch::Sender<bool>,
    join: JoinHandle<()>,
}

impl HeartbeatHandle {
    /// Signal the heartbeat task to stop via the watch channel (NOT `abort()`) and await its
    /// graceful termination. Returns the task's join result — `Ok(())` confirms the task ran
    /// to its own completion (a watch-signalled break) rather than being cancelled, which is
    /// how a caller/test distinguishes the watch-stop from an `abort()` (REQ-REFACTOR-031).
    pub async fn stop(self) -> Result<(), tokio::task::JoinError> {
        // A send error only means the task already exited (e.g. it fenced itself out) — benign.
        let _ = self.stop_tx.send(true);
        self.join.await
    }
}

// @MX:NOTE: [AUTO] spawn_heartbeat — the single lease-heartbeat lifecycle; watch-based stop (REQ-REFACTOR-031)
//   Replaces the two duplicated `tokio::spawn(heartbeat loop) + hb_handle.abort()` scaffolds.
//   The stop signal is a `watch` channel, so the task terminates at its next select! boundary
//   instead of being cancelled mid-poll; a fenced-out beat still breaks the task on its own.
// @MX:SPEC: SPEC-REFACTOR-001 REQ-REFACTOR-030 REQ-REFACTOR-031
/// Spawn a heartbeat task that calls `beat` on each interval tick, stoppable via the returned
/// [`HeartbeatHandle`]. The task terminates when (a) [`HeartbeatHandle::stop`] signals it, or
/// (b) a `beat` returns [`HeartbeatStep::FencedOut`] (the `claimed_by` fence fired).
pub fn spawn_heartbeat(
    interval_secs: u64,
    mut beat: impl FnMut() -> LeaseFut<HeartbeatStep> + Send + 'static,
) -> HeartbeatHandle {
    let (stop_tx, mut stop_rx) = watch::channel(false);
    let join = tokio::spawn(async move {
        let mut interval = tokio::time::interval(StdDuration::from_secs(interval_secs));
        loop {
            tokio::select! {
                // Stop requested (or the sender was dropped): terminate gracefully — NOT abort().
                _ = stop_rx.changed() => break,
                _ = interval.tick() => {
                    match beat().await {
                        HeartbeatStep::Renewed | HeartbeatStep::Errored => {}
                        HeartbeatStep::FencedOut => break,
                    }
                }
            }
        }
    });
    HeartbeatHandle { stop_tx, join }
}

// @MX:ANCHOR: [AUTO] run_lease_worker — the single claim/heartbeat/complete/release lease-queue scaffold
// @MX:REASON: fan_in >= 2 — collection_queue::run_collection_queue_worker and
//             backfill::run_backfill_worker both delegate their entire worker loop here (F-53b).
//             It owns the guarded shutdown select! arms (a dropped watch sender breaks the loop,
//             never busy-spins — REQ-SCHED-065.3) and the watch-based heartbeat stop
//             (REQ-REFACTOR-031). The transient/permanent/soft-skip classification stays in each
//             worker's injected `work`/`finalize` closures, so this scaffold must NOT reintroduce
//             a private per-worker copy of the loop (the F-53b duplication this consolidation removes).
// @MX:SPEC: SPEC-REFACTOR-001 REQ-REFACTOR-030 REQ-REFACTOR-031 REQ-REFACTOR-032 SPEC-SCHED-001 REQ-SCHED-065
/// Run the shared lease-queue worker loop.
///
/// Per iteration: check shutdown → `claim` an item (idle on empty, bounded-pause on error, both
/// racing shutdown) → spawn a heartbeat keyed on `item.lease_id()` → run `work` → stop the
/// heartbeat via its watch signal (before the terminal transition, matching the pre-refactor
/// `hb_handle.abort()` ordering) → run `finalize` (the worker's complete/release/fail step) →
/// pause before the next claim iff `finalize` asked to. `beat` is cloned per item.
#[allow(clippy::too_many_arguments)]
pub async fn run_lease_worker<T, R>(
    worker_name: &'static str,
    claimed_by: String,
    heartbeat_interval_secs: u64,
    idle_sleep: StdDuration,
    mut shutdown: watch::Receiver<bool>,
    mut claim: impl FnMut() -> LeaseFut<Result<Option<T>, sqlx::Error>>,
    beat: impl Fn(i64) -> LeaseFut<HeartbeatStep> + Clone + Send + 'static,
    mut work: impl FnMut(T) -> LeaseFut<(T, R)>,
    mut finalize: impl FnMut(T, R) -> LeaseFut<LeaseCycleOutcome>,
) -> Result<()>
where
    T: LeaseItem + Send + 'static,
    R: Send + 'static,
{
    info!("{worker_name}: started (replica={claimed_by})");

    loop {
        if *shutdown.borrow() {
            break;
        }

        let item = match claim().await {
            Ok(Some(i)) => i,
            Ok(None) => {
                // Queue empty: idle until the next check or shutdown. A dropped sender
                // (`changed()` → Err) breaks the loop rather than busy-spinning (REQ-SCHED-065.3).
                tokio::select! {
                    res = shutdown.changed() => { if shutdown_arm_should_break(res.is_err(), *shutdown.borrow()) { break; } }
                    _ = tokio::time::sleep(idle_sleep) => {}
                }
                continue;
            }
            Err(e) => {
                error!("{worker_name}: claim error: {e}");
                // Bounded pause raced against shutdown so shutdown stays prompt and a dropped
                // sender breaks the loop (REQ-SCHED-062/065.3).
                tokio::select! {
                    res = shutdown.changed() => { if shutdown_arm_should_break(res.is_err(), *shutdown.borrow()) { break; } }
                    _ = tokio::time::sleep(StdDuration::from_secs(1)) => {}
                }
                continue;
            }
        };

        // Heartbeat: renews the lease while work runs, stopped via a watch signal (REQ-REFACTOR-031).
        let id = item.lease_id();
        let beat_for_item = beat.clone();
        let heartbeat = spawn_heartbeat(heartbeat_interval_secs, move || beat_for_item(id));

        let (item, result) = work(item).await;

        // Stop the heartbeat BEFORE the terminal transition, matching the pre-refactor
        // `hb_handle.abort()` ordering (but graceful — a watch signal, not a cancel).
        let _ = heartbeat.stop().await;

        let outcome = finalize(item, result).await;

        if outcome == LeaseCycleOutcome::PauseBeforeNextClaim {
            tokio::select! {
                res = shutdown.changed() => { if shutdown_arm_should_break(res.is_err(), *shutdown.borrow()) { break; } }
                _ = tokio::time::sleep(idle_sleep) => {}
            }
        }
    }

    info!("{worker_name}: stopped");
    Ok(())
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // ── AC-REFACTOR-031a: watch-based heartbeat stop (behavioral, no DB) ──────────

    /// The heartbeat renews while running and is stopped via the watch signal, NOT `abort()`:
    /// a graceful `Ok(())` join proves the task ran to its own completion (a cancel would surface
    /// `JoinError::is_cancelled()`), and no further beats occur after `stop()`.
    #[tokio::test(start_paused = true)]
    async fn heartbeat_stops_via_watch_signal_not_abort() {
        let beats = Arc::new(AtomicUsize::new(0));
        let counter = beats.clone();
        let hb = spawn_heartbeat(1, move || {
            let counter = counter.clone();
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                HeartbeatStep::Renewed
            })
        });

        // Let several interval ticks fire under virtual time.
        tokio::time::advance(StdDuration::from_secs(3)).await;
        tokio::task::yield_now().await;
        assert!(
            beats.load(Ordering::SeqCst) >= 1,
            "heartbeat must renew the lease while the item is being worked"
        );

        // Graceful stop via the watch signal — Ok(()) proves it was not `abort()`ed.
        let join = hb.stop().await;
        assert!(
            join.is_ok(),
            "stop() must gracefully terminate the heartbeat (watch signal, not abort())"
        );

        // No further beats after the task is joined.
        let after_stop = beats.load(Ordering::SeqCst);
        tokio::time::advance(StdDuration::from_secs(10)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            beats.load(Ordering::SeqCst),
            after_stop,
            "no heartbeats may fire after stop()"
        );
    }

    /// A `FencedOut` beat terminates the heartbeat task on its own (preserving the pre-refactor
    /// `Ok(false)` → break behavior); `stop()` then joins cleanly.
    #[tokio::test(start_paused = true)]
    async fn heartbeat_terminates_itself_on_fenced_out() {
        let hb = spawn_heartbeat(1, || Box::pin(async { HeartbeatStep::FencedOut }));
        tokio::time::advance(StdDuration::from_secs(2)).await;
        tokio::task::yield_now().await;
        let join = hb.stop().await;
        assert!(
            join.is_ok(),
            "a fenced-out heartbeat must terminate on its own and join cleanly"
        );
    }

    // ── AC-REFACTOR-032a: LeaseCycleOutcome classification is distinct ────────────

    #[test]
    fn lease_cycle_outcome_variants_are_distinct() {
        assert_ne!(
            LeaseCycleOutcome::Continue,
            LeaseCycleOutcome::PauseBeforeNextClaim
        );
    }

    #[test]
    fn heartbeat_step_variants_are_distinct() {
        assert_ne!(HeartbeatStep::Renewed, HeartbeatStep::FencedOut);
        assert_ne!(HeartbeatStep::Renewed, HeartbeatStep::Errored);
        assert_ne!(HeartbeatStep::FencedOut, HeartbeatStep::Errored);
    }

    // ── AC-REFACTOR-030a: exactly ONE shared lease-queue scaffold ─────────────────
    // Source-scan guards. Each reads only the production half (before `#[cfg(test)]`) so the
    // scan never matches its own assertion-message string literals.

    #[test]
    fn exactly_one_lease_scaffold_definition() {
        // Only lease_worker.rs DEFINES the scaffold; the two workers merely call it.
        let mut defs = 0usize;
        for path in [
            "src/collectors/lease_worker.rs",
            "src/collectors/collection_queue.rs",
            "src/collectors/backfill.rs",
        ] {
            let src = std::fs::read_to_string(path).unwrap_or_else(|_| panic!("read {path}"));
            let code = src.split("#[cfg(test)]").next().unwrap_or(&src);
            // Needle split so this literal is not itself a false definition match.
            let needle = format!("pub async fn {}", "run_lease_worker");
            defs += code.matches(&needle).count();
        }
        assert_eq!(
            defs, 1,
            "exactly one shared lease-queue scaffold must be defined (REQ-REFACTOR-030)"
        );
    }

    #[test]
    fn both_workers_delegate_to_shared_scaffold() {
        for path in [
            "src/collectors/collection_queue.rs",
            "src/collectors/backfill.rs",
        ] {
            let src = std::fs::read_to_string(path).unwrap_or_else(|_| panic!("read {path}"));
            let code = src.split("#[cfg(test)]").next().unwrap_or(&src);
            let call = format!("{}(", "run_lease_worker");
            assert!(
                code.contains(&call),
                "{path} must delegate its worker loop to the shared lease-queue scaffold (REQ-REFACTOR-030)"
            );
            // Watch-based heartbeat stop (REQ-REFACTOR-031): no local heartbeat abort remains.
            let abort = format!(".{}()", "abort");
            assert!(
                !code.contains(&abort),
                "{path} heartbeat must stop via the shared watch signal, not abort (REQ-REFACTOR-031)"
            );
        }
    }

    #[test]
    fn scaffold_guards_dropped_sender() {
        // The guarded shutdown arms (REQ-SCHED-065.3) live in the scaffold after the M3 extraction.
        let src = std::fs::read_to_string("src/collectors/lease_worker.rs")
            .expect("read lease_worker.rs");
        let code = src.split("#[cfg(test)]").next().unwrap_or(&src);
        let guard = format!("shutdown_arm{}", "_should_break");
        assert!(
            code.contains(&guard),
            "the shared scaffold's select! arms must adopt shutdown_arm_should_break (REQ-SCHED-065.3)"
        );
        let unguarded = format!("_ = shutdown{}", ".changed()");
        assert!(
            !code.contains(&unguarded),
            "no un-captured shutdown.changed() arm may remain — the result must be bound and is_err()-guarded"
        );
    }
}
