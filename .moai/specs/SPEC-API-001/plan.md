# Implementation Plan — SPEC-API-001 (REST API & OpenAPI v3.1)

Schema: [../SPEC-DB-001/spec.md](../SPEC-DB-001/spec.md). Collection:
[../SPEC-SCHED-001/spec.md](../SPEC-SCHED-001/spec.md),
[../SPEC-PROV-001/spec.md](../SPEC-PROV-001/spec.md).
Research: [../SPEC-PROV-001/research.md](../SPEC-PROV-001/research.md) (§5 versioning,
§4.6 keyset). Methodology: greenfield TDD — handler tests with `axum-test` + a
doc-parity string-match test.

## Technical Approach

Build an Axum router under `/v1` with two resource groups (coins, markets) and the
read sub-resources. Reuse the `ticker-collector` keyset cursor design (`encode_keyset_
cursor`/`decode_keyset_cursor`, base64url-no-pad JSON of the ordering key). All reads
are keyset-paginated and time-range filterable. A shared error type maps to the uniform
JSON error body. The OpenAPI document is authored by hand (as in ticker) and guarded by
a doc-parity test.

## Files (anticipated; confirmed in run phase)

| File | Change |
|---|---|
| `src/api/mod.rs` (new) | `/v1` router assembly, shared extractors, error mapper. |
| `src/api/cursor.rs` (new) | Keyset key types + `encode/decode_keyset_cursor` (ported from ticker `api/v2/cursor.rs`). |
| `src/api/coins.rs` (new) | coins management + search handlers. |
| `src/api/markets.rs` (new) | markets management + search handlers. |
| `src/api/quotes.rs` (new) | spot latest + history reads. |
| `src/api/candles.rs` (new) | candles read (interval validation + keyset). |
| `src/api/metadata.rs` (new) | coin metadata latest + as-of reads. |
| `src/api/coin_market.rs` (new) | coin market aggregates latest + history. |
| `src/api/derivatives.rs` (new) | derivatives latest + history. |
| `src/api/dto.rs` (new) | request/response DTOs with `Decimal` fields + serialization convention. |
| `api/crypto-collector.yaml` (new) | OpenAPI 3.1.0 document (implementation deliverable). |

## Milestones (priority-ordered, no time estimates)

### Milestone 1 — keyset cursor + error model (Priority High)
- RED: cursor round-trips per key type; invalid base64/JSON → error→400 (REQ-API-070/071);
  uniform error body for 400/404/422/500 (REQ-API-074).
- GREEN: port cursor helpers; shared error mapper.

### Milestone 2 — coins management (Priority High)
- RED: POST new → 201 + enqueue; POST existing → 200 (idempotent); list keyset-
  paginated; get/patch/delete; search capped (REQ-API-010..013).
- GREEN: handlers + registry queries + enqueue hook (SPEC-SCHED-001).

### Milestone 3 — markets management (Priority High)
- RED: POST `(base,quote,venue?)` → 201 + enqueue + backfill; idempotent 200; uniqueness
  with NULL venue; list/filter/get/patch/delete; search (REQ-API-020..023).
- GREEN: handlers + registry queries respecting `COALESCE(venue,'')` uniqueness.

### Milestone 4 — read endpoints (Priority High)
- RED: quotes latest/history (REQ-API-030/031); candles with required+validated
  interval and nullable volume (REQ-API-040..042); metadata latest + as-of
  (REQ-API-050); coin market latest/history (REQ-API-051/052); derivatives latest/
  history (REQ-API-060/061). All histories keyset-paginated with `next_cursor`.
- GREEN: read handlers + partition-pruned, keyset queries (`btree(key, ts DESC)`).

### Milestone 5 — precision serialization + limits (Priority High)
- RED: `Decimal` serialises losslessly (tiny/huge values) per the chosen convention
  (REQ-API-073, OR-API-2); `limit` validated/capped → 400 out of range (REQ-API-072).
- GREEN: serialization newtype/helper + limit validation.

### Milestone 6 — OpenAPI document + parity (Priority Medium)
- RED: doc-parity test asserts every endpoint operationId and schema name appears in
  `api/crypto-collector.yaml` (mirrors ticker `openapi_spec_contains_*`) (REQ-API-002/003).
- GREEN: author the OpenAPI 3.1.0 document covering all paths/schemas/components.

## Risks

- **Keyset correctness over partitioned tables.** The cursor key must match the index
  ordering (`ts DESC, id`) so pages are stable and partition-pruned; a mismatch
  silently skips/duplicates rows. Port the ticker keyset design verbatim in shape and
  test round-trips + ordering (REQ-API-070).
- **Doc drift.** Handlers and OpenAPI diverge without enforcement; the parity test is
  mandatory (REQ-API-003).
- **Precision in transit.** A `Decimal`→`f64`→JSON path truncates; the serialization
  convention (OR-API-2, recommend string) must be lossless and tested (REQ-API-073).
- **Idempotent registration races.** Concurrent POSTs of the same coin/market must both
  resolve to the existing record (200), relying on the registry uniqueness; test the
  race (REQ-API-011/021).
- **Read-while-degraded.** When upstream is down, reads return last-persisted data
  (SPEC-PROV-001 REQ-PROV-005), not 5xx; reads must not couple to provider liveness.

## Definition of Done

- Single `/v1` router: coins + markets management (idempotent), all three read domains,
  all list reads keyset-paginated with `next_cursor`.
- Required+validated candle interval; nullable candle volume surfaced.
- Metadata as-of reads; coin market + derivatives latest/history.
- Lossless `Decimal` serialization; validated/capped `limit`; uniform error model.
- `api/crypto-collector.yaml` (OpenAPI 3.1.0) published and guarded by a doc-parity test.
- All EARS REQ-API-001..074 covered by `axum-test` + parity tests.
- Open items OR-API-1..4 resolved or explicitly deferred with user sign-off.
