# PEARL Constitution

**Version:** 1.0.0
**Status:** Ratified
**Source:** `系統開發需求書.md` sections 4–15
**Enforcement:** `pearl constitution check` (CI gate) + `crates/pearl-governance`

---

## Preamble

These twelve articles are not coding guidelines. They are binding constraints on every
Framework component, Workflow, Agent, Skill, Tool, Script, PR and ADR in this repository.

A change that violates an article does not merit discussion about whether it is convenient.
It is rejected.

The single sentence that generates all twelve articles:

> **能算的不要猜，能驗的不要信，不能驗的不要假裝確定。**
>
> What can be computed must not be guessed.
> What can be verified must not be trusted.
> What cannot be verified must not be claimed as certain.

---

## Article 1 — Determinism First

**Rule:** Work that a deterministic program can complete MUST NOT be delegated to an LLM.

The following are deterministic and therefore forbidden to LLM judgement:

| Domain | Examples |
|---|---|
| Structure | JSON Schema validation, YAML parsing, config consistency |
| Arithmetic | date computation, counting, sorting, threshold comparison, scoring formulas |
| Identity | hashing, SHA comparison, digest equality |
| Filesystem | file existence, path resolution, encoding (UTF-8/CJK) |
| Version control | git status, git diff, commit existence |
| Network | HTTP status codes, API health |
| Execution | test results, exit codes, timeout, retry, frequency, dependency order |

**Machine check:** `constitution::check_no_llm_for_deterministic` — any capability manifest
declaring `quality.deterministic: true` MUST have `execution.kind: script`.

**Violation example (rejected):**
> Prompt: "Please judge whether this JSON conforms to the schema."

when `jsonschema` can decide it.

---

## Article 2 — Exact Items Require a Machine Verifier

**Rule:** Any item that *can* be exactly confirmed, or that the business *requires* to be
exactly confirmed, MUST have a Machine Verifier.

### Case A — an exact method already exists

Tests passing, JSON validity, Todoist completion, file existence, git commit existence,
record counts, HTTP 200, SHA equality, schema conformance.

→ MUST be decided by script. No exceptions.

### Case B — the business requires exactness but no verifier exists yet

Examples: "not a single record may be missing", "every citation must resolve",
"no duplicate notification may be sent".

→ The task MUST NOT enter `VERIFIED_SUCCESS`. It enters `UNVERIFIED` and stays there until
either a verifier is built or a Human Gate resolves it.

**LLM self-assessment is not a verifier and never substitutes for one.**

**Machine check:** `constitution::check_exactness_has_verifier` — a task spec with
`quality.exactness_required: true` and `quality.deterministic_verification: false`
fails compilation with `CompileError::MissingVerifier`.

---

## Article 3 — The LLM Is Not Responsible for Infrastructure

**Rule:** Prompts MUST NOT carry infrastructure responsibility.

Forbidden in prompts: retry, timeout, fallback, queue, locking, process cleanup, heartbeat,
schedule, schema validation, idempotency, dependency resolution, circuit breaker,
budget enforcement.

Permitted to the LLM: interpret, classify, plan, reason, research, synthesize, recommend, explain.

**Rationale:** Infrastructure semantics must be identical on every run. Prompt text is not.

---

## Article 4 — Every Success Must Be Provable

**Rule:** A bare `{"status": "success"}` is not a result.

A terminal success payload MUST carry:

```json
{
  "status": "success",
  "result": {},
  "evidence": [],
  "verification": [],
  "artifacts": [],
  "runtime": {},
  "metrics": {}
}
```

> **Success = Result + Evidence + Verification**

No evidence ⇒ the run MUST NOT be recorded as `VERIFIED_SUCCESS`.

**Machine check:** `constitution::check_success_has_evidence` — the state machine refuses the
`VERIFYING → VERIFIED_SUCCESS` transition when the evidence set is empty.

---

## Article 5 — Side Effects Must Be Idempotent

**Rule:** Every external side effect MUST carry an `idempotency_key`.

Covered: sending notifications, completing Todoist tasks, sending email, database writes,
artifact writes, git commits, publishing, deletion.

Key format is `{effect}:{target}:{scope}`:

```
todoist:complete:task_123:run_456
ntfy:daily_digest:2026-08-15
```

Runtime retry MUST NOT produce a duplicate side effect. The effect ledger is consulted
before the effect is committed.

**Machine check:** `constitution::check_effect_has_idempotency_key` — a capability with
`risk.side_effect: true` and no `idempotency` declaration fails the CI gate.

---

## Article 6 — State Must Be Persistent

**Rule:** Process memory is not state.

