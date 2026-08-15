# ADR-0003: Declared Plans Are Persisted, and Verification Is Three-Valued

**Status:** Accepted
**Date:** 2026-08-15
**Constitution articles touched:** 2 (exactness needs a verifier), 4 (provable success), 8 (no self-certification), 9 (cancellability), 10 (single source of truth)

---

## Finding

Building the execution plane (§69, §70) surfaced four defects that shared one shape: a
declaration existed, a type existed, a test existed — and nothing connected them, so the
system reported success for work it had not done.

1. **The declared plan was discarded at submission.** `TaskSpec` accepted `capability:`,
   `assurance:` and `timeout_seconds:`. `into_submission()` validated the assurance list
   against the Article 2 gate and then dropped all three on the floor. A task could pass the
   gate by promising a verifier that no worker could ever discover, because the promise was
   never persisted.

2. **Assurance checks did not run.** `AssuranceEngine` invoked an injected closure and
   never interpreted `CheckKind`. `SchemaValidation`, `ScriptVerifier` and `TestCommand`
   were data the engine did not read, so `3/3 checks passed` could be reported without a
   schema having been validated or a verifier having been spawned.

3. **A verifier's failure to run was indistinguishable from a rejection.** With a
   two-valued `CheckOutcome`, a missing verifier script, an unreadable schema and a
   genuinely failing result all mapped to `Failed`. A failed check invites a retry; a
   verifier that cannot run will fail identically on every retry, and the task would be
   dead-lettered as if the work were wrong.

4. **The process supervisor mixed two clocks.** `spawn` recorded `started_at` from
   `Utc::now()` while `wait` compared the deadline against the injected `Clock`. Under a
   test clock, work timed out instantly or never — depending only on which side of the
   fixed instant real time happened to fall. This is how the first worker integration test
   failed: a script that exits in 40ms reported `TimedOut`.

## Context

Each of these is the same failure mode the audit had already named at the crate level:
component written, system not assembled. What made them worth an ADR rather than four
patches is that fixing them required deciding *where a claim lives*.

The Constitution is explicit that success must be provable (Article 4) and that only a
machine verifier may declare verification (Article 8). It is silent on what happens when
verification is *impossible* — and that silence was being resolved, implicitly, in favour
of whichever outcome the code happened to produce.

## Decision

**1. The plan travels with the task.** `TaskPlan { capability, assurance, timeout_seconds }`
lives in `pearl-core`, is embedded in `task.created`, and is projected into a `plan` column
on `tasks`. Following ADR-0002: a declaration that cannot be reconstructed from the ledger
is a defective event, so the plan goes in the event, not only in the projection.

A named capability is dispatched by lookup. Only an unnamed task falls back to the router's
`task_type` matching, which is a heuristic (it includes substring matching) and should not
be the primary path.

**2. Checks are performed by mechanisms, not by closures.**
`pearl_assurance::runners::RuntimeCheckRunner` validates JSON Schema, spawns verifier
scripts under the process supervisor, and runs test commands. Schema references resolve
through a retriever restricted to the schema directory, so a verification result cannot
depend on the network.

**3. Verification is three-valued.** `CheckOutcome::{Passed, Failed, Errored}`. `Errored`
means no verdict was reached. It propagates to `Verdict::Unverified` and the task lands in
`UNVERIFIED`, which is Article 2's case B: resolvable by writing a verifier or by a human
gate, and explicitly not terminal. Exit code `2` from a verifier, and `status: "error"` in a
verification-result document, both mean this.

**4. The deadline and its enforcement share one clock.** `ProcessSupervisor::spawn` takes
`&dyn Clock`. A `spawn_now` default is provided for callers with no injected clock.

**5. A synchronous worker sizes its lease to the work.** A worker that blocks on execution
cannot heartbeat mid-flight, so it claims for `2 × timeout + 30s`. Since the supervisor
enforces the timeout, the work cannot outlive the lease, and the reaper cannot hand a
running task to a second worker.

## Alternatives Considered

