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
- A wrapper around any single LLM provider or build tool
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

### B.1 agent-dashboard: SQLite Event Store, Typed Domain Events, and Workflow State Machines

The agent-dashboard Rust multi-crate workspace provides the primary **architecture reference** for PEARL v2's event-driven persistence and workflow state management patterns.

#### Event Lifecycle via Typed DomainEvent Enum

`crates/agentflow-core/src/domain_event.rs` defines a complete set of domain events as a tagged union:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DomainEvent {
    WorkflowRunCreated { workflow_run_id: Uuid, workflow_id: String, workflow_version: String },
    WorkflowStatusChanged { workflow_run_id: Uuid, from: WorkflowStatus, to: WorkflowStatus },
    StepRunCreated { workflow_run_id: Uuid, step_run_id: Uuid, step_id: String },
    StepStatusChanged { workflow_run_id: Uuid, step_run_id: Uuid, step_id: String, from: StepStatus, to: StepStatus },
    ArtifactStored { workflow_run_id: Uuid, step_run_id: Uuid, artifact_id: Uuid, name: String, kind: ArtifactKind },
    ApprovalRequested { workflow_run_id: Uuid, step_run_id: Uuid, step_id: String },
    ApprovalGranted { workflow_run_id: Uuid, step_run_id: Uuid, approved_by: String },
    ApprovalDenied { workflow_run_id: Uuid, step_run_id: Uuid, denied_by: String, reason: Option<String> },
}
```

| agent-dashboard Event | PEARL v2 Event Equivalent |
|---|---|
| `WorkflowRunCreated` | `task.created`, `run.started` |
| `WorkflowStatusChanged` | `task.state_changed` (state machine transition) |
| `StepRunCreated` | `step.started` |
| `StepStatusChanged` | `step.state_changed` |
| `ArtifactStored` | `evidence.stored` |
| `ApprovalRequested` | `approval.requested` (human-in-the-loop) |
| `ApprovalGranted/Denied` | `approval.resolved` |

#### EventEnvelope with UUIDv7

```rust
pub struct EventEnvelope {
    pub id: Uuid,              // UUIDv7 - sortable by time
    pub schema_version: u32,   // Forward compatibility
    pub occurred_at: DateTime<Utc>,
    pub trace_id: Uuid,        // Correlates all events for a workflow run
    pub event: DomainEvent,
}
```

This directly maps to PEARL's requirement for `trace_id + task_id + run_id` on every event. The UUIDv7 choice provides time-ordered identifiers without requiring a sequence counter.

#### SQLite Event Store

`crates/agentflow-store/src/event_store.rs` implements append-only event persistence:

```rust
pub fn insert(conn: &Connection, envelope: &EventEnvelope) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO domain_events
            (id, schema_version, trace_id, occurred_at, event_type, payload)
         VALUES (?1,?2,?3,?4,?5,?6)",
        params![envelope.id.to_string(), envelope.schema_version,
                envelope.trace_id.to_string(), envelope.occurred_at.to_rfc3339(),
                event_type, payload],
    )?;
    Ok(())
}
```

Key design decisions applicable to PEARL:
- Events stored as typed JSON payload in SQLite (queryable, indexed by trace_id)
- `schema_version` field enables forward-compatible event evolution
- Query by `trace_id` retrieves complete workflow execution history in temporal order
- Uses `rusqlite` with bundled SQLite, `chrono`, and `uuid` features

This validates PEARL's choice of SQLite for the Event Ledger: append-only event table as source of truth, with materialized views rebuilt from events.

#### Workflow State Machine with Transition Validation

`crates/agentflow-core/src/workflow_run.rs` implements a validated state machine:

```rust
pub enum WorkflowStatus {
    Pending, Queued, Running, Waiting, WaitingApproval, Retrying, Degraded, Completed, Failed, Cancelled,
}

impl WorkflowStatus {
    pub fn can_transition_to(&self, next: &WorkflowStatus) -> bool {
        self.allowed_next().contains(next)
    }
}
```

Transition rules: `Pending -> Queued -> Running -> {Waiting, WaitingApproval, Retrying, Degraded, Completed, Failed, Cancelled}`. Terminal states (`Completed`, `Failed`, `Cancelled`) allow no further transitions.

This maps to PEARL's task state machine: `CREATED -> PLANNING -> PLANNED -> READY -> LEASED -> RUNNING -> VERIFYING -> VERIFIED_SUCCESS / FAILED / CANCELLED`

#### Step State Machine with Type Classification

`crates/agentflow-core/src/step_run.rs` provides step-level state management:

```rust
pub enum StepStatus { Pending, Waiting, Running, Retrying, WaitingApproval, Completed, Failed, Skipped, Cancelled }
pub enum StepType { Tool, Agent, Notification, Approval, Deterministic }
```

Key fields on `StepRun`:
- `idempotency_key: Option<String>` - supports PEARL Constitution Article 5 (Idempotency)
- `attempt: u32` - retry tracking per step
- `output: Option<serde_json::Value>` - structured step output
- `error_message: Option<String>` - error details for failed steps

The `StepType` enum directly validates PEARL's Precision classification concept: `Deterministic` maps to P0, `Tool` to P1, `Agent` to P2/P3.

#### Multi-Crate Workspace Architecture

`Cargo.toml` workspace structure:
```toml
[workspace]
members = ["crates/agentflow-core", "crates/agentflow-schema",
           "crates/agentflow-store", "crates/agentflow-runtime", "apps/server"]
