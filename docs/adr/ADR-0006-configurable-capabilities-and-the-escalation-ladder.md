# ADR-0006: Configurable capabilities, and the escalation ladder as declarations

**Status:** Accepted
**Date:** 2026-08-16
**Constitution articles touched:** 1 (determinism first), 2 (exactness needs a verifier), 4 (success must be provable), 5 (side effects need idempotency), 8 (only a verifier certifies), 10 (single source of truth), 12 (architecture changes are recorded)

---

## Finding

Porting DDP's 33 automations natively surfaced six things that could not be settled by analogy
with existing code. Five are gaps that made the port impossible as written; one is a correction
to ADR-0005.

1. **A task could not configure its capability.** `TaskSpec.payload` is declared in
   `schemas/task-spec-v1.json` and has been since v1. The parser dropped it, so every spec that
   set one was ignored without complaint. The consequence is not abstract: DDP's knowledge
   scanner hardcoded its own repository root, and a faithful port had nowhere to put the path.
   The only alternative was hardcoding it here too, which turns one reusable capability into one
   capability per use.

2. **A worker could load exactly one capability directory.** An application's capabilities and
   the framework's shared verifiers and effects are separate trees by design —
   `applications/ddp/capabilities` holds what that application can do, `capabilities/` holds
   `verifier.task-result` and `effect.notify` that every application needs. With one directory
   the choice was to duplicate the shared capabilities into every application or to flatten
   every application into one directory.

3. **`pearl task submit` produced something inert.** It left the task in `CREATED`, which no
   worker can claim and no command could advance. A task could therefore only ever be run by
   scheduling it; a manually submitted one sat there forever.

4. **An `effect` step was counted as a model call.** `StepType::Effect` mapped to
   `PrecisionClass::P2`, and since P1–P3 mean `permits_llm_generation`, `llm_step_count` charged
   an imaginary call against `max_llm_calls`. A wholly mechanical fetch → verify → push workflow
   could not declare `max_llm_calls: 0` — the number that says "nothing here reasons".

5. **`effect.notify` could not run inside a workflow at all.** It requires an idempotency key
   (Article 5). The worker gets one from the ledger when it requests an effect; the workflow
   executor had no equivalent, so the step failed with `'idempotency_key' is required`.

6. **ADR-0005 asserted something false.** It dropped `Eq` from nine types on the stated grounds
   that `serde_json::Value` is only `PartialEq`. `AssuranceStep` has held a `Value` and derived
   `Eq` since it was written, and a probe test confirms `Value: Eq`.

## Context

- 系統開發需求書 states the ladder this port follows: `Mechanical → Workflow → LLM-assisted →
  Autonomous Agent`, with deterministic work executing mechanically, precisely verifiable work
  permitted an LLM *and* requiring mechanical verification, less determinable work resting on
  evidence, policy and assurance, and high-risk work that cannot be mechanically confirmed
  forbidden from declaring success automatically.
- Every rung of that ladder is already expressible with declarations the gates enforce. Nothing
  new was needed to represent it — but nothing recorded which automation sat where, so the
  escalation was a judgement made per file and forgotten.
- The Exactness Gate already implements the top rung: `exactness_required` with
  `deterministic_verification: false` blocks auto-completion and lands the task in `UNVERIFIED`.
- Retry semantics differ between the two execution paths. The worker requests effects through
  `StateStore::request_effect`; the workflow executor does not touch `effects` at all.

## Decision

**Payload.** `TaskSpec.payload` persists into `TaskPlan` and reaches the capability on
`PEARL_INPUT`, merged *under* the task's identity so a payload cannot claim to be a different
task — the rule the workflow executor already applies to step identity.

**Directories.** `CapabilityRegistry::load_directories` loads several trees into one registry,
and `--capabilities` / `--capabilities-path` are repeatable. A capability id defined in two
directories is an error rather than a precedence rule.

**Admission.** `pearl task submit` walks the task to `READY`, as the daemon does for a scheduled
occurrence and for the same reason: a spec's plan is already declared, so there is nothing left
to plan. `--hold` keeps the previous behaviour.

**Precision of effects.** `StepType::Effect` maps to `P0`. Precision classifies *reasoning*; what
makes an effect safe is `risk.side_effect` plus its idempotency key.

**Effect keys in workflows.** The executor derives `{task_id}/{step_id}` and applies it with the
step identity. Derived rather than clock-based, so a retried or resumed step presents the same
key while separate occurrences differ (a workflow task id already carries a timestamp).

**The ladder is written down.** `applications/ddp/ESCALATION.md` classifies all 33 automations by
rung and states which declarations express each rung, including the four groups that must not
self-certify.

**`Eq` restored** on the nine types, and ADR-0005 corrected.

## Alternatives Considered

