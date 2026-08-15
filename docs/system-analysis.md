# PEARL v2 System Analysis

## A. Executive Summary

PEARL v2 is a **Deterministic-first Autonomous Execution Framework** designed to formalize and elevate the proven engineering patterns from the daily-digest-prompt (DDP) production system into a principled framework governed by a 12-article constitution.

### Core Philosophy

The framework inverts the typical agent-first approach:

```
Mechanical Script (P0) > Workflow (P1) > LLM-assisted (P2) > Autonomous Agent (P3)
```

Increasing uncertainty moves work down this hierarchy; increasing verifiability moves work up.

### What PEARL v2 Is NOT

- A fork of any existing agent framework
- A wrapper around DeepSeek Harness or Grok Build
- A prompt collection or DDP rename

### What PEARL v2 IS

A new framework that extracts DDP's proven operational patterns (YAML single-source-of-truth, PowerShell orchestrators, Hooks-as-guards, 264+ skills, 775+ tools, 20+ workflows) into formal framework primitives backed by:

- **Rust core** with multi-crate workspace architecture
- **SQLite/WAL** for durable state and event ledger
- **Python/PowerShell script runtimes** for mechanical execution
- **Planner-Executor** separation for controlled LLM involvement
- **12-article Constitution** as enforceable governance (not just documentation)

### Key Differentiator

> "What can be computed must not be guessed; what can be verified must not be trusted; what cannot be verified must not be claimed as certain."

---

## B. Reference Architecture Mapping

### B.1 deepseek-harness: Event Lifecycle and Capability Seam Patterns

The deepseek-harness TypeScript monorepo provides the primary **architecture reference** for PEARL v2's event-driven and service-decomposition patterns.

#### Event Lifecycle -> PEARL Event Ledger

deepseek-harness defines a complete workflow lifecycle through typed events in `packages/workflow/workflow/src/index.ts`:

| deepseek-harness Event | PEARL v2 Event Equivalent |
|---|---|
| `workflow/start` | `task.started`, `run.started` |
| `workflow/phase` | `step.started` (phase grouping) |
| `workflow/log` | Structured log emission |
| `workflow/agent-start` | `agent.started` |
| `workflow/agent-end` | `agent.completed` |
| `workflow/end` | `task.completed`, `run.ended` |

Evidence from `packages/workflow/workflow/src/types.ts`:
- `WorkflowRunInfo` carries `id: WorkflowRunId` and `meta: WorkflowMeta` - parallels PEARL's requirement for trace_id + task_id + run_id on every event
- `WorkflowResult` includes `stopReason: 'completed' | 'cancelled' | 'error'` and `agentsStarted` count - maps to PEARL's task state machine (VERIFIED_SUCCESS, CANCELLED, FAILED)
- `WorkflowPhase` with `title`, `detail`, `provider`, `model` fields - maps to PEARL's step-level metadata

#### Service Definition Pattern -> PEARL Capability Seam

The `WorkflowEngine` class in `packages/workflow/workflow/src/index.ts` demonstrates the **abstract service definition** pattern (extending `Service` from `@deepseek-ai/cordis`):

```typescript
export abstract class WorkflowEngine extends Service {
  constructor(ctx: Context) {
    super(ctx, 'workflowEngine')
  }
  abstract start(request: WorkflowStartRequest): WorkflowRun
}
```

This maps to PEARL's **Runtime Adapter Contract** - each execution runtime (Script, Claude Code CLI, Codex CLI, LLM API) must implement a similar abstract contract with `spawn`, `status`, `cancel`, `timeout`, `cleanup` methods (Constitution Article 9).

#### Error Taxonomy -> PEARL Precision Classification

`WorkflowErrorCode` in deepseek-harness defines machine-routable error codes:
```
SCRIPT_PARSE | META_INVALID | INVALID_ARGUMENT | UNSUPPORTED_OPTION |
UNSUPPORTED_SCHEMA | AGENT_CAP | ITEM_CAP | AGENT_START | AGENT_RESULT |
RESULT_UNSERIALIZABLE | CANCELLED
```

