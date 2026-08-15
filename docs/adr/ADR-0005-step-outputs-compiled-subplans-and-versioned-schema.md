# ADR-0005: Step outputs, compiled sub-plans, and a versioned schema

**Status:** Accepted
**Date:** 2026-08-15
**Constitution articles touched:** 1 (determinism first), 2 (exactness needs a verifier), 6 (history is immutable), 8 (only a verifier certifies), 10 (single source of truth), 12 (architecture changes are recorded)

---

## Finding

Three gaps named in ADR-0003 and ADR-0004 as deliberately unfinished turned out to share a
shape: each was a place where the system's *declared* behaviour and its *representable*
behaviour had drifted apart.

1. **Data could not flow between steps, and not for want of trying.** `StepExecutor` was
   `Box<dyn Fn(&PlanStep) -> StepOutcome>`. The step function was handed the step and nothing
   else, so a predecessor's output was not merely unavailable — it was unreachable from inside
   the closure. `capabilities/workflows/example.score-twice.yaml` therefore ran the same
   self-sufficient capability twice and explained in a comment why it could not do anything more
   interesting. Worse, `RuntimeStepExecutor` already discarded the material: on success it did
   `structured_output.map(|v| v.to_string())`, stringifying JSON that had just been parsed.

2. **§40 required two workflow forms and one existed.** The declarative form was complete. The
   dynamic form — `Planner → sub-plan → Compiler → Execution` — had no representation at all: no
   step could return a plan, and if one had, nothing would have compiled it. The risk this
   creates is specific. §31 forbids the Executor from changing policy, expanding its tools,
   ignoring a dependency or adding a side effect, and a plan arriving as text from something
   reasoning is an attempt at all four at once.

3. **The schema could gain tables but not columns.** `pearl-state` and `pearl-events` each
   applied a `const SCHEMA` of `CREATE TABLE IF NOT EXISTS` statements on every open. Against an
   existing table those statements do nothing whatsoever, so the two column additions already
   shipped — `tasks.plan` in ADR-0003 and the schedule's task-spec columns in ADR-0004 — were
   only survivable by deleting the database. For a system whose first Article of architecture is
   that the ledger is truth, "delete the database" is not an upgrade path.

## Context

- §41 already said only a *committed* checkpoint licenses the next step, and `checkpoints` already
  had an unused `payload` column. The durability half of data flow was designed and unwired.
- `AssuranceStep` already carries a `serde_json::Value` and derives `Eq`, so adding JSON fields
  to `PlanStep` costs nothing in trait bounds. (An earlier draft of this ADR claimed `Value` is
  only `PartialEq` and dropped `Eq` from four types on that basis. The claim was wrong —
  `serde_json::Value` implements `Eq` — and the derives were restored.)
- `CompilerConfig` already carried `known_capabilities` and `verified_steps`; the §30 gate a
  dynamic sub-plan needs is the gate that already existed, not a new one.