| Alternative | Why rejected |
|---|---|
| Hardcode configuration inside each ported capability | What DDP did, and the reason its knowledge scanner was a fact about one checkout. One capability per use, none of them reusable. |
| Add `payload` to `TaskSpec` but not persist it in `TaskPlan` | Exactly the bug being fixed: the submission gate would approve a payload the worker never receives, which is how `assurance` was lost before ADR-0003. |
| Let a payload override `task_id` | Identity is a fact about the invocation. A spec that could rename itself would make the ledger's task history unreliable. |
| Precedence rules for a duplicate capability id across directories | Behaviour would depend on argument order, and an id exists to name one thing (Article 10). |
| Copy shared verifiers and effects into each application tree | Two definitions of `verifier.task-result` drift, and the drift is invisible until one application verifies differently from another. |
| Leave `task submit` at `CREATED` and add `pearl task admit` | A second command to make the first one useful. Nothing was served by the intermediate state: the plan is declared at submission, which is precisely the condition the daemon uses to skip straight through. |
| Reclassify `effect` steps as P1 instead of P0 | Would keep charging them against the LLM budget while also claiming a model may generate them. Both wrong. |
| Rename `max_llm_calls` to match what it counted | Renaming a field to fit a bug. The field's name was right and the mapping was wrong. |
| Let a workflow declare its own `idempotency_key` literal | It cannot know which occurrence it is. A literal would be identical across every run, deduplicating unrelated notifications against each other — the failure `check_effect_has_idempotency_key` exists to catch in manifests. |
| Derive the effect key from the clock | A retry would present a different key, so the retry could double the effect. Stability under retry is the whole property being bought. |
| Wire `StateStore::request_effect` into the workflow executor now | The right end state, and larger than this change: it needs the executor to hold a store, which it deliberately does not (its checkpoint sink is the only durable seam). Deferred and stated as deferred, rather than implied by the presence of a key. |
| Keep `Eq` dropped and leave ADR-0005 as written | The ADR would document a false reason for a real API regression. |

## Consequences

### Positive

- A capability can be general and a task can be specific, which is what makes one scanner serve
  every knowledge tree and one verifier serve every task.
- An application ships its own capability tree without vendoring the framework's.
- `pearl task submit` does what it appears to do.
- A mechanical workflow can assert that it is mechanical, and the assertion is checked.
- Rung 2 works end to end: `ddp.pingtung-event-digest` fetches a live feed, verifies the document
  it produced, and pushes it.
- The escalation ladder is reviewable. "Why is this an agent?" has a written answer per
  automation, and four groups are on record as never auto-completing.

### Negative / accepted cost

- `WorkerConfig.capabilities_dir` becomes `capability_dirs: Vec<PathBuf>` — a breaking change for
  any external constructor.
- `task submit` changing its terminal state is a behaviour change for anything scripting it.
  Four CLI tests asserted the old contract and were updated.
- A workflow effect step now has a key without having deduplication, which is a half-measure and
  is documented as one at the point where the key is produced. The risk is unchanged from before
  (there was no dedup either way); what is new is that the step can run at all.
- `effect` steps losing P2 means they no longer count toward `max_llm_calls`. A workflow that was
  relying on that budget to bound its effects was relying on the wrong number, but it was a
  number, and it is gone.

### Migration impact

No stored data changes. `TaskPlan.payload` is `Option` with a serde default, so ledgers written
before it replay unchanged — the projection column added in ADR-0005's migration set already
holds the plan as JSON, so no migration is needed.

Behaviour changes: `task submit` ends in `READY` rather than `CREATED`; a workflow's `effect`
steps are P0; capability directory flags are repeatable and reject duplicate ids.

Rollback is ordinary revert. Nothing here writes a shape an older build cannot read.

## Verification

- [x] Test / check name: `pearl-worker` end-to-end suite, unchanged and passing, confirms the
      payload merge does not disturb the identity keys it merges under
- [x] Test / check name: `an_effect_step_is_given_a_stable_idempotency_key` and
      `a_payload_cannot_supply_its_own_idempotency_key` in `pearl-executor`
- [x] Test / check name: `an_effect_step_is_not_counted_as_a_model_call` and
      `a_plan_step_is_counted_as_a_model_call` in `pearl-workflow`
- [x] Test / check name: `a_held_submission_stays_out_of_the_queue_until_something_admits_it` in
      the CLI suite, with three sibling tests updated to the new contract
- [x] Test / check name: `shipped_prompts_render_from_the_payloads_their_workflows_supply`, plus
      its converse, so an agent capability's one machine-decidable property is checked
- [x] Constitution check updated: no new checks required. Passes on `capabilities/` (6 manifests)
      and `applications/ddp/capabilities/` (27)
- [x] Replay or determinism impact assessed: no new event types, `EVENT_SCHEMA_VERSION` stays at
      2. `TaskPlan.payload` defaults on absence so older ledgers replay unchanged
- [x] End to end against real sources: `ddp.knowledge-hygiene` reaching `VERIFIED_SUCCESS` with
      evidence and, separately, failing verification with `'healthy' expected True, got False`;
      `ddp.pingtung-event-digest` against the live county feed and a running AgentFlow-Notify hub

Not verified: any model call. No llama.cpp or Ollama server was reachable, so `ddp.zen-koan`
compiles and its mechanical steps run while its compose step has never been executed.

## Promotion

- [x] Verification passed — 758 tests across 60 binaries; clippy `-D warnings` clean; fmt clean
- [x] Reviewed
- [x] Landed

---

> Per Constitution Article 12, an architecture-level change requires this chain to be
> complete before promotion: Finding → Proposal → ADR → Verification → Promotion.
