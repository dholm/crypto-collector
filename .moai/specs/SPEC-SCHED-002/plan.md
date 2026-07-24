# SPEC-SCHED-002 — Implementation Plan

Development mode: **TDD** (RED → GREEN → REFACTOR). This plan is ordered by
decision-reversibility: the highest-change-likelihood design decisions lead, mechanical
edits follow.

## Goal

Refine the collector workers' retry / backpressure / error-classification policy per
REQ-SCHED-060..065 without touching the claim/lease/fencing machinery, without a new
migration, and without a new dependency.

---

## 1. Schema Investigation — cooldown-deferral vs fixed-sleep (drives D3, highest stakes)

**Question (from the task):** can an existing column (`lease_expires_at` / next-eligible /
`heartbeat_at`) carry a cooldown-deferral timestamp WITHOUT a new migration, so the busy-loop
fix (REQ-SCHED-062) can defer a released item until `cooldown_until` instead of a fixed sleep?

**Investigated (observed, not inferred):**

- `backfill_chunks` DDL (`migrations/0012_coin_backfill.sql:20-43`, superseding
  `0008_backfill.sql`): columns are `id, job_id, coin_id, dataset, interval, range_start,
  range_end, cursor, status, claimed_by, lease_expires_at, heartbeat_at, attempts, last_error,
  created_at, updated_at`. **No `next_eligible_at` / `not_before` / cooldown column.** The only
  index is `backfill_chunks_claim_idx ON (created_at) WHERE status='pending'` — there is **no**
  lease-expired reclaim index for backfill.
- `collection_queue` DDL (`migrations/0007_collection_queue.sql:26-62`): same shape; it *does*
  have a lease-expired reclaim index, but likewise **no** dedicated deferral column.
- Backfill claim predicate (`backfill.rs:180-187`) and queue claim predicate
  (`collection_queue.rs:71-78`): a `pending` row is **immediately** claimable; `lease_expires_at`
  is consulted **only** for the reclaim branch `status IN ('claimed','running') AND
  lease_expires_at < now()`.

**Conclusion — the schema does NOT cleanly support timestamp-deferral.** The only timestamp that
gates re-claim is `lease_expires_at`, and it applies only to rows kept in `claimed`/`running`.
To defer a released chunk via `lease_expires_at` you must **not** reset it to `pending`; you
must park it in `claimed`/`running` with `lease_expires_at = cooldown_until`. That overload has
two concrete, observed collisions:

1. **`backfill-stalled` alarm collision.** `reconciler.rs:403-417` (REQ-ALARM-033 row 8b) fires
   when `status IN ('pending','claimed','running') AND updated_at < now() - stall_secs`. A
   routine multi-minute upstream cooldown longer than `stall_secs` would leave a parked chunk's
   `updated_at` frozen and **falsely trip `backfill-stalled`** — a parked-for-cooldown chunk is
   indistinguishable from a genuinely stalled one.
2. **Un-indexed reclaim for backfill.** Parking chunks in `claimed` relies on the reclaim
   branch, which for `backfill_chunks` has **no supporting index** (only the `status='pending'`
   partial index exists).

**Decision (D3): use a bounded fixed sleep raced against the shutdown watch channel via
`tokio::select!`** — the always-available minimum the task names. This matches the existing
claim-error pause (`tokio::time::sleep(1s)` in both worker loops, `backfill.rs:728`,
`collection_queue.rs:762`) and the idle-sleep `select!` pattern already present
(`backfill.rs:720-723`, `collection_queue.rs:754-757`). Reuse the existing idle-sleep duration
(`BACKFILL_IDLE_SLEEP_MS` default 1000 ms; `collection_idle_sleep_ms`) for the soft-skip pause,
or add a dedicated `*_BACKOFF_MS` env var if a distinct value is wanted — env-only, no migration.
Timestamp-deferral is explicitly deferred to a later phase that can add a proper
`next_eligible_at` column via migration and update the `backfill-stalled` predicate.