The `fatal` flag on `WorkflowError` drives combinator discipline - fatal errors propagate, non-fatal errors map to `null`. This directly maps to PEARL's distinction between:
- Recoverable failures (RETRY_WAIT)
- Permanent failures (FAILED, DEAD)
- Cancellation (CANCELLED)

#### Guard/Timeout Pattern -> PEARL Guard Engine

`packages/guard/timeout-policy/src/index.ts` implements a **cooperative timeout enforcer**:

```typescript
export function apply(ctx: Context): void {
  ctx.on('tools/execute', async (exec, next): Promise<ToolExecutionResult> => {
    const timeoutMs = ctx.tools.get(exec.name, exec.agent)?.timeoutMs
    if (timeoutMs === undefined) return next()
    using d = deadline(exec.signal, timeoutMs, TOOL_TIMEOUT)
    // ...
  })
}
```

Key design decisions applicable to PEARL:
- Tools declare their own timeout budget (`timeoutMs`) - parallels PEARL's per-capability `timeout_seconds` in manifests
- Timeout produces a structured `TOOL_TIMEOUT` error code (not a generic exception)
- Uses `AbortSignal` cooperative cancellation - the tool sees the abort and reaches quiescence
- Plugin architecture: guards are composable middleware (`ctx.on('tools/execute', ...)`)

This maps to PEARL's Guard Engine execution chain: `Request -> Pre Guard -> Execution -> Post Guard -> Verification`

#### Job Registry -> PEARL Durable Work Plane

`packages/jobs/jobs/src/index.ts` defines `JobRegistry` (abstract service):

- `start(spec: JobStart): JobId` - parallels PEARL's task submission
- `list(caller?: Agent): JobSnapshot[]` - owner-scoped visibility
- `read(id: JobId, caller?: Agent): JobRead` - stream output with cursor
- `kill(id: JobId, caller?: Agent, reason?: string)` - cancellation with reason
- `wait(id: JobId, timeoutMs: number, ...)` - bounded wait with abort signal
- `onJobDone(listener: JobDoneListener)` - completion notification
- `attachController(name: string)` - controller registration (jobs require a controller to start)

Key semantics that map to PEARL:
- **Settlement is first-wins** - one terminal record, released waiters
- **Owner-scoped access** - fenced by session id (authorization boundary)
- **Disposal waits for child cleanup** - maps to PEARL's Process Supervisor tree cleanup
- **Registration outlives producer/controller fibers** - durability guarantee

#### Scheduler -> PEARL Scheduler

`packages/schedule/schedule/src/index.ts` provides agent-scoped durable scheduling:
- `ScheduleRuntime` with `start()` and `dispose()` lifecycle
- Event-log-driven reminders (`schedule/change` events in session)
- Types in `domain.ts`: `createAfterScheduleRecord`, `createAtScheduleRecord`, `createEveryScheduleRecord`
- One-shot (`at`), fixed-rate (`every`), and relative (`after`) scheduling

Maps to PEARL's scheduler requirements: cron, interval, one-shot, calendar time, conditional trigger.

---

### B.2 daily-digest-prompt (DDP): Production Evidence Source

DDP is the **primary evidence source** - a production system with 183 YAML configs, 775 tools, 264 skills, 20+ workflows, and 904 state files running autonomously.

#### YAML Single-Source-of-Truth -> PEARL Config Resolution

`config/scoring.yaml` demonstrates the SoT pattern with:

```yaml
version: 3
formula: "score = priority x confidence x description_bonus x time_proximity x label_bonus x recency_penalty x fatigue x citizen_impact"
```