resolver = "2"
```

Dependencies: `rusqlite` (bundled, chrono, uuid features), `uuid` (v7, serde), `axum` (HTTP), `tokio` (full), `chrono` (serde), `tracing`/`tracing-subscriber`, `serde`/`serde_json`/`serde_yaml`.

---

### B.2 daily_rust: Scheduler Engine, Process Supervisor, State Store, and Health Monitor

The daily_rust workspace provides the **implementation reference** for PEARL's scheduler, process management, state persistence, and self-healing health monitoring.

#### Scheduler Engine with Cron, Interval, and Slot-Based Scheduling

`src/scheduler/mod.rs` implements `SchedulerEngine` with:

```rust
pub struct SchedulerEngine {
    config: Arc<SchedulerConfig>,
    state: Arc<StateStore>,
    runner: Arc<TaskRunner>,
    health: Arc<HealthMonitor>,
    semaphore: Arc<Semaphore>,       // Global parallel cap
    in_flight: Arc<AtomicUsize>,     // Profile-aware cap tracking
    notifier: NtfyNotifier,
}
```

Key scheduling modes (from `ScheduleKind`):
- **Cron**: Standard cron expressions with timezone awareness (`"Asia/Taipei"`) and same-trigger-point deduplication
- **Interval**: Fixed-rate execution with elapsed-time tracking
- **Slot**: Group-based scheduling with selection algorithms and per-day dedup
- **Manual**: On-demand only; **Disabled**: explicitly turned off

Concurrency control:
- `Semaphore` for global `max_parallel_tasks` enforcement
- `InFlightGuard` (Drop-based) for profile-aware cap tracking via `AtomicUsize` CAS loop
- `try_begin_task(cap)` returns `Option<InFlightGuard>` for lock-free slot claiming
- Profile cap can be lower than global cap (e.g., degraded profile limits to 1 parallel task)

Maps to PEARL's scheduler requirements: cron, interval, one-shot, conditional trigger, profile-based throttling.

#### Process Supervisor with Platform-Specific Tree Kill

`src/process/mod.rs` defines the `ProcessSupervisor` trait:

```rust
pub trait ProcessSupervisor {
    fn spawn(&self, cmd: &CommandSpec, stdout_path: &Path, stderr_path: &Path) -> Result<SupervisedProcess>;
    fn graceful_stop(&self, proc: &SupervisedProcess) -> Result<()>;
    fn force_kill_tree(&self, proc: &SupervisedProcess) -> Result<()>;
    fn is_alive(&self, proc: &SupervisedProcess) -> bool;
    fn try_wait(&self, proc: &mut SupervisedProcess) -> Result<Option<i32>>;
}
```

Platform implementations:
- **Unix**: `UnixProcessSupervisor` using process groups via `nix` crate (sends signals to entire process group)
- **Windows**: `WindowsJobSupervisor` using Job Objects (kernel-level process tree containment)

`SupervisedProcess` carries `pid`, `child: Option<Child>`, and on Windows a Job Object handle.

This directly maps to PEARL's Runtime Adapter Contract - each execution runtime (Script, Claude Code CLI, Codex CLI, LLM API) must implement `spawn`, `status`, `cancel`, `timeout`, `cleanup` methods (Constitution Article 9).

#### State Store with Task Locks and Heartbeat

`src/state/mod.rs` implements durable state management:

```rust
pub struct TaskLock {
    pub task_id: String,
    pub run_id: String,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub heartbeat_at: DateTime<Utc>,
}