| Alternative | Why rejected |
|---|---|
| Derive assurance from capability manifests only, by naming convention | Leaves `assurance:` in the task spec cosmetic — the same class of defect being fixed |
| Normalise the plan into `task_assurance` rows | Read whole, written once, never queried by field; JSON in one column is honest about that |
| Keep `CheckOutcome` two-valued and treat "could not run" as failure | A broken verifier would be retried until the task dead-lettered, reported as failing work |
| Treat "could not run" as a pass with a warning | Article 8 in its purest violation: unverified work declared verified |
| Heartbeat from a background thread during execution | `StateStore` holds a `rusqlite::Connection`, which is `Send` but not `Sync`; sharing it needs a mutex the worker would hold across the whole execution |
| Cap capability timeouts at the lease duration instead | Makes a capability's declared timeout silently untrue, and 60s is too short for real work |
| Pass an absolute deadline in `CommandSpec` instead of the clock | Equivalent, but moves deadline arithmetic to every caller; the supervisor is the component that owns it |

## Consequences

### Positive

- The §70 acceptance scenario is executable end to end and asserted against the shipped
  capabilities, not fixtures: `crates/pearl-worker/tests/end_to_end.rs`.
- Nine projection tables that were DDL-only now have writers and readers.
- `UNVERIFIED` is reachable for the two distinct reasons Article 2 describes, and both are
  covered by tests.
- Windows is a supported platform: job-object tree kill, and interpreter resolution that
  does not assume `python3` and `bash`.

### Negative / accepted cost

- `EVENT_SCHEMA_VERSION` moves to 2. `plan` is `#[serde(default)]`, so a v1 ledger still
  replays into an empty plan — but a task replayed from a v1 ledger runs with no declared
  assurance, which is a silent downgrade rather than an error.
- `ProcessSupervisor::spawn` is a breaking trait change for any out-of-tree implementor.
- `steps`, `verification_results` and `runtime_health` are written directly rather than
  projected from events, so a rebuild empties them. They are diagnostics, not state; the
  alternative is a step-level event vocabulary, which is deferred.
- The worker executes one task at a time. Concurrency is a separate decision.

### Migration impact

None deployed. The `plan` column is nullable and `CREATE TABLE IF NOT EXISTS` will not add
it to an existing database — a pre-existing `pearl.db` must be rebuilt from its ledger, or
have the column added by hand. This is the second time schema evolution has needed a manual
step, and it is the argument for the `migrations/` directory §57 asks for.

## Verification

- [x] `the_acceptance_scenario_reaches_verified_success` — §70 end to end, including the
      full expected event sequence and replay equivalence of the plan
- [x] `a_task_demanding_exactness_with_no_verifier_becomes_unverified` — Article 2 case B
- [x] `a_verifier_that_cannot_decide_leaves_the_task_unverified` — the `Errored` path
- [x] `a_verifier_that_rejects_the_result_fails_the_task` — the `Failed` path stays distinct
- [x] `a_capability_that_is_not_permitted_never_runs` — no run is opened at all
- [x] `a_task_abandoned_by_a_dead_worker_is_reclaimed_and_completed_by_another` — §34
- [x] `kill_tree_reclaims_the_entire_process_tree` — Article 9 on Windows, three generations
      confirmed dead through win32 rather than through the code under test
- [x] `a_conforming_document_passes_the_real_schema` and
      `a_cross_referenced_schema_resolves_without_the_network` — schema validation is real
- [x] `the_repository_capabilities_all_resolve` — every shipped manifest names a script that
      exists
- [x] `the_declared_plan_survives_into_the_submission` — the defect this ADR opens with
- [x] Constitution check passes on `capabilities/` (4 manifests, exit 0)
- [x] Replay impact assessed: `plan` is in the event; `checkpoints` project from
      `checkpoint.committed`; `runtime_health` added to the projection clear-list, which it
      was missing from

## Promotion

- [x] Verification passed
- [ ] Reviewed
- [ ] Landed