Specific mechanical fields that map to PEARL's P0 (fully deterministic) layer:
- `priority_scores`: `{4: 4, 3: 3, 2: 2, 1: 1}` - direct numerical mapping
- `confidence_multipliers`: `{tier1: 1.0, tier2: 0.8, tier3: 0.6}` - routing confidence
- `time_proximity_bonus`: `{overdue: 1.5, today: 1.3, tomorrow: 1.1, this_week: 1.0, no_due: 0.9}`
- `recency_penalty`: overlap-based decay `{overlap_0_1: 1.0, overlap_2: 0.85, overlap_3_plus: 0.7}`
- `tiebreaker.order`: deterministic tie-breaking (`due_time_asc > priority_desc > label_count_desc > task_id_asc`)

This is the **canonical evidence** for PEARL Constitution Article 1 (Determinism First) - scoring is pure computation, never LLM.

The `allowed_tools_table` in scoring.yaml demonstrates capability-level access control:
```yaml
read_only: "Read,Bash"
full_dev: "Read,Bash,Write,Edit"
research_kb: "Read,Bash,Write,WebSearch,WebFetch"
```

Maps to PEARL's Policy Engine for per-task capability restrictions.

The `mental_model_routing` section demonstrates prompt injection based on keyword matching - this is a P1 pattern (LLM-assisted but mechanically routed).

#### Hooks -> PEARL Guard Engine

`hooks/pre_bash_guard.py` implements a comprehensive command guard with:

1. **Rule-based blocking** with structured rule definitions:
   - `nul-redirect`: blocks Windows nul redirect
   - `scheduler-state-write`: protects state file from agent writes
   - `destructive-delete`: blocks `rm -rf /`, `rm -rf ~`, etc.
   - `force-push`: blocks `git push --force` to main/master
   - `sensitive-env`: blocks reading TOKEN/SECRET/KEY/PASSWORD
   - `exfiltration`: blocks data exfiltration via curl/wget/pipe/base64

2. **Fail-closed semantics**: `check_bash_command()` returns `(blocked, reason, guard_tag)` - blocked commands produce `output_decision("block", reason)`

3. **YAML-driven rules**: `load_yaml_rules("bash_rules", FALLBACK_BASH_RULES)` with fallback to built-in defaults

4. **ReDoS protection**: `MAX_COMMAND_CHARS = 16_384` - commands exceeding this limit are rejected

5. **Structured audit logging**: `log_blocked_event(session_id, "Bash", command, reason, guard_tag)`

6. **Passport system**: `passport_check("Bash")` provides additional access control layer

This directly maps to PEARL Constitution Article 7 (Guard Fail-Closed) and demonstrates the Guard Engine pattern: Guards (security/filesystem/production) must fail-closed; Hooks (logging/metrics) may fail-open.

#### Workflow Index -> PEARL Capability Registry

`workflows/index.yaml` (version 1.2.4, 30+ entries) demonstrates:
- Typed entries: `workflow_yaml`, `validation_checklist`, `output_schema`, `tool`
- Task-type routing: each entry has `task_types` (e.g., `["all"]`, `["system_insight", "self_heal"]`)
- Priority classification: P0 (mandatory) vs P1 (recommended)
- Version tracking: `version`, `created_at`
- Dependency alignment: `alignment` field links to specific config files
- Read-when triggers: `read_when` field for conditional loading

This maps to PEARL's Capability Registry with types: `script`, `tool`, `verifier`, `skill`, `agent`, `workflow`, `runtime`, `guard`.

#### DDP Skill/Tool/Workflow Scale

Production evidence of the ecosystem PEARL must accommodate:
- **264 skills** in `skills/` (e.g., `academic-paper-research`, `arch-evolution`, `auto-task-creator`, `a-tier-task-optimizer`)
- **775 tools** in `tools/` (Python CLI tools following JSON stdin/stdout contract)
- **20+ workflows** in `workflows/` (YAML-defined execution sequences)
- **183 YAML configs** in `config/` (scoring, routing, cache-policy, budget, timeouts, frequency-limits)

---

### B.3 daily_agent: Rust Core Implementation Patterns

The daily_agent Rust workspace provides the **implementation reference** for PEARL's core crate architecture.

#### Multi-Crate Workspace -> PEARL Crate Structure