- Databases exist in three shapes in the wild: v1 (before either column), interim (columns
  present, no version recorded, written by the PR #2/#3 builds), and current. A migration runner
  that only handled the first and third would fail on the second with "duplicate column name" —
  and the second is the shape on every machine that has run this repository recently.
- Article 6 forbids rewriting history, so migrations may add to `events` but never alter it.

## Decision

**1. A step's output is data, and the executor carries it.** `StepExecutor` becomes
`Fn(&PlanStep, &StepOutputs)`. `StepOutcome::Success` keeps both the text a step printed and that
text parsed, `Checkpoint` accumulates the outputs, and the CLI commits each one into the
checkpoint payload and restores it on `--resume`. A step declares what it needs in two fields:
`input` for constants and `input_from` for `steps.<id>.output[.path]` references. Reading a step
is depending on it, so the compiler refuses a reference to a step absent from `depends_on`.

**2. A proposed plan is compiled, never executed.** `StepRole::Plan` marks a step whose output is
a plan. `PlanProposal` — with `deny_unknown_fields` — is the only way that output becomes steps,
and those steps go through the same `Planner` and `PlanCompiler`, against the same capability set,
as a workflow written by hand. Sub-plan ids are namespaced by the planning step, the parent's
budget is shared, nesting is depth-limited, and the whole facility is off unless a caller passes
`ExecutorConfig::with_dynamic_planning`.

**3. The schema has a version, recorded per component.** `migrations/{ledger,projections}/*.sql`
are applied in order, one transaction each, recorded in a `schema_migrations` table. Each
migration declares the columns it adds so the runner can check `PRAGMA table_info` before running
it: all present means the goal is already met and the migration is *adopted*; some present is an
error; none present means run it. A database recording a version this build does not know is
refused rather than opened.

## Alternatives Considered

| Alternative | Why rejected |
|---|---|
| One `input` map, with strings starting `steps.` treated as references | The map would have to guess. `steps.score.output` is a legitimate literal to pass a capability, and the safe-looking guess — treat it as a literal — turns a typo into a step that runs on the wrong data and reports success. Two fields cost one word and remove the guess. |
| Allow a reference to any *transitively* reachable step | Ordering would still be correct, but `depends_on` would stop being a complete statement of what a step needs, so a reader could not tell where a step's input came from without walking the graph. Requiring the edge is cheap and the compiler names the one to add. |
| Keep `StepOutcome::Success { output: String }` and re-parse when resolving a path | Round-trips through a string a value that was already parsed, and leaves no way to distinguish "the step emitted an object" from "the step printed something that happens to parse". The error message for a missing field would have to be vaguer than it needs to be. |
| Persist step outputs in the `steps` projection instead of the checkpoint payload | `steps` records what ran, for humans. §41 makes the checkpoint the unit that licenses the next step, so when the next step consumes an output, the output belongs to the thing being committed. Splitting them means a resume could find one without the other. |
| Let a planning capability's output be executed directly, trusting the prompt | §29's prohibitions would then be enforced by asking politely. `deny_unknown_fields` makes a proposal carrying a `command` key fail to parse, which is a mechanical guarantee rather than a hope. |
| Infer the planning role from the capability id or type | The role decides whether text is *data* or *instructions*. Nothing should have to guess that from a naming convention; a capability renamed would silently change what the executor does with its output. |
| Let a sub-plan run without an explicit opt-in, inheriting the parent's registry | Which capabilities a planner may draw from is a policy decision. Inheriting it means the decision gets made by whoever adds a `plan` step, which is exactly the delegation §31 forbids. |
| Give a sub-plan its own budget | Replanning would become a way to buy more work than the run was authorised to do, and the parent's `max_steps` would describe only the part of the plan that was written down. |
| Namespace sub-plan ids only on collision | A collision with a *pending* parent step is undetectable at expansion time and makes the executor skip that step as already completed. Unconditional prefixing has no such case, and the resulting id says which planner produced it. |
| `PRAGMA user_version` for schema versioning | One integer per file, and there are two independently owned schemas in the file. It also records no history; a table can say when each migration landed, which is what one wants when a database misbehaves. |
| One migration list covering ledger and projections together | `pearl-events` must be able to open a database without projections. A single list would make a projection migration a precondition for reading history. |
| Tolerate any "duplicate column name" error and continue | Blanket tolerance would also accept a column of the wrong type, and would hide a genuinely broken migration. Declaring the columns turns the tolerance into a checkable post-condition. |
| Baseline the migration set at the *current* schema | The v1 databases would then be recorded as current while lacking two columns — the exact failure the runner exists to prevent, made invisible. The baseline has to be the oldest shipped shape. |
| Recreate `schedules` rather than `ALTER` it, to make `spec_path` genuinely `NOT NULL` | Rows predating the column have no spec path to supply, so recreation means discarding schedules an operator created. `DEFAULT ''` keeps the row and the daemon already reports an unreadable spec without stopping. |

## Consequences

### Positive

- A workflow can be a pipeline. The shipped example now scores a task and verifies *that score*,
  which is the shape every real workflow has and none could previously express.
- Article 8 gets easier to honour: a verifier can be handed the exact document under test rather
  than re-deriving it, so generation and verification stay separate capabilities.
- §40 is satisfiable without weakening §30 or §31, because the dynamic path reuses the gate
  rather than paralleling it.
- Adding a column is a file, not a migration plan plus a support note.
- `pearl doctor` answers "which schema is this database on", which was previously unanswerable
  from outside the process.

### Negative / accepted cost

- `StepExecutor` is a breaking signature change for any external caller.
- Checkpoint payloads now hold whole step outputs, so the projection grows with output size. It is
  a cache and can be rebuilt, but it is no longer negligible.
- `pearl-planner` and `pearl-workflow` gain a `serde_json` dependency.
- Dynamic planning has no CLI surface yet, so the feature is only reachable from library callers.
  This is deliberate — the flag needs a way to express a capability allow-list — but it means the
  shipped dynamic example validates and cannot be run.
- A `plan` step is classified P1, so a workflow declaring `max_llm_calls: 0` will not compile one.
  That is intended, and it will surprise someone.

### Migration impact

No action required. Every existing database is brought up to date on the next open: v1 gains both
columns with its rows intact, interim adopts the two migrations it already satisfies, current is
untouched. The rollback path is that an older build refuses to open a database this one migrated,
loudly, rather than misreading it — so rolling back means restoring a copy taken beforehand,
which is why the refusal exists.

Behaviour changes: `pearl workflow run` prints a one-line summary per step instead of the debug
form of the outcome; `steps.description` holds that summary rather than `format!("{:?}")`; the
score-twice example has three steps instead of two.

## Verification

- [x] Test / check name: `crates/pearl-state/tests/migrations.rs` — 7 tests covering all five
      database shapes, including `a_v1_database_is_upgraded_in_place_rather_than_rebuilt` and
      `an_interim_database_adopts_the_migrations_whose_columns_it_already_has`
- [x] Test / check name: `crates/pearl-executor/tests/dynamic_planning.rs` — 9 tests covering the
      expansion loop, including `a_refused_plan_fails_the_run_and_says_why` and
      `a_planning_step_in_a_run_that_did_not_enable_planning_fails_rather_than_being_ignored`
- [x] Test / check name: `pearl_planner::proposal` — `a_proposal_asking_for_more_than_steps_is_refused`
      is the mechanical form of §29
- [x] Test / check name: `apps/pearl-cli/tests/cli.rs::workflow_run_executes_the_steps_and_records_them`
      asserts the verifier's verdict names keys that came from the upstream step, so the data flow
      is confirmed by its effect rather than by inspection
- [x] Constitution check updated: no new checks needed; `pearl constitution check` passes on
      `capabilities/` (6 manifests, including the new `agent.propose-plan`) and
      `applications/ddp/capabilities/` (19)
- [x] Replay or determinism impact assessed: no new event types, so `EVENT_SCHEMA_VERSION` stays
      at 2. `schema_migrations` is deliberately excluded from `PROJECTION_TABLES`, verified by
      `a_rebuild_does_not_forget_which_schema_it_is_rebuilding_into`. Migration 0001 is the v1
      baseline and every statement in it is `IF NOT EXISTS`, so replaying it over any existing
      database is a no-op.

## Promotion

- [x] Verification passed — 744 tests across 60 binaries; clippy `-D warnings` clean; fmt clean
- [x] Reviewed
- [x] Landed

---

> Per Constitution Article 12, an architecture-level change requires this chain to be
> complete before promotion: Finding → Proposal → ADR → Verification → Promotion.