Any work spanning more than a single step MUST be represented by durable `Task`, `Run`,
`Attempt`, `Checkpoint` and `Event` records.

After a restart the system MUST be able to answer:

1. What happened?
2. How far did it get?
3. Can it be retried?

**Machine check:** replay tests — rebuilding materialized state from the event ledger MUST
yield byte-identical state.

---

## Article 7 — Guards Fail Closed

**Rule:** Hooks and Guards have opposite failure semantics and MUST NOT be conflated.

| Kind | Concerns | On failure |
|---|---|---|
| **Hook** | logging, metrics, notification, telemetry | **fail-open** — execution proceeds |
| **Guard** | secrets, security, filesystem boundary, production write, dangerous shell, budget, side effect | **fail-closed** — execution is denied |

A Guard that crashes denies the operation. A Guard that cannot reach its policy source
denies the operation. Silence is never consent.

**Machine check:** `constitution::check_guard_fail_closed` — every registered guard must
declare `on_error: deny`.

---

## Article 8 — The LLM May Not Declare Verification Passed

**Rule:** Verification is an act of execution, not of assertion.

Forbidden:
> Agent: "I have checked it, the tests should be fine."

Required chain:

```
Agent → Script verifier → pytest / cargo test / schema / hash / query → PASS
```

The verifier's exit status is the verdict. Agent narration about the verdict is metadata.

---

## Article 9 — Every Runtime Must Be Cancellable

**Rule:** A Runtime Adapter MUST implement all five operations:

```
spawn    status    cancel    timeout    cleanup
```

Cancellation MUST reclaim the entire execution scope — worker, child, grandchild —
via process group (Unix) or Job Object (Windows).

A backend that cannot be reliably cancelled MUST NOT be registered as a Runtime.

**Machine check:** runtime contract tests — every adapter is spawned, cancelled, and asserted
to leave no surviving descendant process.

---

## Article 10 — Configuration Has Exactly One Source of Truth

**Rule:** Configuration resolves through a fixed precedence chain, and every run records
which revision it used.

```
System → Profile → Task Type → Task → Runtime Emergency Override
```

Every `Run` MUST persist `config_revision` and `config_hash`. Without them the run is not
reproducible and therefore not auditable.

**Machine check:** `constitution::check_run_has_config_revision` — a run record missing
either field is rejected at insert time.

---

## Article 11 — Autonomy Is Inversely Proportional to Unverifiability

**Rule:** Autonomy is granted by verification coverage, never by model capability.

```
Verification ↑  ⇒  Autonomy ↑
Verification ↓  ⇒  Autonomy ↓
```

> A stronger model does not earn wider permission. A system that can verify more earns
> the right to act more autonomously.

**Machine check:** the Policy Engine derives the permitted autonomy level from the
verification coverage of the capability being invoked, not from the identity of the backend.

---

## Article 12 — Architecture Changes Require an ADR

**Rule:** Self-Heal may propose, implement and test candidate changes. It may not land
architecture-level change without the full chain:

```
Finding → Proposal → ADR → Verification → Promotion
```

**Machine check:** `constitution::check_architecture_change_has_adr` — a diff touching
`crates/*/src/lib.rs` public surface, `schemas/`, or `CONSTITUTION.md` requires a
corresponding `docs/adr/ADR-*.md` in the same change set.

---

## Enforcement Summary

| Article | Machine check | Enforced in |
|---|---|---|
| 1 | `check_no_llm_for_deterministic` | `pearl-governance` |
| 2 | `check_exactness_has_verifier` | `pearl-plan-compiler` (planned), CI gate |
| 3 | prompt-surface lint | CI gate |
| 4 | `check_success_has_evidence` | `pearl-state` transition guard |
| 5 | `check_effect_has_idempotency_key` | `pearl-governance` |
| 6 | replay determinism test | `tests/replay` |
| 7 | `check_guard_fail_closed` | `pearl-guard` (planned) |
| 8 | verifier-in-chain assertion | `pearl-assurance` (planned) |
| 9 | runtime contract tests | `tests/contract` |
| 10 | `check_run_has_config_revision` | `pearl-state` insert guard |
| 11 | autonomy derivation | `pearl-policy` (planned) |
| 12 | `check_architecture_change_has_adr` | CI gate |

Checks marked *(planned)* correspond to crates not yet implemented in Phase 1. The article
is still binding; the automated check lands with its crate.

---

## Amendment

This document is amended only through the Article 12 chain. An amendment PR MUST include:

1. The ADR that justifies it
2. The updated machine check
3. The test proving the new check rejects the behaviour it forbids