`Cargo.toml` defines:
```toml
[workspace]
members = [
    "crates/da-core",      # mechanical foundation
    "crates/da-llm",       # LLM integration
    "crates/da-postprocess", # output processing
    "crates/da-tools",     # tool implementations
    "crates/da-runtime",   # runtime orchestration
    "crates/daily-agent",  # application binary
]
```

Key principle from `da-core/src/lib.rs`:
> "da-core does not contain any LLM knowledge" (da-core: mechanical foundation - paths/config/state/breaker/notify/memory/envctl)

This directly maps to PEARL's proposed structure:
- `da-core` -> `pearl-core` + `pearl-state` + `pearl-events`
- `da-llm` -> `pearl-router` + LLM runtime adapters
- `da-runtime` -> `pearl-executor` + `pearl-process-supervisor`
- `daily-agent` -> `apps/pearl-daemon`

#### StateStore -> PEARL Durable State

`da-core/src/state.rs` implements:
- **Atomic writes**: `save()` uses tmp file + rename to prevent half-written JSON
  ```rust
  let tmp = self.dir.join(format!("{name}.json.{}.{seq}.tmp", std::process::id()));
  std::fs::write(&tmp, text)?;
  std::fs::rename(&tmp, &p)?;
  ```
- **File-based locking**: `update_locked()` provides read-modify-write atomicity via `acquire_file_lock()`
- **Task locks**: `try_acquire_lock()` with stale detection (mtime > 2x timeout)
- **Resource group locks**: `try_acquire_resource_lock()` with cooldown tracking
- **JSONL audit trail**: `append_jsonl()` for append-only logging
- **Hash chain tamper detection**: `append_jsonl_chained()` using FNV-1a hash chains (S3 pattern)
- **Chain verification**: `verify_chain()` detects tampering or line deletion

Maps to PEARL's Event Ledger append-only requirement and Constitution Article 6 (State Persistence). The hash chain pattern (`chain = fnv64(previous_line)`) directly implements tamper-evident logging.

#### BreakerBank -> PEARL Circuit Breaker / Runtime Health

`da-core/src/breaker.rs` implements:
- **Three-state machine**: `Closed -> Open -> HalfOpen -> Closed`
- **Verdict enum**: `BreakerVerdict { Closed, HalfOpen, Open }`
- **Persistent state**: stored in `state/circuit-breakers.json` via StateStore
- **Configurable thresholds**: `BreakerCfg { failure_threshold, cooldown_s, min_samples, error_rate }`
- **Two modes**: consecutive-failure (simple) and rate-based (min_samples + error_rate)
- **Counter aging**: `age_counters()` halves counts above AGE_CAP=200 to prevent unbounded growth
- **Thread-safe RMW**: `record_failure()` and `record_success()` both use `update_locked()`
- **Half-open probe**: success in half-open clears all state (self-healing)

Maps to PEARL's Runtime Health monitoring and fallback routing decisions. The breaker verdict feeds into PEARL's Backend Router to determine whether a runtime/backend is available.

#### Config Loading -> PEARL Config Resolution

`da-core/src/config.rs` demonstrates:
- **Version-gated loading**: `load_yaml()` requires `version:` field, fails without it
- **Structured config hierarchy**: `SchedulerConfig -> RuntimeCfg -> TaskCfg`
- **EngineKind separation**: distinguishes execution engines at config level
- **Timezone-aware scheduling**: `timezone: "Asia/Taipei"` default
- **Resource management**: `resource_group_cooldowns` in config

Maps to PEARL's Config Resolution hierarchy: `System -> Profile -> Task Type -> Task -> Runtime Emergency Override`

#### Notification Layer -> PEARL Effect System

`da-core/src/notify.rs` demonstrates:
- **Mechanical sending**: "LLM only drafts; mechanical layer sends"
- **Structured notification**: `Notification { title, body, priority, tags, task_result, result_summary }`
- **Result summary types**: `kind: "list" | "stats" | "evidence" | "digest"` with `ResultItem { text, meta }`
- **Hub + fallback**: AgentFlow-Notify hub primary, ntfy fallback