_This design question came back **unambiguous** — resolved from evidence; no `[NEEDS
CLARIFICATION]` needed here._

---

## 2. DispatchError type (new interface — high change likelihood, drives D4 / REQ-SCHED-063)

Introduce a small `thiserror` error carrying a transient/permanent classification. `thiserror`
is already a dependency (`src/pacer/mod.rs`, `src/providers/mod.rs` use it). Two acceptable
shapes (run-phase choice):

- `enum DispatchError { Transient(String), Permanent(String) }`, or
- `struct DispatchError { kind: DispatchErrorKind, msg: String }` with
  `enum DispatchErrorKind { Transient, Permanent }`.

Permanent conditions to map (observed sites):
- `coin {id} not found` (`collection_queue.rs:405`),
- `no provider supports OHLC` / capability unsupported (`collection_queue.rs:410`,
  `backfill.rs:620`),
- unknown `(target_kind, kind)` dispatch pair (the `match` fall-through in `dispatch_item`).

Transient conditions: provider network/rate-limited failures, upsert DB errors — everything the
current `Result<bool, String>` treats uniformly.

`dispatch_item` changes signature from `Result<bool, String>` to a `Result` over the classified
error (keeping the `bool` soft-skip channel or folding soft-skip into the ok arm — run-phase
detail). The worker match arms then branch: `Permanent` → fail immediately (call the genuine-
failure path with the real `max_attempts`), `Transient` → the retry path.

**live_poller per-coin consequence (REQ-SCHED-063.4):** today `is_transient_provider_error`
only changes log level (`live_poller.rs:333-347`). Add a per-coin consequence for permanent
errors beyond logging — at minimum skip re-claiming that coin until a widened interval. The
migration-free lever is the existing `live_poll_claimed_until` marker: on a permanent per-coin
error, set the marker forward (a widened interval) instead of clearing it, so the coin is not
immediately re-due. Confirm the exact widened-interval source (e.g. `LIVE_POLL_MAX_INTERVAL_SECS`
default 3600 s, `config.rs:250`) during run-phase; a per-coin failure streak counter is an
optional enhancement, not required by the AC.

**Intended staleness trade-off (mirrors the §1 `lease_expires_at`/`backfill-stalled` analysis).**
The marker-forward defer sets `live_poll_claimed_until` forward but does NOT advance
`last_polled_at` (the transient-failure path already leaves it stale via
`LIVE_COIN_FAILURE_CLEAR_SQL`). A permanently-erroring coin therefore stays stale and MAY surface
via the aggregated `coins-stalled` alarm (REQ-ALARM-040, `reconciler.rs:192-193`/`420-422`,
driven by `last_polled_at`/`last_collected_at` staleness). This surfacing is **intended and
acceptable**: a coin that no provider can serve SHOULD become operator-visible rather than be
silently re-polled every tick. This is a deliberate consequence, not a regression — the same
"make the stuck state visible" reasoning applied to the `lease_expires_at`/`backfill-stalled`
collision analyzed in §1.

---

## 3. Release ≠ failure mechanism (drives D1 / REQ-SCHED-060)

The root cause (F-01): `attempts` is incremented at **claim** time (`CLAIM_BACKFILL_SQL:178`,
`CLAIM_QUEUE_SQL:69`), and multi-page partial releases re-claim the chunk, so `attempts` counts
pages. Two migration-free approaches:

- **(a) Dedicated administrative-release SQL (recommended).** Add `RELEASE_BACKFILL_SQL` /
  `RELEASE_QUEUE_SQL` for non-failure releases that reset the row to `pending`, write
  `last_error = NULL`, and **neutralize the claim-time increment** (e.g.
  `attempts = GREATEST(attempts - 1, 0)`), keeping `FAIL_OR_RETRY_*_SQL` for genuine failures.
  **Preserves the `CLAIM_*_SQL` `@MX:ANCHOR` verbatim** (satisfies the "preserve claim/lease/
  fencing invariants exactly" constraint) and keeps crash-reclaim bounding intact (a crash does
  not route through the release SQL, so its `+1` stays and crash-loops remain bounded).