pub struct RuntimeState {
    pub heartbeat_at: DateTime<Utc>,
    pub active_profile: String,
    pub running_tasks: u32,
    pub started_at: DateTime<Utc>,
    pub version: String,
}
```

Key state operations:
- `acquire_lock(lock)` / `release_lock(task_id)` - per-task execution locking
- `read_lock(task_id)` / `list_locks()` - lock enumeration for health monitoring
- `write_run(record)` - persists `TaskRunRecord` with status, attempt, exit_code, duration, pid, fallback_from
- `update_heartbeat()` - periodic liveness signal for stale detection
- `read_task_meta(task_id)` - `TaskMeta` with `consecutive_failures`, `last_success_date`, `round_robin_counter`, `last_cron_trigger`
- `cleanup_task_runs(keep_days)` - retention-based pruning by mtime

`RunStatus` enum: `Pending | Running | Success | Failed | Timeout | Killed`

Maps to PEARL's Lease + Heartbeat + Reaper pattern and durable work plane.

#### Health Monitor with Stale Lock Recovery and Profile Degradation

`src/health/mod.rs` implements self-healing:

```rust
pub struct HealthMonitor {
    config: Arc<SchedulerConfig>,
    state: Arc<StateStore>,
    supervisor: Box<dyn ProcessSupervisor + Send + Sync>,
    notifier: NtfyNotifier,
}
```

Health check cycle (`check_once()`):
1. `revalidate_config()` - re-parse config for hot-reload
2. `recover_stale_locks()` - detect dead processes via `is_alive()` + timeout heuristic (`elapsed > timeout * 2`), then `force_kill_tree()` + `release_lock()`
3. `check_repeated_failures()` - profile escalation: Normal -> Degraded (3+ fresh failures) -> Recovery (5+ fresh failures), with automatic restoration when failures clear
4. `check_log_size()` - warns when log directory exceeds 1GB

**Failure decay** (`failure_decay_hours`): Failures older than the decay window stop counting toward profile degradation. This prevents permanently stuck tasks from wedging the system in degraded/recovery mode. Default is 4 hours, configurable per deployment.

**Profile notification**: Profile changes trigger ntfy notifications with deduplication (same profile is not re-notified).

Maps to PEARL's Runtime Health monitoring, fallback routing decisions, and self-healing OODA cycle.

---

### B.2.1 daily_rust/agentflow-harness-rust: Workflow Engine, Checkpoint/Resume, Quality Gates, Policy Guard

The `agentflow-harness-rust` sub-crate within daily_rust provides the **workflow execution engine** reference for PEARL's plan execution and assurance patterns.

#### Workflow Engine with Checkpoint/Resume and Degraded Fallback

`src/workflow_engine.rs` implements `WorkflowEngine`:

```rust
pub struct WorkflowRunOptions {
    pub run_id: Option<String>,
    pub job_id: Option<String>,
    pub resume: bool,                    // Resume from checkpoint
    pub state_dir: Option<PathBuf>,
    pub fallback_workflow: Option<WorkflowDef>,  // Degraded fallback
    pub auto_degraded_fallback: bool,
}
```

Key patterns:
- **Checkpoint/Resume**: Each completed stage persists a `WorkflowCheckpoint` (run_id, workflow_id, completed_step_ids, context, partial_report). On resume, execution skips already-completed stages.
- **Degraded Fallback**: When primary workflow fails quality gate, automatically runs `degraded_workflow_for(primary_id)` mapping (e.g., `"daily_digest" -> "daily_digest_degraded"`)
- **DAG-based Stage Execution**: Steps within a stage execute in parallel via `JoinSet` with `Semaphore` for `max_parallel` control
- **Per-attempt Timeout**: Each retry attempt gets a fresh timeout budget (not shared across retries)
- **Critical-aware Retries**: Critical steps get at least 1 retry even if configured with 0

This maps to PEARL's Plan Executor with durable checkpoint and recovery capabilities.

#### Checkpoint Model

`src/checkpoint.rs` provides crash recovery:

```rust
pub struct WorkflowCheckpoint {
    pub run_id: String,
    pub workflow_id: String,
    pub job_id: Option<String>,
    pub stages_completed: usize,
    pub completed_step_ids: Vec<String>,
    pub context: WorkflowContext,
    pub partial_report: Option<WorkflowRunReport>,
}
```

Operations: `save_checkpoint(base, checkpoint)` and `load_checkpoint(base, run_id)` with filesystem persistence.

Maps to PEARL's Workflow Checkpoint/Resume requirement - durable step-level checkpoints with context preservation for crash recovery.

#### Quality Gates

`src/quality_gate.rs` evaluates workflow completion:

```rust
pub struct QualityGateResult {
    pub passed: bool,
    pub status: WorkflowRunStatus,     // Success | Partial | Failed
    pub missing_required: Vec<String>,
    pub failed_critical: Vec<String>,
    pub degraded: bool,
    pub message: String,
}
```

Evaluation logic:
- `require_tasks`: Steps that must succeed for the gate to pass
- `allow_partial`: When true, missing requirements do not fail the gate (partial success)
- `degraded_visible`: Surfaces degradation signal from step outputs
- Critical step failures always fail the gate regardless of `allow_partial`

Maps to PEARL's Assurance Engine - separating "execution finished" from "verified success" with quality gate evaluation.

#### Policy Guard with Command and Path Validation

`src/policy.rs` implements security boundaries:

```rust
pub struct PolicyGuard {
    config: PolicyConfig,
    workspace_root: PathBuf,
}

impl PolicyGuard {
    pub fn validate_command(&self, command: &str) -> Result<()> { ... }
    pub fn validate_path(&self, path: &str) -> Result<PathBuf> { ... }
}
```

Key features:
- **Command blocking**: Blocked command patterns matched case-insensitively
- **Command allowlist**: If non-empty, only allowlisted command prefixes are permitted
- **Path validation**: Resolves paths against workspace root, rejects traversal (`..` escaping workspace boundary)
- **Deny paths**: Explicit path patterns that are always rejected
- **Secret redaction**: `sanitize_log()` uses regex to redact sensitive patterns before logging

Maps to PEARL's Guard Engine and Constitution Article 7 (Guard Fail-Closed) with workspace boundary enforcement.

---

### B.3 daily_mistral: Planner-Executor with Plan Budget, Event Logger, Loop Engine, and Output Validator

The daily_mistral Python system provides the **Planner-Executor reference** for PEARL's plan generation, budget management, quality refinement, and observability patterns.

#### Planner-Executor with Typed Actions and Plan Budget

`src/daily_mistral/planner_executor.py` implements the separation:

```python
TOOL_ACTIONS = frozenset({"search", "fetch_article"})
LLM_ACTIONS = frozenset({"llm_extract", "llm_compare", "llm_outline", "llm_write_section", "llm_verify"})
ACTION_WHITELIST = TOOL_ACTIONS | LLM_ACTIONS

@dataclass
class PlanBudget:
    max_steps: int = 8
    max_llm_calls: int = 16
    max_search_queries: int = 5
    max_replan: int = 1
