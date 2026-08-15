# ADR-0001: Event Ledger as Source of Truth, Materialized Tables for Query

**Status:** Accepted
**Date:** 2026-08-15
**Constitution articles touched:** 6 (state persistence), 10 (config revision recording)

---

## Finding

`docs/system-analysis.md` §C.3 records that Article 6 has no implementation in any reference
project: `daily_rust` persists one JSON file per task run (904 scattered state files in DDP,
per 系統開發需求書 §44/§67), which cannot answer "what happened, how far did it get, can it be
retried" after a restart without scanning the filesystem. `agent-dashboard` demonstrates the
opposite pattern — a queryable SQLite event table — but only for workflow/step events, not for
lease or verification lifecycle.

## Context

- 系統開發需求書 §42 requires an append-only `events` table with a fixed event vocabulary.
- §43 requires materialized tables (`tasks`, `runs`, `attempts`, `leases`, …) for query.
- §61 requires replay tests: replaying the ledger must reproduce identical materialized state.
- Article 6 requires that restart recovery be possible at all.
- `agent-dashboard/crates/agentflow-store/src/event_store.rs` proves the SQLite+`rusqlite`
  approach works with no external service dependency.

The tension: a single table cannot serve both "immutable history" and "fast current-state
query" without either losing auditability or losing performance.

## Decision

The event ledger is the sole source of truth. Materialized tables are a derived cache that can
be dropped and rebuilt from the ledger at any time.

Concretely:

- Writes append an `EventEnvelope` (UUIDv7 id, `schema_version`, `trace_id`, `occurred_at`) to
  `events`. This table is append-only; it has no `UPDATE` or `DELETE` path in the API surface.
- The same transaction applies the event's effect to the materialized tables.
- `pearl event replay` rebuilds materialized state from `events` alone and is asserted to
  produce state identical to the incrementally-maintained tables.
- SQLite runs in WAL mode: concurrent readers, single writer, ACID, no external dependency.

## Alternatives Considered

| Alternative | Why rejected |
|---|---|
| Materialized tables as source of truth, events as audit log | Audit log can drift from state with no detection; replay cannot be a correctness check because there is nothing to compare against |
| JSON files per task (daily_rust pattern) | No query capability; DDP's 904-file sprawl is the exact failure being migrated away from |
| Postgres | Adds an operational dependency for a single-node autonomous agent; SQLite/WAL already satisfies the durability requirement |
| Event log only, derive state on every read | Read cost grows without bound; §43 explicitly asks for queryable tables |

## Consequences

### Positive

- Article 6 becomes testable rather than aspirational: replay equality is a mechanical check.
- Corrections are expressed as new events, so history is never rewritten (§42).
- `trace_id` correlation gives a complete per-task narrative for free.

### Negative / accepted cost

- Every state change costs two writes (ledger + materialized) inside one transaction.
- Event vocabulary changes require a `schema_version` bump and a replay-compatibility test.
- Materialized-table schema changes require a rebuild, not an ad-hoc migration.

### Migration impact

Phase 1 only introduces the kernel; nothing in DDP is switched over yet. DDP's existing state
files remain authoritative for DDP until Phase 8 (§67) migrates them category by category.
Rollback is to stop using the kernel — no production behaviour depends on it during Phase 1.

## Verification

- [x] Test / check name: `tests/replay/replay_equivalence.rs` — rebuild-from-ledger equals
      incrementally maintained state
- [x] Test / check name: `pearl-events` append-only unit test — no public mutation path
- [x] Constitution check updated: Article 6 row in `CONSTITUTION.md` enforcement table points
      at the replay test
- [x] Replay or determinism impact assessed: replay is the determinism check for this ADR

## Promotion

- [x] Verification passed
- [x] Reviewed
- [x] Landed