Maps to PEARL's idempotent side-effect model (Constitution Article 5) with structured effect types.

#### Utility Functions -> PEARL Infrastructure

- `fnv8()` and `fnv64()`: Non-cryptographic hashing for context manifests and notification deduplication
- `redact::redact()`: Secret masking before logging (referenced in `append_jsonl`)
- `envctl` module: Environment control switches

---

### B.4 AgentFlow-Notify: Specification-Driven Governance

The `speckit.constitution` file provides the **governance model** for PEARL's own development process.

#### Constitution Structure -> PEARL Development Governance

Key governance principles from `speckit.constitution`:

1. **Specification-First sequence**: `spec.md -> plan.md -> tasks.md -> implementation`
   - No production code without completed spec-plan-tasks chain
   - Maps to PEARL Constitution Article 12 (ADR required for architecture changes) and Section 55 (development quality)

2. **Required coverage areas** for every spec:
   - Notification delivery: message shape, routing, success/failure semantics, idempotency
   - Retry behavior: retryable vs non-retryable, backoff, max attempts, dead-letter
   - Observability: logs, metrics, traces, alerts
   - Adapter boundary: core vs external, input/output contracts, isolation

   Maps to PEARL's non-functional requirements (Section 60) and Capability Manifest schema requirements.

3. **Plan requirements**:
   - Crate/module boundaries and public interfaces
   - Persistence/queueing/runtime assumptions
   - Test strategy and operational verification
   - Risks, trade-offs, migration concerns

   Maps to PEARL's Plan Compiler validation (Section 30): Schema, Capability exists, Dependency DAG, Policy, Budget, Precision classification, Verifier presence.

4. **Change control**: Revised artifacts must be re-approved in order; implementation pauses on conflicts.
   Maps to PEARL's Constitution CI Gate (Section 56).

---

## C. Gap Analysis

### C.1 What Exists in References (Reusable Patterns)

| PEARL v2 Component | Reference Evidence | Reuse Strategy |
|---|---|---|
| Event Lifecycle | deepseek-harness `workflow/*` events | Adapt event naming and payload structure |
| Service Definition | deepseek-harness `WorkflowEngine extends Service` | Adapt abstract trait pattern in Rust |
| Timeout Policy | deepseek-harness `timeout-policy` plugin | Port cooperative timeout with AbortSignal to Rust CancellationToken |
| Durable Job Registry | deepseek-harness `JobRegistry` (start/list/get/read/kill/wait) | Adapt ownership + settlement semantics |
| Scheduler (one-shot/interval) | deepseek-harness `schedule` (at/every/after) | Extend with cron + misfire policy |
| Scoring/Routing SoT | DDP `config/scoring.yaml` (formula, priority_scores, multipliers) | Migrate as first P0 Mechanical Script |
| Guard Engine | DDP `hooks/pre_bash_guard.py` (regex rules, fail-closed, audit) | Port guard model to Rust pre/post middleware |
| Capability Index | DDP `workflows/index.yaml` (typed, versioned, task_types routing) | Evolve into unified Capability Registry |
| StateStore (atomic write) | daily_agent `state.rs` (tmp+rename, file lock, JSONL) | Adapt for pearl-state, eventual SQLite migration |
| Circuit Breaker | daily_agent `breaker.rs` (3-state, persistent, rate-mode) | Direct port to pearl-core |
| Hash Chain Audit | daily_agent `state.rs::append_jsonl_chained()` (FNV-1a chain) | Adopt for Event Ledger tamper evidence |
| Config Version Gate | daily_agent `config.rs::load_yaml()` (version required) | Adopt in pearl-core config loading |
| Notification Effect | daily_agent `notify.rs` (mechanical send, hub+fallback) | Reference for idempotent effect pattern |
| Governance Model | AgentFlow-Notify `speckit.constitution` (spec-first sequence) | Adopt for PEARL development process |

### C.2 What PEARL v2 Must Build New