```

Key design principle: "The model (Mistral) never acts directly (no tool capability); all tool steps (search/fetch_article) are executed by Python. The model only declares what to do and synthesizes text."

`PlannerExecutorResult` carries: `status` ("success" | "partial" | "downgrade" | "failed"), `final_text`, `plan`, `execution_state`, `llm_calls_used`, `replanned`.

**DAG Validation**: `topological_order(steps)` validates step dependency graphs before execution, ensuring no cycles and proper ordering.

Maps to PEARL's Plan Compiler (validates plans against budget/capability), Planner-Executor separation (Constitution Article 1: model declares, engine executes), and precision classification (TOOL_ACTIONS = P0, LLM_ACTIONS = P2).

#### JSONL Event Logger with Atomic Append

`src/daily_mistral/events.py` implements structured observability:

```python
VALID_STEPS = frozenset({"select", "fetch", "sanitize", "build", "llm_call", "validate", "repair", "write", "memory", "notify", "kb_import", "done", "plan", "exec_step", "replan", "synthesize", "loop_iter"})
VALID_STATUSES = frozenset({"ok", "error", "skip"})

def generate_run_id(task_key: str, now: datetime) -> str:
    """<YYYYMMDD>-<HHmm>-<task_key>-<6位hex>"""
    return f"{now:%Y%m%d}-{now:%H%M}-{task_key}-{secrets.token_hex(3)}"
```

`EventLogger` writes JSONL with:
- Single `write()` call per event (atomic under POSIX PIPE_BUF / Windows guarantees)
- Pre-serialization to complete string before writing (no partial lines)
- Validated step and status values (rejects unknown values)
- Daily file rotation (`logs/events-YYYYMMDD.jsonl`)
- Run ID with 24-bit random suffix for collision resistance

Maps to PEARL's Event Ledger append-only requirement and structured observability. The validated step/status enum pattern maps to PEARL's typed event system.

#### Loop Engine for Quality Refinement (Evaluator-Optimizer)

`src/daily_mistral/loop_engine.py` implements the L3 quality refinement cycle:

```python
@dataclass
class CriticScore:
    score_total: float
    dimensions: dict[str, float]
    defects: list[dict]

@dataclass
class RefinementResult:
    final_draft: str
    final_score: float
    iterations: list[RefinementIteration]
    converged: bool
    stop_reason: str  # "threshold_met" | "no_improvement" | "budget_exhausted" | "deadline"
```

Anti-oscillation guards (all deterministic logic, no LLM):
- **Best-version retention**: Final output is always the highest-scoring iteration, not the last
- **Monotonic improvement check**: If `new_score <= prev_score + min_improvement`, stops early (no improvement)
- **Deadline awareness**: Checks `minutes_remaining_fn()` before each iteration, stops if insufficient time
- **Budget sharing**: Calls count against `remaining_budget_calls` passed from caller

This is a pure function (no I/O; critic_fn/rewrite_fn injected by caller). Maps to PEARL's quality verification loop and OODA governance with deterministic stopping criteria.

#### Output Validator with JSON Schema

`src/daily_mistral/output_validator.py` provides:
- JSON extraction from mixed text/code-block outputs
- `jsonschema` validation against declared schemas
- Simplified Chinese detection (language compliance)
- Placeholder detection (catches unfilled template markers)
- URL validation
- Repair feedback generation for re-prompting on validation failure

Maps to PEARL's Assurance Engine verifier plugins - machine-checkable output validation before declaring success.

---

### B.4 llamaindex_daily: Workflow Orchestrator with DAG Execution, Policy Engine, and Scheduler

The llamaindex_daily Python system provides the **workflow orchestration reference** for PEARL's parallel task execution, RBAC policy enforcement, and scheduling.

#### Workflow Orchestrator with Dependency DAG and Parallel Execution

`src/core/workflow.py` implements:

```python
class WorkflowStatus(str, Enum):
    PENDING = "pending"
    RUNNING = "running"
    SUCCESS = "success"
    PARTIAL = "partial"
    FAILED = "failed"
    CANCELLED = "cancelled"

class TaskDef(BaseModel):
    id: str
    name: str
    handler: Optional[Callable]
    depends_on: list[str]
    timeout_sec: int = 300
    retries: int = 1
    skip_on_error: bool = False
    critical: bool = False
    config: dict[str, Any]
```

Key design patterns:
- **Dependency DAG**: `depends_on` field creates execution ordering; tasks with satisfied deps execute in parallel
- **concurrent.futures parallel execution**: `max_parallel` controls thread pool size
- **Critical task distinction**: `critical: bool` determines whether failure propagates to workflow level
- **Degradation signaling**: `TaskResult.degraded: bool` surfaces partial success (LLM fallback signal)
- **Error classification**: `TaskResult.error_code: Optional[str]` enables machine-routable error handling
- **Skip-on-error**: Non-critical tasks can be skipped without failing the workflow

`WorkflowResult` aggregates: `workflow_id`, `status` (WorkflowStatus), per-task results map, timing, error.

Maps to PEARL's workflow orchestration with dependency resolution, parallel execution, and the distinction between recoverable and permanent failures.

#### Policy Engine with RBAC

`src/policy_engine/policy.py` implements role-based access control:

```python
class Role(str, Enum):
    ADMIN = "admin"
    STAFF = "staff"
    GUEST = "guest"

class RiskLevel(str, Enum):
    LOW = "low"
    MEDIUM = "medium"
    HIGH = "high"
    CRITICAL = "critical"

