# ADR-0004: Runtime Families, Schedules That Point at Specs, and Where the Verifier Obligation Comes From

**Status:** Accepted
**Date:** 2026-08-15
**Constitution articles touched:** 1 (determinism first), 2 (exactness needs a verifier), 3 (LLM not responsible for infrastructure), 9 (cancellability), 10 (single source of truth), 11 (autonomy from verifiability)

---

## Finding

Completing §37 (runtime support), §47 (durable scheduler) and §59 (CLI) surfaced five decisions
that could not be made by analogy with existing code.

1. **The agent runtimes were enum variants and nothing else.** §37 lists Claude Code, Codex,
   Cursor and an OpenAI-compatible API as *required*. All four were stubs returning "not
   configured" unconditionally, and there was no way to reach a fifth provider at all.

2. **The verifier obligation was keyed to the wrong thing.** The plan compiler demanded a
   verifier for every `P0` or `P1` step. §30 keys it on `exactness_required`. The difference is
   not academic: with the compiler's rule, *no* mechanical workflow could compile, because every
   deterministic step was treated as load-bearing. A cache-age probe and a financial calculation
   are both P0; only one of them must be right.

3. **A schedule had nothing to point at.** The `schedules` table keyed on a task, but a
   recurring schedule that re-ran one task id would collide with its own previous occurrence —
   the state machine forbids re-entering a terminal state, so the second day would simply fail.

4. **Two parsers for task specs.** `TaskSpec` lived in the CLI. The scheduler needs to submit
   tasks from spec files, so it would have needed its own — and two parsers eventually disagree
   about the Article 2 gate, which is the one thing a spec parser exists to enforce.

5. **An unconfigured API provider cost a run record.** Credentials were discovered at execution
   time, after `start_run`. For a paid endpoint that is the wrong moment to find out, and it left
   a run recorded for work that never began.

## Decision

**1. Runtimes have families, and providers are named.**
`RuntimeFamily::{Mechanical, AgentCli, Api}` classifies every runtime, because the three need
different things from the caller: a supervisor, a supervisor, and an HTTP client respectively.
Groq, Mistral and NVIDIA are explicit `Runtime` variants rather than configurations of
`openai_compatible`, so a manifest, a permission rule and a routing decision can all speak about
one provider — they differ in cost and rate limit, which is exactly what policy cares about. One
adapter serves all of them because they share a protocol.

**2. Prompts are files.** An agent capability's entrypoint is a prompt template, and the task
payload is rendered into `{{placeholders}}`. Article 3 keeps infrastructure out of prompts; the
mirror of that rule is keeping prompts out of code, where they cannot be reviewed as content or
changed without a rebuild. An unknown placeholder is an error, not an empty string: a prompt that
silently lost a variable produces a plausible answer built on missing information.

**3. Readiness is checked before anything is opened.** The worker asks the runtime to `validate`
before `start_run`. Everything that check covers — is the tool installed, is there a credential,
can the prompt be filled — is knowable without executing. An unconfigured provider now costs
nothing and leaves no trace but a `BLOCKED` task naming the variable to set.

**4. The exactness demand lives on the step.** `PlanStep::exactness_required`, declared by the
workflow author, is what obliges a verifier — and it is checked unconditionally, where the old
rule was skipped entirely when no capability registry was supplied, which is the default.

**5. A schedule points at a spec.** Each occurrence is submitted as a new task with a
timestamped id, carrying the spec's plan and quality contract, so a scheduled run is verified
exactly as a manual one is. Firing is recorded *after* submission: a crash in between re-fires
rather than skips, and for a schedule a duplicate is recoverable while a silent miss is not.

**6. One spec parser, in `pearl-state`.** A spec produces a `TaskSubmission`, so it belongs with
the type it produces.

## Alternatives Considered