| Component | Gap Description | Complexity |
|---|---|---|
| **SQLite Event Ledger** | No reference uses append-only event sourcing with SQLite; daily_agent uses file-based JSONL | High |
| **Precision Decision Engine (P0-P3)** | Novel concept - no reference has step-level classification before execution | High |
| **Plan Compiler** | No reference validates execution plans against policy/budget/capability/verifier presence | High |
| **Assurance Engine** | No reference separates "execution finished" from "verified success" with pluggable verifiers | Medium |
| **Durable Task State Machine** | CREATED->PLANNING->PLANNED->READY->LEASED->RUNNING->VERIFYING->VERIFIED_SUCCESS | Medium |
| **Lease + Heartbeat + Reaper** | No reference implements worker lease with heartbeat timeout and reclamation | Medium |
| **Process Supervisor (tree kill)** | No reference handles full process tree cleanup (Job Objects on Windows, process groups on Linux) | Medium |
| **Constitution CI Gate** | Automated enforcement of 12 articles (no side effect without idempotency, etc.) | Medium |
| **Workflow Checkpoint/Resume** | No reference implements durable workflow step checkpoints with crash recovery | Medium |
| **OODA Governance Loop** | DDP has informal OODA; PEARL needs Observe(machine) -> Orient(hybrid) -> Decide(policy) -> Act(transactional) | Medium |
| **Repair Transaction** | Isolated workspace -> apply -> verify -> promote/rollback for self-heal | Low-Medium |
| **Runtime Profile** (NORMAL/DEGRADED/RECOVERY/EMERGENCY) | DDP has concept but not formalized; needs control over concurrency/budget/effects | Low |
| **Multi-Runtime Adapter** | Unified contract for Rust/Python/PowerShell/Shell/Claude/Codex/Cursor/LLM API | Medium |

### C.3 Critical Gaps by Constitution Article

| Article | Gap |
|---|---|
| Art. 1 (Determinism First) | Need P0 classifier to prevent LLM involvement in computable work |
| Art. 2 (Machine Verifier) | Need UNVERIFIED state + verifier registry; no reference implements this |
| Art. 4 (Provable Success) | Need Evidence model + evidence_required field per step |
| Art. 5 (Idempotency) | Need idempotency_key infrastructure for all effects |
| Art. 6 (Persistent State) | Need SQLite migration from 904 state files |
| Art. 7 (Guard Fail-Closed) | DDP has implementation; needs Rust port + hook vs guard distinction |
| Art. 8 (LLM Cannot Self-Verify) | Need mandatory script verifier in execution chain |
| Art. 9 (Cancellable Runtime) | Need unified cancel/timeout/cleanup contract per runtime |
| Art. 10 (Single SoT) | Need Config Resolution with revision tracking (config_revision, config_hash) |
| Art. 11 (Autonomy vs Verifiability) | Need runtime enforcement of autonomy level based on verification coverage |
| Art. 12 (ADR for Architecture) | Need ADR workflow with Finding -> Proposal -> ADR -> Verification -> Promotion |

---

## D. Recommended Implementation Priorities

Based on the Phase 0-8 migration strategy in the spec (Section 62) and available reference patterns:

### Phase 0: Constitution + Specification (Week 1-2)
- [x] Create `SPEC.md` (system requirements specification) - DONE
- [ ] Create `CONSTITUTION.md` (12 articles as enforceable rules)
- [ ] Create `schemas/` directory with initial JSON schemas
- [ ] Define ADR template and process
- **No production code** - governance documents only

### Phase 1: Kernel (Week 3-6)
- [ ] Set up Cargo workspace (reference: `daily_agent/Cargo.toml` multi-crate pattern)
- [ ] Implement `pearl-core`: config loading (reference: `da-core/src/config.rs` version-gate pattern)
- [ ] Implement `pearl-state`: StateStore with atomic writes (reference: `da-core/src/state.rs`)
- [ ] Implement `pearl-events`: SQLite Event Ledger (append-only, typed events from Section 42)
- [ ] Implement task state machine: CREATED -> ... -> VERIFIED_SUCCESS
- [ ] Implement lease + heartbeat (reference: `da-core/src/state.rs::try_acquire_lock()`)
- [ ] Implement basic worker with process supervision
- **No LLM** - pure mechanical kernel