- **(b) Relocate the increment to the failure path.** Remove `attempts = attempts + 1` from the
  claim SQL; increment only in `FAIL_OR_RETRY_*_SQL`. Cleaner conceptually, but it **modifies a
  tested `@MX:ANCHOR`** and **changes crash-reclaim semantics** (lease-expired re-claims no
  longer count toward the bound). Requires updating the SQL-shape test that asserts
  `CLAIM_*_SQL.contains("attempts + 1")` (`collection_queue.rs:943`).

The task accepts either. **Recommended: (a)** — it is the more surgical option and the one the
"preserve invariants exactly" constraint points to. Whichever is chosen, the observable AC
(REQ-SCHED-060) is what run-phase must satisfy; the SQL-shape tests must be updated to match.

Apply the identical fix to both the backfill partial-release path (`backfill.rs:795-812`, drop
the `i32::MAX, "partial"` reuse) and the queue soft-skip path (`collection_queue.rs:810-828`,
drop the `i32::MAX, "pacer_skip"` reuse).

---

## 4. Backfill pacer backpressure classification (D2 / REQ-SCHED-061)

Mirror `pacer_should_skip_queue` (`collection_queue.rs:43-47`) in the backfill worker. Today
`process_chunk` maps every `acquire_slot` error to `Err` (`backfill.rs:622-624`). Change the
acquisition to branch: `Cooldown`/`CreditExhausted` → soft-skip (release via the new release SQL
from §3, no attempt, then the bounded sleep from §1); other errors → genuine failure. Reuse the
existing `pacer_should_skip` predicate rather than adding a third copy (both existing copies are
identical — `live_poller.rs:73` and `collection_queue.rs:43`).

---

## 5. Live-poller claim LIMIT + shutdown-between-coins (D5 / REQ-SCHED-064)

- Add `LIMIT` to the `claimed` CTE in `LIVE_COIN_CLAIM_SQL` (`live_poller.rs:102-115`), bound to
  a batch that completes within the claim TTL (`LIVE_POLL_CLAIM_TTL_SECS` default 120 s,
  `config.rs:257`). Bind the limit as a parameter; add a `live_poll_claim_batch_limit()` config
  reader reading env var `LIVE_POLL_CLAIM_BATCH_LIMIT` (**default 50**, operator-overridable),
  following the existing `parse_env_i64`/`parse_env_u32` pattern (`config.rs:240-314`).
- Add a shutdown check inside the per-coin loop (`live_poller.rs:267-349`) — pass the
  `shutdown` receiver (or a cheap `*shutdown.borrow()` check) into `poll_cycle` and `break` the
  coin loop when set, so graceful shutdown does not wait for a whole batch.
- Update the `LIVE_COIN_CLAIM_SQL` SQL-shape test to assert the `LIMIT` clause.

### OR-SCHED-1 RESOLVED — `LIVE_POLL_CLAIM_BATCH_LIMIT` default = 50

Resolved at Implementation Kickoff (user decision): the default is **50**. Derivation: at the
120 s claim TTL (`LIVE_POLL_CLAIM_TTL_SECS`) and a conservative ~2 s per-coin serial cost
(dominated by the pacer min-gap; ~2 s at CoinGecko demo), 50 coins × ~2 s = ~100 s stays within
the 120 s TTL. The value is operator-overridable via the `LIVE_POLL_CLAIM_BATCH_LIMIT` env var
for deployments whose per-coin latency (provider mix, tier, network) differs. No open
clarification marker remains for this SPEC.

---

## 6. Diagnostics hygiene + collector F-47 arms (D6 / REQ-SCHED-065 — mostly mechanical)

- Administrative releases write `last_error = NULL` (folded into the §3 release SQL) — stop
  writing `"partial"` / `"pacer_skip"` over a real prior error.