| Alternative | Why rejected |
|---|---|
| Keep one `openai_compatible` runtime, select the provider by env | A permission rule could not distinguish a free local model from a paid endpoint, which is the distinction policy exists to make |
| Build prompts in Rust | Prompt revisions become code revisions; content stops being reviewable as content |
| Empty string for an unknown placeholder | Produces a confident answer built on missing input — the failure mode hardest to detect downstream |
| Derive the exactness obligation from `precision_class` | Makes every mechanical step require a verifier, so no ordinary workflow compiles |
| Add `exactness_required` to the capability manifest instead | The capability does not know how a task will use its result; the same script can be load-bearing in one plan and a probe in another |
| Schedule points at a task id, and the daemon resets its state | Rewrites history: the second occurrence would overwrite the first one's run and evidence |
| Record the firing before submitting | A crash in between silently skips an occurrence, which for a daily digest means the day it crashed has no digest and nothing says so |
| Let the scheduler own its own spec parser | Two parsers, one Article 2 gate, eventually disagreeing |
| Check credentials at execution time (as before) | Costs a run record, and for a paid endpoint discovers the problem after committing to the call |
| Registry skips any YAML it cannot parse | A typo in a capability id would become a silently missing capability. The carve-out is exactly one shape: `steps` and no `execution`, which is a workflow |

## Consequences

### Positive

- §37's required runtimes are reachable: agent CLIs spawn under the process supervisor, and the
  API family covers five providers through one adapter.
- `pearl script run` executes, `pearl verify` asks a machine verifier, `pearl workflow run`
  compiles and executes with durable checkpoints, and `pearl-daemon` fires schedules.
- Schedules survive a restart, and a restart no longer re-fires or skips an occurrence.
- Nine formerly write-only projection tables are readable, and `runtime_health` is written by
  the daemon's own observations, so the numbers OODA reasoned about are the numbers an operator
  sees.

### Negative / accepted cost

- `ureq` and `rustls` enter the dependency tree. The kernel now contains an HTTP client, which
  the guard rules were written partly to prevent; the mitigation is that no request is built
  without an explicitly configured credential.
- `ProcessSupervisor::spawn` and `PlanStep` are breaking changes for out-of-tree code.
- `pearl workflow run` ends in `UNVERIFIED` even when every step succeeds, because an ad-hoc run
  declares no assurance. That is Article 2 working as intended, and it will surprise people.
- Step-to-step data flow still does not exist: each step receives the task payload, not its
  predecessor's output. The shipped example workflow says so rather than implying otherwise.
- The DDP application still has capabilities with no implementation. They now say so in their
  description and carry no entrypoint, and a new warning-level check surfaces them.
- `Runtime::Anthropic` is still absent, so §37's Anthropic-compatible API is unmet.

### Migration impact

The `schedules` table gained columns. `CREATE TABLE IF NOT EXISTS` does not add them, so an
existing database must be rebuilt from its ledger. This is the third schema change needing a
manual step; `migrations/` is now overdue rather than merely absent.

## Verification

- [x] `an_agent_cli_capability_executes_through_the_configured_tool` — the agent path end to
      end, with a wrapper standing in for the real CLI, asserting `agent.started` rather than
      `script.started`
- [x] `an_api_capability_with_no_credential_is_refused_before_any_request` — no key, no call, no
      run record
- [x] `an_unnamed_task_can_reach_an_agent_capability_by_task_type` — an agent is reachable by
      routing, not only by being named
- [x] `a_missing_credential_is_refused_before_any_request_is_built`, `a_local_model_needs_no_credential`
- [x] `an_unknown_placeholder_is_an_error_rather_than_an_empty_string`
- [x] `an_exactness_demand_is_checked_even_with_no_registry`,
      `a_step_making_no_exactness_demand_needs_no_verifier`
- [x] `the_last_trigger_survives_a_restart`, `an_interval_schedule_does_not_fire_twice_before_its_interval_elapses`
- [x] `a_schedule_with_a_missing_spec_is_reported_without_stopping_the_loop`
- [x] `the_reaper_reclaims_a_lease_from_a_worker_that_disappeared`
- [x] `workflow_run_refuses_to_run_a_plan_that_did_not_compile` — §30 is a gate, not advice
- [x] `verify_run_distinguishes_a_check_that_could_not_run` — exit 2, not 1
- [x] Constitution check passes on `capabilities/` (5 manifests) and on
      `applications/ddp/capabilities/` (19 manifests)
- [x] Replay impact assessed: schedules and health are observations rather than ledger
      projections, so a rebuild empties them; nothing in the event vocabulary changed except the
      two new `agent.*` variants, which are inert for projection

## Promotion

- [x] Verification passed
- [ ] Reviewed
- [ ] Landed