### Phase 2: Mechanical Runtime (Week 7-10)
- [ ] Implement Precision Decision Engine (P0 classification)
- [ ] Port script runtime adapters (Rust/Python/PowerShell/Shell)
- [ ] Implement Capability Manifest and Registry (reference: DDP `workflows/index.yaml`)
- [ ] Port circuit breaker (reference: `da-core/src/breaker.rs`)
- [ ] Port guard engine (reference: DDP `hooks/pre_bash_guard.py`)
- [ ] Migrate DDP scoring/routing/health-check scripts as first P0 capabilities

### Phase 3: Capability Import (Week 11-14)
- [ ] Implement PythonCapabilityAdapter (CLI + JSON stdout contract)
- [ ] Import DDP's 775 Python tools without rewriting
- [ ] Implement Assurance Engine with pluggable verifiers
- [ ] Implement Evidence model

### Phase 4-8: Application Migration (Week 15+)
- Daily Digest, Todoist Automation, System Audit, OODA, State Consolidation

### Priority Justification

1. **Kernel first**: Without durable state + events, nothing else is reliable
2. **Mechanical before Agent**: Constitution Article 1 - prove the deterministic layer works before adding LLM
3. **Import before rewrite**: 775 tools + 264 skills represent years of production hardening
4. **Governance from day one**: Constitution CI Gate prevents regression of principles

---

## E. Technology Decisions

### E.1 Rust Workspace Architecture

**Decision**: Multi-crate Cargo workspace following `daily_agent` pattern.

**Evidence**: `daily_agent/Cargo.toml` demonstrates successful separation:
- Core (no LLM knowledge) vs LLM vs Runtime vs Tools vs Application
- Shared workspace dependencies with version pinning
- `resolver = "2"` for modern dependency resolution

**PEARL mapping** (from spec Section 57):
```
pearl/
  crates/
    pearl-core/         # config, paths, error, utility (like da-core)
    pearl-events/       # SQLite event ledger
    pearl-state/        # durable state store
    pearl-queue/        # durable work queue
    pearl-lease/        # worker lease + heartbeat
    pearl-scheduler/    # cron/interval/one-shot scheduling
    pearl-planner/      # plan generation
    pearl-plan-compiler/# plan validation
    pearl-executor/     # plan execution
    pearl-assurance/    # verification engine
    pearl-precision/    # P0-P3 classification
    pearl-policy/       # policy engine
    pearl-guard/        # pre/post guards
    pearl-router/       # capability + backend routing
    pearl-workflow/     # declarative + dynamic workflows
    pearl-runtime/      # runtime adapter contracts
    pearl-process-supervisor/  # process tree management
    pearl-capabilities/ # capability registry
    pearl-evidence/     # evidence model
    pearl-governance/   # OODA, ADR, repair transactions
```

**Key dependencies** (from daily_agent workspace):
- `tokio` (async runtime with process/signal support)
- `serde` + `serde_json` + `serde_yaml` (serialization)
- `chrono` + `chrono-tz` (time handling)
- `clap` (CLI)
- `uuid` (identifiers)
- `tracing` + `tracing-subscriber` (structured logging)
- `tempfile` (atomic file operations)
- Additional: `rusqlite` (SQLite), `cron` (schedule parsing)

### E.2 SQLite/WAL for Event Ledger

**Decision**: SQLite with WAL mode for the Event Ledger and materialized state.

**Rationale**:
- Spec Section 42 requires append-only event storage with typed events
- Spec Section 43 requires materialized views (tasks, runs, attempts, leases, etc.)
- daily_agent's file-based state (JSONL + hash chains) proves the concept but lacks query capability
- deepseek-harness's job registry semantics (start/list/get/read/wait) require indexed lookups
- SQLite provides ACID guarantees without external dependencies
- WAL mode enables concurrent readers with single writer