class PolicyRule(BaseModel):
    role: Role
    allowed_tools: list[str]
    allowed_risk_levels: list[RiskLevel]
    max_retries: int = 3
    timeout_sec: int = 300
    require_approval: bool = False
```

`PolicyEngine` loads rules from JSON config, with defaults for each role:
- ADMIN: all tools, all risk levels, no approval required
- STAFF: limited tools, up to HIGH risk, approval for critical
- GUEST: minimal tools, LOW risk only, approval always required

Maps to PEARL's Policy Engine for per-task capability restrictions and the RBAC model for approval-gated operations (Constitution Article 11: Autonomy vs Verifiability).

#### Scheduler

`src/scheduler/__init__.py` exports `Scheduler` and `ScheduledJob` for job scheduling integration, complementing the workflow orchestrator with time-based triggering.

---

### B.5 daily-digest-prompt (DDP) + small_daily_tasks: Production Evidence Source

DDP is the **primary evidence source** - a production system with 183 YAML configs, 775 tools, 264 skills, 20+ workflows, and 904 state files running autonomously.

#### YAML Single-Source-of-Truth (from DDP production evidence)

DDP's `config/scoring.yaml` demonstrates the SoT pattern with:

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

The `allowed_tools_table` demonstrates capability-level access control:
```yaml
read_only: "Read,Bash"
full_dev: "Read,Bash,Write,Edit"
research_kb: "Read,Bash,Write,WebSearch,WebFetch"
```

Maps to PEARL's Policy Engine for per-task capability restrictions.

#### Hooks and Guards (from small_daily_tasks)

`small_daily_tasks/hooks/pre_bash_guard.py` implements a comprehensive command guard derived from DDP:

1. **YAML-driven rules with fallback**: Loads from `config/hook-rules.yaml`; falls back to built-in `FALLBACK_RULES` on missing/corrupt YAML
2. **Cloud LLM API blocking** (6 hosts): `api.anthropic.com`, `api.openai.com`, `generativelanguage.googleapis.com`, `api.mistral.ai`, `api.groq.com`, `integrate.api.nvidia.com`
3. **ReDoS protection**: `MAX_COMMAND_CHARS = 16_384` - rejects commands exceeding limit before regex evaluation
4. **Structured decision output**: `output_decision("block", reason)` for machine-parseable guard responses
5. **Fail-open on exceptions**: Unlike DDP's fail-closed guards, this variant uses fail-open for development flexibility
6. **Guard tags**: `cloud-api-guard`, `nul-guard`, `safety-guard`

Pattern: `read_stdin_json() -> check rules -> output_decision()` - maps to PEARL's Guard Engine execution chain: `Request -> Pre Guard -> Execution -> Post Guard -> Verification`

This directly maps to PEARL Constitution Article 7 (Guard Fail-Closed) and demonstrates the Guard Engine pattern.

#### DDP Skill/Tool/Workflow Scale

Production evidence of the ecosystem PEARL must accommodate:
- **264 skills** in `skills/` (e.g., `academic-paper-research`, `arch-evolution`, `auto-task-creator`)
- **775 tools** in `tools/` (Python CLI tools following JSON stdin/stdout contract)
- **20+ workflows** in `workflows/` (YAML-defined execution sequences)
- **183 YAML configs** in `config/` (scoring, routing, cache-policy, budget, timeouts, frequency-limits)

#### Workflow Index (DDP Capability Registry)

`workflows/index.yaml` (version 1.2.4, 30+ entries) demonstrates:
- Typed entries: `workflow_yaml`, `validation_checklist`, `output_schema`, `tool`
- Task-type routing: each entry has `task_types` (e.g., `["all"]`, `["system_insight", "self_heal"]`)
- Priority classification: P0 (mandatory) vs P1 (recommended)
- Version tracking: `version`, `created_at`
- Dependency alignment: `alignment` field links to specific config files
- Read-when triggers: `read_when` field for conditional loading

Maps to PEARL's Capability Registry with types: `script`, `tool`, `verifier`, `skill`, `agent`, `workflow`, `runtime`, `guard`.

---

### B.6 AgentFlow-Notify: Specification-Driven Governance

The `speckit.constitution` file provides the **governance model** for PEARL's own development process.

#### Constitution Structure

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
| Event Lifecycle (typed events) | agent-dashboard `crates/agentflow-core/src/domain_event.rs` DomainEvent enum + EventEnvelope | Adapt event naming, adopt UUIDv7 + schema_version pattern |
| SQLite Event Store | agent-dashboard `crates/agentflow-store/src/event_store.rs` insert/query by trace_id | Direct-port append-only pattern with PEARL event types |
| Workflow State Machine | agent-dashboard `crates/agentflow-core/src/workflow_run.rs` WorkflowStatus with `can_transition_to()` | Extend with PEARL-specific states (LEASED, VERIFYING, VERIFIED_SUCCESS) |
| Step State Machine | agent-dashboard `crates/agentflow-core/src/step_run.rs` StepStatus + StepType + idempotency_key | Adapt with precision classification (P0-P3) mapped to StepType |
| Scheduler (cron/interval/slot) | daily_rust `src/scheduler/mod.rs` SchedulerEngine with cron, interval, slot modes | Adapt with additional trigger types (one-shot, conditional) |
| Process Supervisor (tree kill) | daily_rust `src/process/mod.rs` ProcessSupervisor trait + Unix/Windows impls | Direct-port for PEARL's runtime adapter process management |
| State Store + Task Locks | daily_rust `src/state/mod.rs` TaskLock with heartbeat, RunStatus, TaskRunRecord | Adapt for pearl-state with SQLite backend |
| Health Monitor (self-healing) | daily_rust `src/health/mod.rs` stale lock recovery, failure decay, profile escalation | Adapt for PEARL's OODA governance loop |
| Workflow Engine + Checkpoint | daily_rust `agentflow-harness-rust/src/workflow_engine.rs` + `checkpoint.rs` | Adapt checkpoint/resume for PEARL's Plan Executor |
| Quality Gates | daily_rust `agentflow-harness-rust/src/quality_gate.rs` QualityGateResult | Adapt as PEARL's Assurance Engine quality evaluation |
| Policy Guard | daily_rust `agentflow-harness-rust/src/policy.rs` PolicyGuard (command + path validation) | Adapt for PEARL's Guard Engine with workspace boundary |
| Planner-Executor separation | daily_mistral `src/daily_mistral/planner_executor.py` PlanBudget + action whitelist | Reference for PEARL's Planner/Plan Compiler/Executor separation |
| DAG Validation | daily_mistral `src/daily_mistral/planner_executor.py` topological_order() | Direct-port for plan dependency validation |
| Event Logger (atomic JSONL) | daily_mistral `src/daily_mistral/events.py` EventLogger with validated steps/statuses | Reference for PEARL's structured event format |
| Quality Refinement Loop | daily_mistral `src/daily_mistral/loop_engine.py` refine() with anti-oscillation | Reference for PEARL's OODA quality gates |
| Output Validation | daily_mistral `src/daily_mistral/output_validator.py` jsonschema + repair | Adapt as PEARL Assurance Engine verifier plugin |
| Workflow Orchestrator (DAG) | llamaindex_daily `src/core/workflow.py` WorkflowDef + parallel execution | Reference for PEARL's workflow execution model |
| RBAC Policy Engine | llamaindex_daily `src/policy_engine/policy.py` Role + RiskLevel + PolicyRule | Adapt for PEARL's Policy Engine per-capability rules |
| Scoring/Routing SoT | DDP `config/scoring.yaml` (formula, priority_scores, multipliers) | Migrate as first P0 Mechanical Script |
| Guard Engine | small_daily_tasks `hooks/pre_bash_guard.py` (YAML rules, fail-open, cloud API block) | Port guard model to Rust pre/post middleware |
| Capability Index | DDP `workflows/index.yaml` (typed, versioned, task_types routing) | Evolve into unified Capability Registry |
| Governance Model | AgentFlow-Notify `speckit.constitution` (spec-first sequence) | Adopt for PEARL development process |

### C.2 What PEARL v2 Must Build New

| Component | Gap Description | Complexity |
|---|---|---|
| **Precision Decision Engine (P0-P3)** | Novel concept - no reference has step-level classification before execution; agent-dashboard's StepType provides a starting taxonomy | High |
| **Plan Compiler** | No reference validates execution plans against policy/budget/capability/verifier presence; daily_mistral validates DAG and budget separately | High |
| **Assurance Engine** | No reference separates "execution finished" from "verified success" with pluggable verifiers; quality gates come closest | Medium |
| **Durable Task State Machine** | CREATED->PLANNING->PLANNED->READY->LEASED->RUNNING->VERIFYING->VERIFIED_SUCCESS; extends agent-dashboard's model significantly | Medium |
| **Lease + Heartbeat + Reaper** | daily_rust has TaskLock with heartbeat but not formal lease semantics with reclamation | Medium |
| **Constitution CI Gate** | Automated enforcement of 12 articles (no side effect without idempotency, etc.) | Medium |
| **OODA Governance Loop** | DDP has informal OODA; daily_rust health monitor provides escalation; PEARL needs Observe(machine) -> Orient(hybrid) -> Decide(policy) -> Act(transactional) | Medium |
| **Repair Transaction** | Isolated workspace -> apply -> verify -> promote/rollback for self-heal | Low-Medium |
| **Runtime Profile** (NORMAL/DEGRADED/RECOVERY/EMERGENCY) | daily_rust has profile degradation; needs formalization with control over concurrency/budget/effects | Low |
| **Multi-Runtime Adapter** | Unified contract for Rust/Python/PowerShell/Shell/Claude/Codex/Cursor/LLM API; daily_rust has single-runtime supervisor | Medium |
| **Durable Queue with Retry** | No reference implements a full durable queue with dead-letter semantics | Medium |

### C.3 Critical Gaps by Constitution Article

| Article | Gap |
|---|---|
| Art. 1 (Determinism First) | Need P0 classifier to prevent LLM involvement in computable work |
| Art. 2 (Machine Verifier) | Need UNVERIFIED state + verifier registry; no reference implements this |
| Art. 4 (Provable Success) | Need Evidence model + evidence_required field per step |
| Art. 5 (Idempotency) | agent-dashboard has idempotency_key on StepRun; need infrastructure for all effects |
| Art. 6 (Persistent State) | agent-dashboard demonstrates SQLite event store; need full materialized view rebuild |
| Art. 7 (Guard Fail-Closed) | small_daily_tasks/DDP has implementation; needs Rust port + hook vs guard distinction |
| Art. 8 (LLM Cannot Self-Verify) | Need mandatory script verifier in execution chain |
| Art. 9 (Cancellable Runtime) | daily_rust has ProcessSupervisor; need unified cancel/timeout/cleanup contract per runtime |
| Art. 10 (Single SoT) | Need Config Resolution with revision tracking (config_revision, config_hash) |
| Art. 11 (Autonomy vs Verifiability) | llamaindex_daily has RBAC; need runtime enforcement of autonomy level based on verification coverage |
| Art. 12 (ADR for Architecture) | AgentFlow-Notify has governance model; need ADR workflow with Finding -> Proposal -> ADR -> Verification -> Promotion |

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
- [ ] Set up Cargo workspace (reference: agent-dashboard `Cargo.toml` multi-crate pattern)
- [ ] Implement `pearl-core`: config loading (reference: daily_rust `src/config/mod.rs` version-gate pattern)
- [ ] Implement `pearl-state`: StateStore with task locks and heartbeat (reference: daily_rust `src/state/mod.rs`)
- [ ] Implement `pearl-events`: SQLite Event Ledger (reference: agent-dashboard `crates/agentflow-store/src/event_store.rs`)
- [ ] Implement task state machine with transition validation (reference: agent-dashboard `crates/agentflow-core/src/workflow_run.rs`)
- [ ] Implement lease + heartbeat (reference: daily_rust `src/state/mod.rs` TaskLock)
- [ ] Implement basic worker with process supervision (reference: daily_rust `src/process/mod.rs` ProcessSupervisor)
- **No LLM** - pure mechanical kernel

### Phase 2: Mechanical Runtime (Week 7-10)
- [ ] Implement Precision Decision Engine (P0 classification)
- [ ] Port script runtime adapters (Rust/Python/PowerShell/Shell)
- [ ] Implement Capability Manifest and Registry (reference: DDP `workflows/index.yaml`)
- [ ] Implement Guard Engine (reference: small_daily_tasks `hooks/pre_bash_guard.py` + daily_rust/agentflow-harness-rust `src/policy.rs`)
- [ ] Implement Scheduler (reference: daily_rust `src/scheduler/mod.rs` cron + interval + slot)
- [ ] Port Health Monitor with profile degradation (reference: daily_rust `src/health/mod.rs`)
- [ ] Migrate DDP scoring/routing/health-check scripts as first P0 capabilities

### Phase 3: Capability Import (Week 11-14)
- [ ] Implement PythonCapabilityAdapter (CLI + JSON stdout contract)
- [ ] Import DDP's 775 Python tools without rewriting
- [ ] Implement Assurance Engine with pluggable verifiers (reference: daily_rust/agentflow-harness-rust `src/quality_gate.rs`)
- [ ] Implement Evidence model
- [ ] Implement Plan Compiler with DAG validation (reference: daily_mistral `planner_executor.py` topological_order)

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

**Decision**: Multi-crate Cargo workspace following agent-dashboard and daily_rust patterns.

**Evidence**: agent-dashboard `Cargo.toml` demonstrates successful separation:
- Core (domain events, state machines) vs Store (SQLite persistence) vs Runtime vs Application
- Shared workspace dependencies with version pinning
- `resolver = "2"` for modern dependency resolution

daily_rust `Cargo.toml` extends this with:
- Scheduler + Process Supervisor + State + Health in separate modules
- Sub-crate (`agentflow-harness-rust`) for workflow execution engine

**PEARL mapping** (from spec Section 57):
```
pearl/
  crates/
    pearl-core/         # config, paths, error, utility
    pearl-events/       # SQLite event ledger (ref: agentflow-store)
    pearl-state/        # durable state store (ref: daily_rust state)
    pearl-queue/        # durable work queue
    pearl-lease/        # worker lease + heartbeat (ref: daily_rust TaskLock)
    pearl-scheduler/    # cron/interval/one-shot scheduling (ref: daily_rust scheduler)
    pearl-planner/      # plan generation
    pearl-plan-compiler/# plan validation (ref: daily_mistral topological_order)
    pearl-executor/     # plan execution (ref: agentflow-harness-rust workflow_engine)
    pearl-assurance/    # verification engine (ref: quality_gate)
    pearl-precision/    # P0-P3 classification
    pearl-policy/       # policy engine (ref: llamaindex_daily policy_engine)
    pearl-guard/        # pre/post guards (ref: small_daily_tasks pre_bash_guard)
    pearl-router/       # capability + backend routing
    pearl-workflow/     # declarative + dynamic workflows
    pearl-runtime/      # runtime adapter contracts
    pearl-process-supervisor/  # process tree management (ref: daily_rust process)
    pearl-capabilities/ # capability registry
    pearl-evidence/     # evidence model
    pearl-governance/   # OODA, ADR, repair transactions