- Replace `let _ = clear_coin_poll_marker(...)` (6 sites, `live_poller.rs:285-346`) with a
  `if let Err(e) = ... { warn!(...) }`.
- In the collector-worker `tokio::select!` idle/soft-skip arms, handle `shutdown.changed()`
  returning `Err` (dropped sender) by breaking. Only the collector-worker arms are in scope;
  `listener.rs` and `reconciler.rs` arms are out (lifecycle phase).

---

## Milestones (priority-ordered, no time estimates)

- **M1 (Priority High) — Release ≠ failure + pacer classification (F-01/F-02).** New release
  SQL, backfill pacer soft-skip branch, updated SQL-shape tests. The interlocking core; ship
  with its regression suite.
- **M2 (Priority High) — Busy-loop fix (F-03).** Bounded sleep raced against shutdown after
  every soft-skip/retryable-failure release in both queue workers.
- **M3 (Priority Medium) — Error classification (F-04).** `DispatchError` type; permanent
  fast-fail; live_poller per-coin permanent consequence.
- **M4 (Priority Medium) — Live-poller bounds (F-05).** Claim `LIMIT` + config reader +
  shutdown-between-coins. Blocked on OR-SCHED-1 default resolution.
- **M5 (Priority Low) — Diagnostics hygiene + F-47 collector arms (F-06).** `last_error` NULL,
  `warn!` on marker-clear, dropped-sender break.
- **M6 (Priority Low) — `@MX` tag refresh + final gate.** Update the claim/release SQL `@MX`
  comments to the new semantics; `cargo test`, `cargo clippy --all-targets --all-features -- -D
  warnings`, `cargo fmt --check` green.

## Constraints

- Preserve claim/lease/fencing invariants **exactly**: short transactions,
  `FOR UPDATE SKIP LOCKED`, `AND claimed_by = $self` fencing. Do not restructure.
- No new dependencies (`thiserror` already present). Env-var-only config; no config files. No
  new migration this phase.
- Follow existing code style, `@MX` conventions, and the SQL-shape test patterns already in
  these files.
- Externally visible behavior changes limited to retry/pacing semantics; API untouched.

## Risks

- **R1 — SQL-shape test drift.** Any changed statement (claim, new release SQL, live claim
  LIMIT) has a colocated `*.contains(...)` shape test that must be updated in lockstep, or
  `cargo test` fails. Mitigation: update shape tests within the same milestone as the SQL change.
- **R2 — Approach (b) crash-reclaim semantics.** If run-phase chooses to relocate the increment
  to the failure path, lease-expired crash re-claims stop counting toward the retry bound. This
  is acceptable (crash-loops are covered by the `backfill-stalled` alarm) but must be a
  conscious choice; approach (a) avoids it.
- **R3 — Deferred timestamp approach re-appearing.** A future editor may re-introduce
  `lease_expires_at` cooldown-parking without adding the `next_eligible_at` column; the new
  `@MX:WARN` on the release SQL is the guardrail against silently re-creating the
  `backfill-stalled` collision.

## Anti-Patterns to avoid

- Reusing `FAIL_OR_RETRY_*_SQL` with `i32::MAX` for administrative releases (the F-01 root
  cause) — use the dedicated release SQL.
- Adding a third copy of the `pacer_should_skip` predicate — reuse the existing one.
- Busy-`continue` after a soft-skip without a sleep (the F-03 root cause).
- Silencing marker-clear failures with `let _ =`.

## Cross-References

- `research/idiomatic-rust.md` §6 Category A (F-01..F-06), §6 F-47, § synthesis (F-01/F-02/F-03
  interlock — "ship as one change set with one regression suite").
- SPEC-SCHED-001 (REQ-SCHED-011/013/014/015/021/022/027 — preserved machinery + clarified bound).
- SPEC-DB-001 (`collection_queue`, `backfill_*` schema), SPEC-PROV-001 (pacer + `Provider` chain).
- REQ-ALARM-033 row 8b (`backfill-stalled`, `reconciler.rs:403-417`) — the deferral collision.