**Schema design principle**: Events table is append-only (the audit trail); materialized tables are rebuilt from events (replay capability per spec Section 61).

### E.3 Event Sourcing Approach

**Decision**: Event Ledger as source of truth; materialized tables for queries.

**Evidence supporting this**:
- daily_agent `append_jsonl_chained()` already implements append-only audit with hash chain verification
- deepseek-harness events are observe-only (`@mode emit`) - listeners cannot modify events
- DDP state files (904) represent accumulated materialized state that needs consolidation
- Spec Section 42 explicitly lists event types: `task.created`, `task.planned`, `task.leased`, etc.

**Implementation**:
- Write: append event to `events` table (immutable)
- Read: query materialized tables (tasks, runs, attempts, etc.)
- Recovery: replay events from last checkpoint to rebuild materialized state
- Audit: verify chain integrity (adopt FNV-1a pattern from daily_agent)

### E.4 Script-First Routing

**Decision**: Router always checks for mechanical capability before agent routing.

**Evidence**:
- DDP `config/scoring.yaml` demonstrates that task scoring is pure computation (formula with multipliers)
- DDP already separates mechanical work (YAML-driven scoring, schema validation) from LLM work
- Spec Section 50 mandates: "Router must first ask: is there a mechanical capability that can complete this?"

**Implementation pattern**:
```
Router
  -> Check Capability Registry for script/verifier/tool matching task requirements
  -> If found: route to Script Runtime (P0)
  -> If not: check precision classification
  -> Route to appropriate runtime (P1/P2/P3)
```

### E.5 Guard Architecture

**Decision**: Two-tier guard system distinguishing hooks (fail-open) from guards (fail-closed).

**Evidence**:
- DDP `hooks/pre_bash_guard.py` implements fail-closed blocking with audit
- deepseek-harness `timeout-policy` implements cooperative pre-execution middleware
- Spec Section 46 defines: `Request -> Pre Guard -> Execution -> Post Guard -> Verification`

**Guard types** (from DDP evidence):
- `safety-guard`: destructive operations
- `state-guard`: protected file writes
- `git-guard`: force push protection
- `env-guard`: secret access
- `exfiltration-guard`: data leak prevention
- `confirm-guard`: operations requiring confirmation
- `passport-guard`: access control
- `redos-input-guard`: input validation

### E.6 Development Governance

**Decision**: Adopt AgentFlow-Notify's specification-driven development model.

**Evidence**: `speckit.constitution` defines enforceable sequence:
1. Feature spec with required coverage (delivery, retry, observability, adapter boundary)
2. Plan with crate boundaries, test strategy, risks
3. Tasks with spec/plan traceability
4. Implementation only after approval chain

**PEARL adaptation**:
- Every crate/feature needs: spec -> plan -> tasks -> implementation
- Constitution CI Gate validates governance compliance
- ADR process for architecture decisions (spec Section 55)
- Change control: revised artifacts re-approved in order (spec Section 12, Article 12)

---

## References

- deepseek-harness: `packages/workflow/workflow/src/index.ts`, `types.ts`
- deepseek-harness: `packages/jobs/jobs/src/index.ts`
- deepseek-harness: `packages/guard/timeout-policy/src/index.ts`
- deepseek-harness: `packages/schedule/schedule/src/index.ts`
- daily-digest-prompt: `config/scoring.yaml`
- daily-digest-prompt: `hooks/pre_bash_guard.py`
- daily-digest-prompt: `workflows/index.yaml`
- daily-digest-prompt: `skills/` (264 skills directory)
- daily_agent: `Cargo.toml`, `crates/da-core/src/lib.rs`
- daily_agent: `crates/da-core/src/state.rs`
- daily_agent: `crates/da-core/src/breaker.rs`
- daily_agent: `crates/da-core/src/config.rs`
- daily_agent: `crates/da-core/src/notify.rs`
- AgentFlow-Notify: `speckit.constitution`
