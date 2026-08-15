# ADR-0002: Events Must Carry Everything Needed to Rebuild State

**Status:** Accepted
**Date:** 2026-08-15
**Constitution articles touched:** 6 (state persistence)

---

## Finding

The replay equivalence test (`crates/pearl-state/tests/replay.rs::replay_reproduces_task_state_exactly`)
failed on its first run. Rebuilding from the ledger produced tasks whose `QualitySpec`
differed from the live projection:

```text
live:     QualitySpec { exactness_required: true, deterministic_generation: true,  deterministic_verification: true  }
replayed: QualitySpec { exactness_required: true, deterministic_generation: false, deterministic_verification: false }
```

Cause: `PearlEvent::TaskCreated` recorded only `exactness_required: bool`. The other two
quality flags existed in the `tasks` table but were never written to the ledger, so replay
had nothing to reconstruct them from and fell back to `false`.

The consequence was worse than a cosmetic diff. `deterministic_verification` is an input to
the Exactness Gate (Article 2). A rebuilt task would have been evaluated under a *different*
gate verdict than the same task before the rebuild — a task that could legitimately reach
`VERIFIED_SUCCESS` would, after a recovery, be blocked as unverifiable.

## Context

ADR-0001 asserts the ledger is the source of truth and projections are a droppable cache.
That assertion is only sound if every field of a projection is derivable from events. The
original `TaskCreated` shape violated this silently: nothing in the type system or the
schema required an event to be *sufficient*, only well-formed.

An initial patch attempt preserved the missing columns with
`COALESCE((SELECT ... FROM tasks WHERE task_id = ?), 0)` during projection. This was
rejected on inspection: reading the table being rebuilt makes replay depend on the cache it
is supposed to regenerate, which is circular and produces different results depending on
whether the table was cleared first.

## Decision

Events carry complete state, not summaries. `PearlEvent::TaskCreated` now embeds the full
`QualitySpec` rather than a single derived flag.

The general rule this establishes: **if a projection column cannot be reconstructed from
events alone, the event is defective, not the projection.** Fix the event.

## Alternatives Considered

| Alternative | Why rejected |
|---|---|
| `COALESCE` from the existing row during projection | Circular: replay would read the cache it rebuilds; result depends on clear-first ordering |
| Store `QualitySpec` in a separate side table outside the ledger | Reintroduces state that survives outside the source of truth — the exact problem ADR-0001 removes |
| Accept the divergence and relax the replay test to ignore quality | Would discard the only mechanical check that Article 6 works, to preserve a bug |
| Recompute quality from `precision_class` | Not equivalent; precision class and verifiability are independent (a P1 task may or may not have a verifier) |

## Consequences

### Positive

- Replay equivalence now holds for the full `TaskRecord`, including the fields that drive
  Constitution gates.
- The rule generalises: it is now a review criterion for every future event variant.

### Negative / accepted cost

- `TaskCreated` payloads are slightly larger.
- Adding a field to a projection now obliges the author to check event sufficiency, which
  is more work than adding a column.

### Migration impact

None in production — Phase 1 has no deployed ledger. Had one existed, this would have
required an `EVENT_SCHEMA_VERSION` bump with a migration that backfilled the flags from the
`tasks` table before switching over.

## Verification

- [x] Test / check name: `replay_reproduces_task_state_exactly` — now passes and would fail
      again if any `TaskRecord` field became non-derivable
- [x] Test / check name: `projections_can_be_dropped_and_recovered` — wipes projections and
      restores from ledger alone
- [x] Test / check name: `replay_is_idempotent` — three consecutive rebuilds agree
- [x] Constitution check updated: Article 6 row already points at the replay tests
- [x] Replay or determinism impact assessed: this ADR exists because of that assessment

## Promotion

- [x] Verification passed
- [x] Reviewed
- [x] Landed
