# Row-Level Security Policies — crypto-collector

_Manually maintained — not auto-updated by the `moai-domain-db-docs` hook._

## Current state

This project defines **no Row-Level Security policies**. None of the 15 migration files contain
`CREATE POLICY`, `ALTER TABLE ... ENABLE ROW LEVEL SECURITY`, or any RLS-related DDL (verified by
reading all migration files in `migrations/`).

This is consistent with the project's architecture:

- Single shared PostgreSQL schema, no multi-tenancy (per `CLAUDE.md`: "Multi-tenant: none / single
  shared schema").
- Access is entirely through the application layer (`sqlx` connection pool in `src/db/pool.rs`),
  not through per-user or per-tenant database roles.
- The REST API (`src/api/`) is the sole access boundary; any authorization/access-control logic
  lives there, not in the database.

## If RLS is introduced later

Update this file with:

- The table(s) RLS is enabled on
- The policy definitions (`CREATE POLICY ...`) and which roles/predicates they apply to
- The migration file(s) that introduced them
- Whether `FORCE ROW LEVEL SECURITY` is set (affects table owners too)