```

**Key dependencies** (from agent-dashboard + daily_rust workspaces):
- `tokio` (async runtime with process/signal support)
- `serde` + `serde_json` + `serde_yaml` (serialization)
- `chrono` + `chrono-tz` (time handling)
- `rusqlite` (bundled, chrono, uuid features - SQLite event store)
- `uuid` (v7 for time-sortable identifiers)
- `clap` (CLI)
- `tracing` + `tracing-subscriber` (structured logging)
- `cron` (schedule parsing)
- `nix` (Unix process group management)
- `axum` (HTTP server for dashboard/API)
- `tempfile` (atomic file operations)

### E.2 SQLite/WAL for Event Ledger

**Decision**: SQLite with WAL mode for the Event Ledger and materialized state.

**Rationale**:
- Spec Section 42 requires append-only event storage with typed events
- Spec Section 43 requires materialized views (tasks, runs, attempts, leases, etc.)
- agent-dashboard proves SQLite event store works with `rusqlite` (bundled, no external deps)
- daily_rust's file-based state (JSON per task) shows limitations at scale (no query capability)
- SQLite provides ACID guarantees without external dependencies
- WAL mode enables concurrent readers with single writer

**Evidence**: agent-dashboard `crates/agentflow-store/src/event_store.rs` demonstrates the exact pattern: typed events serialized to JSON payload, stored in `domain_events` table, queryable by `trace_id` with temporal ordering.

**Schema design principle**: Events table is append-only (the audit trail); materialized tables are rebuilt from events (replay capability per spec Section 61).

### E.3 Event Sourcing Approach

**Decision**: Event Ledger as source of truth; materialized tables for queries.

**Evidence supporting this**:
- agent-dashboard implements typed domain events with UUIDv7 ordering and trace correlation
- daily_mistral EventLogger implements append-only JSONL with validated event types
- daily_rust StateStore persists TaskRunRecord per execution (event-per-run pattern)
- Spec Section 42 explicitly lists event types: `task.created`, `task.planned`, `task.leased`, etc.

**Implementation**:
- Write: append event to `events` table (immutable) - pattern from agent-dashboard
- Read: query materialized tables (tasks, runs, attempts, etc.)
- Recovery: replay events from last checkpoint to rebuild materialized state
- Audit: verify event chain integrity (adopt schema_version from agent-dashboard)

### E.4 Script-First Routing

**Decision**: Router always checks for mechanical capability before agent routing.

**Evidence**:
- DDP `config/scoring.yaml` demonstrates that task scoring is pure computation (formula with multipliers)
- daily_mistral separates TOOL_ACTIONS (mechanical) from LLM_ACTIONS (requiring model) with explicit whitelist
- agent-dashboard `StepType::Deterministic` provides type-level classification
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
- small_daily_tasks `hooks/pre_bash_guard.py` implements YAML-driven rule matching with cloud API blocking
- daily_rust/agentflow-harness-rust `src/policy.rs` PolicyGuard implements command blocking + path validation
- Spec Section 46 defines: `Request -> Pre Guard -> Execution -> Post Guard -> Verification`

**Guard types** (from small_daily_tasks + DDP evidence):
- `cloud-api-guard`: blocks cloud LLM API calls (6 hosts)
- `nul-guard`: blocks Windows nul redirect
- `safety-guard`: destructive operations
- `git-guard`: force push / --no-verify protection
- `env-guard`: secret access
- `exfiltration-guard`: data leak prevention
- `redos-input-guard`: input validation (MAX_COMMAND_CHARS = 16_384)

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

- agent-dashboard: `Cargo.toml` (workspace structure with rusqlite, uuid v7, axum, tokio)
- agent-dashboard: `crates/agentflow-core/src/domain_event.rs` (typed DomainEvent enum + EventEnvelope)
- agent-dashboard: `crates/agentflow-core/src/workflow_run.rs` (WorkflowStatus state machine)
- agent-dashboard: `crates/agentflow-core/src/step_run.rs` (StepStatus, StepType, idempotency_key)
- agent-dashboard: `crates/agentflow-store/src/event_store.rs` (SQLite insert/query)
- daily_rust: `src/scheduler/mod.rs` (SchedulerEngine with cron, interval, slot, profile cap)
- daily_rust: `src/process/mod.rs` (ProcessSupervisor trait, Unix/Windows implementations)
- daily_rust: `src/state/mod.rs` (StateStore, TaskLock, TaskRunRecord, RunStatus)
- daily_rust: `src/health/mod.rs` (HealthMonitor, stale lock recovery, failure decay, profile escalation)
- daily_rust/agentflow-harness-rust: `src/workflow_engine.rs` (WorkflowEngine, checkpoint/resume, degraded fallback)
- daily_rust/agentflow-harness-rust: `src/checkpoint.rs` (WorkflowCheckpoint, save/load)
- daily_rust/agentflow-harness-rust: `src/quality_gate.rs` (QualityGateResult, evaluate_quality_gate)
- daily_rust/agentflow-harness-rust: `src/policy.rs` (PolicyGuard, validate_command, validate_path)
- daily_mistral: `src/daily_mistral/planner_executor.py` (PlanBudget, topological_order, action whitelist)
- daily_mistral: `src/daily_mistral/events.py` (EventLogger, generate_run_id, atomic append JSONL)
- daily_mistral: `src/daily_mistral/loop_engine.py` (refine(), CriticScore, anti-oscillation)
- daily_mistral: `src/daily_mistral/output_validator.py` (jsonschema validation, repair feedback)
- llamaindex_daily: `src/core/workflow.py` (WorkflowDef, TaskDef, parallel execution, degraded signal)
- llamaindex_daily: `src/policy_engine/policy.py` (PolicyEngine, Role, RiskLevel, PolicyRule)
- small_daily_tasks: `hooks/pre_bash_guard.py` (YAML rules, cloud LLM blocking, MAX_COMMAND_CHARS)
- daily-digest-prompt: `config/scoring.yaml`, `workflows/index.yaml`, `skills/`, `tools/`
- AgentFlow-Notify: `speckit.constitution` (spec-first governance, required coverage, change control)
