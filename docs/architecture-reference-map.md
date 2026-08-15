# PEARL v2 Architecture Reference Map

This document maps each PEARL v2 component (from Section 16 architecture diagram) to specific files, patterns, and implementations found in the reference projects: agent-dashboard, daily_rust (including agentflow-harness-rust sub-crate), daily_mistral, llamaindex_daily, daily-digest-prompt (DDP), small_daily_tasks, AgentFlow-Notify, and RAG_Skill.

---

## Legend

| Column | Description |
|--------|-------------|
| **PEARL Component** | Architecture layer or module from the spec |
| **Reference Project** | Source repository providing the pattern |
| **File/Module** | Specific file path with evidence |
| **Pattern/Evidence** | What was found and how it maps |
| **Reuse Level** | `direct-port` / `adapt` / `reference-only` / `new-build` |

---

## 1. PEARL Constitution Layer

| PEARL Component | Reference Project | File/Module | Pattern/Evidence | Reuse Level |
|---|---|---|---|---|
| Constitution Enforcement | AgentFlow-Notify | `speckit.constitution` | 8-section governance: spec-first sequence, required coverage areas (delivery, retry, observability, adapter boundary), change control, bootstrap status | adapt |
| Constitution CI Gate | small_daily_tasks | `hooks/pre_bash_guard.py` | Machine enforcement pattern: YAML-driven rules with `FALLBACK_RULES` as built-in defaults; `output_decision("block", reason)` structured response | adapt |
| ADR Process | AgentFlow-Notify | `speckit.constitution` Section 7 | "When requirements change after spec/plan/tasks are finalized, the affected artifact must be revised and re-approved in the same order" | adapt |

---

## 2. Control Plane

| PEARL Component | Reference Project | File/Module | Pattern/Evidence | Reuse Level |
|---|---|---|---|---|
| Scheduler (cron) | daily_rust | `src/scheduler/mod.rs` | `SchedulerEngine` with `ScheduleKind::Cron(expr)`, timezone-aware (`chrono_tz`), same-trigger-point deduplication via `write_cron_last()`, lookback window of 60s to prevent replay | direct-port |
| Scheduler (interval) | daily_rust | `src/scheduler/mod.rs` | `ScheduleKind::Interval(seconds)` with elapsed-time tracking via `read_interval_last()` / `write_interval_last()` | direct-port |
| Scheduler (slot) | daily_rust | `src/scheduler/mod.rs` | Slot-group scheduling with `current_slot()`, `select_for_slot()`, per-day dedup via `slot_already_fired()` | adapt |
| Scheduler (lifecycle) | llamaindex_daily | `src/scheduler/__init__.py` | `Scheduler` and `ScheduledJob` exports for APScheduler-based job management | reference-only |
| Planner | daily_mistral | `src/daily_mistral/planner_executor.py` | Planner-Executor separation: "model only declares what to do"; `PlanBudget` with `max_steps`, `max_llm_calls`, `max_search_queries`, `max_replan` | adapt |
| Router | daily-digest-prompt | `config/scoring.yaml` | `confidence_multipliers: {tier1: 1.0, tier2: 0.8, tier3: 0.6}` - three-tier routing confidence; `allowed_tools_table` maps task type to tool permissions | adapt |
| Router (mental model) | daily-digest-prompt | `config/scoring.yaml` | `mental_model_routing.models` with keyword-triggered prompt injection (`first_principles`, `inversion`, `bayesian`, `analogical`, `systems_thinking`, `metacognition`) | reference-only |
| Budget | daily_mistral | `src/daily_mistral/planner_executor.py` | `PlanBudget(max_steps=8, max_llm_calls=16, max_search_queries=5, max_replan=1)` with enforcement during execution | adapt |
| Policy | llamaindex_daily | `src/policy_engine/policy.py` | `PolicyEngine` with `PolicyRule` per role: `allowed_tools`, `allowed_risk_levels`, `max_retries`, `timeout_sec`, `require_approval` | adapt |
| SLO | daily-digest-prompt | `workflows/index.yaml` | Priority P0/P1 classification per workflow; success rate targets in system-success-rate-improvement workflow entry | reference-only |

---

## 3. Precision Decision Engine (P0-P3)

| PEARL Component | Reference Project | File/Module | Pattern/Evidence | Reuse Level |
|---|---|---|---|---|
| P0 Scoring (Mechanical) | daily-digest-prompt | `config/scoring.yaml` | Full deterministic formula: `priority_scores` x `confidence_multipliers` x `description_bonus` x `time_proximity_bonus` x `label_count_bonus` x `recency_penalty` x `time_fatigue_factor` x `citizen_impact_bonus` | direct-port |
| P0 Tiebreaker | daily-digest-prompt | `config/scoring.yaml` | `tiebreaker.order: [due_time_asc, priority_desc, label_count_desc, task_id_asc]` - fully deterministic ordering | direct-port |
| P0 Config Validation | daily_rust | `src/config/mod.rs` | `SchedulerConfig` with version gate validation; `validate()` method rejects invalid configs; `normalize_cron_expression()` | direct-port |
| Step Type Classification | agent-dashboard | `crates/agentflow-core/src/step_run.rs` | `StepType { Tool, Agent, Notification, Approval, Deterministic }` - type-level classification before execution | adapt |
| Action Whitelist | daily_mistral | `src/daily_mistral/planner_executor.py` | `ACTION_WHITELIST = TOOL_ACTIONS | LLM_ACTIONS` - only declared action types allowed in plans; `topological_order()` validates DAG | adapt |
| Classification Logic | (none) | -- | No reference implements full step-level P0/P1/P2/P3 classification before execution with precision scoring | new-build |

---

## 4. Durable Work Plane

| PEARL Component | Reference Project | File/Module | Pattern/Evidence | Reuse Level |
|---|---|---|---|---|
| Event Store (append-only) | agent-dashboard | `crates/agentflow-store/src/event_store.rs` | `insert()` appends EventEnvelope to `domain_events` table; `list_by_trace()` retrieves by trace_id ordered by `occurred_at ASC` | direct-port |
| Event Envelope | agent-dashboard | `crates/agentflow-core/src/domain_event.rs` | `EventEnvelope { id: Uuid (v7), schema_version: u32, occurred_at, trace_id, event }` - time-sortable, versioned, trace-correlated | direct-port |
| Task Lock (Lease) | daily_rust | `src/state/mod.rs` | `TaskLock { task_id, run_id, pid, started_at, heartbeat_at }`; `acquire_lock()` / `release_lock()`; stale detection by health monitor | direct-port |
| Heartbeat | daily_rust | `src/state/mod.rs` | `update_heartbeat()` updates `RuntimeState.heartbeat_at`; `TaskLock.heartbeat_at` for per-task liveness | direct-port |
| Task Run Record | daily_rust | `src/state/mod.rs` | `TaskRunRecord { task_id, run_id, status: RunStatus, attempt, started_at, finished_at, exit_code, pid, fallback_from, message }` | direct-port |
| Workflow Run | agent-dashboard | `crates/agentflow-core/src/workflow_run.rs` | `WorkflowRun { id, workflow_id, workflow_version, status, created_at, started_at, completed_at, trace_id, label }` with `transition()` validation | adapt |
| Step Run | agent-dashboard | `crates/agentflow-core/src/step_run.rs` | `StepRun { id, workflow_run_id, step_id, step_type, status, attempt, idempotency_key, output, error_message }` | adapt |
| Checkpoint (Resume) | daily_rust/agentflow-harness-rust | `src/checkpoint.rs` | `WorkflowCheckpoint { run_id, workflow_id, job_id, stages_completed, completed_step_ids, context, partial_report }` with `save_checkpoint()` / `load_checkpoint()` | direct-port |
| Queue/Retry | (none) | -- | No reference implements a full durable queue with dead-letter semantics | new-build |
| Cooldown | daily_rust | `src/state/mod.rs` | `TaskMeta.last_auto_retry_date` prevents more than one reconcile-driven retry per day | adapt |

---

## 5. Planner-Executor

| PEARL Component | Reference Project | File/Module | Pattern/Evidence | Reuse Level |
|---|---|---|---|---|
| Workflow Engine | daily_rust/agentflow-harness-rust | `src/workflow_engine.rs` | `WorkflowEngine.run()` with `WorkflowRunOptions { run_id, job_id, resume, state_dir, fallback_workflow, auto_degraded_fallback }`; DAG-based stage execution via `JoinSet` + `Semaphore` | adapt |
| Degraded Fallback | daily_rust/agentflow-harness-rust | `src/workflow_engine.rs` | `degraded_workflow_for(primary_id)` mapping; automatic fallback when primary fails quality gate; single notification per chain | adapt |
| Workflow Status | agent-dashboard | `crates/agentflow-core/src/workflow_run.rs` | `WorkflowStatus { Pending, Queued, Running, Waiting, WaitingApproval, Retrying, Degraded, Completed, Failed, Cancelled }` with `can_transition_to()` | adapt |
| Workflow Result | agent-dashboard + daily_rust/agentflow-harness-rust | `workflow_run.rs` + `quality_gate.rs` | WorkflowStatus terminal states (Completed/Failed/Cancelled) + `QualityGateResult { passed, status, missing_required, failed_critical, degraded, message }` | adapt |
| Error Codes | agent-dashboard | `crates/agentflow-core/src/step_run.rs` + daily_rust `src/state/mod.rs` | `StepStatus` (Failed/Skipped/Cancelled) + `RunStatus` (Failed/Timeout/Killed) - machine-routable error classification | adapt |
| Plan Budget Validation | daily_mistral | `src/daily_mistral/planner_executor.py` | `PlanBudget` enforcement: `max_steps`, `max_llm_calls`, `max_search_queries`, `max_replan`; exceeding budget triggers status "downgrade" | adapt |
| DAG Validation | daily_mistral | `src/daily_mistral/planner_executor.py` | `topological_order(steps)`: validates dependency graph, detects cycles, returns execution order or None | direct-port |
| Plan Compiler | (none) | -- | No reference validates plans against policy/budget/capability/verifier presence as unified validation | new-build |
| Assurance Engine | (none) | -- | No reference separates execution completion from verified success with pluggable verifiers | new-build |

---

## 6. Execution Runtime

| PEARL Component | Reference Project | File/Module | Pattern/Evidence | Reuse Level |
|---|---|---|---|---|
| Process Supervisor (trait) | daily_rust | `src/process/mod.rs` | `ProcessSupervisor` trait: `spawn()`, `graceful_stop()`, `force_kill_tree()`, `is_alive()`, `try_wait()` | direct-port |
| Process Supervisor (Unix) | daily_rust | `src/process/unix_group.rs` | `UnixProcessSupervisor` using `nix` crate for process group signals | direct-port |
| Process Supervisor (Windows) | daily_rust | `src/process/windows_job.rs` | `WindowsJobSupervisor` using Job Objects for kernel-level tree containment | direct-port |
| Script Runtime (Python) | small_daily_tasks | `hooks/pre_bash_guard.py` | Python CLI pattern: `read_stdin_json()` -> process -> `output_decision()` JSON output; `main()` entry point | reference-only |
| Timeout Policy | daily_rust/agentflow-harness-rust | `src/workflow_engine.rs` | `run_with_timeout(tool_timeout, ...)` per-attempt timeout; cooperative cancellation; critical steps get at least 1 retry (`critical_aware_retries`) | adapt |
| Command Validation | daily_rust/agentflow-harness-rust | `src/policy.rs` | `PolicyGuard.validate_command()`: blocked patterns (case-insensitive) + allowlist prefix matching | adapt |
| Path Validation | daily_rust/agentflow-harness-rust | `src/policy.rs` | `PolicyGuard.validate_path()`: workspace boundary enforcement, traversal prevention, deny_paths | adapt |
| Runtime Config | daily_rust | `src/config/mod.rs` | `RuntimeConfig { timezone, max_parallel_tasks, state_dir, log_dir, default_timeout_seconds, failure_decay_hours }` | adapt |
| Concurrency Control | daily_rust | `src/scheduler/mod.rs` | `Semaphore` (global cap) + `AtomicUsize` CAS (profile cap) + `InFlightGuard` (Drop-based release) | direct-port |
| Runtime Adapters (multi) | (none) | -- | No reference implements unified contract for Rust/Python/PowerShell/Shell/Claude/Codex/LLM API | new-build |

---

## 7. Capability Fabric

| PEARL Component | Reference Project | File/Module | Pattern/Evidence | Reuse Level |
|---|---|---|---|---|
| Capability Registry | daily-digest-prompt | `workflows/index.yaml` | Typed index (version 1.2.4): entries with `id`, `path`, `type` (workflow_yaml/validation_checklist/output_schema/tool), `task_types`, `priority` (P0/P1), `version`, `created_at`, `summary`, `read_when`, `alignment` | adapt |
| Capability Types | daily-digest-prompt | `workflows/index.yaml` | Evidence of types: `workflow_yaml`, `validation_checklist`, `output_schema`, `tool` - maps to PEARL's script/tool/verifier/skill/agent/workflow/runtime/guard | adapt |
| Capability Routing | daily-digest-prompt | `workflows/index.yaml` | `task_types` field routes capabilities: `["all"]` or specific types like `["system_insight", "self_heal"]` | adapt |
| Skill Structure | daily-digest-prompt | `skills/` directory | 264 skills (e.g., `academic-paper-research`, `arch-evolution`, `auto-task-creator`) each with `SKILL.md` manifest | reference-only |
| Tool Contract | small_daily_tasks | `hooks/pre_bash_guard.py` | Python tool pattern: stdin JSON -> processing -> stdout JSON (`output_decision()`); structured imports (`hook_utils`) | adapt |
| Tool Registry (typed) | daily_rust/agentflow-harness-rust | `src/workflow_engine.rs` | `TypedToolRegistry` with `execute(ToolExecutionRequest { tool, input }) -> ToolExecutionResponse { tool, success, output }` | adapt |
| Verifier Pattern | daily-digest-prompt | `workflows/index.yaml` | `wf-20260411-done-cert-validation`: DONE_CERT quality certification; `wf-20260329-error-object-schema`: error validation checklist | reference-only |
| Monorepo Packaging | RAG_Skill | `pnpm-workspace.yaml` + `package.json` | pnpm monorepo with `packages/*` glob; `@rag/server` and `@rag/web` scoped packages; Node >= 20 | reference-only |

---

## 8. Evidence / State / Memory Layer

| PEARL Component | Reference Project | File/Module | Pattern/Evidence | Reuse Level |
|---|---|---|---|---|
| Event Ledger (SQLite) | agent-dashboard | `crates/agentflow-store/src/event_store.rs` | `domain_events` table with columns: `id`, `schema_version`, `trace_id`, `occurred_at`, `event_type`, `payload`; indexed by trace_id for temporal queries | direct-port |
| Event Types (typed enum) | agent-dashboard | `crates/agentflow-core/src/domain_event.rs` | `DomainEvent` enum with serde tag: WorkflowRunCreated, WorkflowStatusChanged, StepRunCreated, StepStatusChanged, ArtifactStored, ApprovalRequested/Granted/Denied | direct-port |
| Event Logger (JSONL) | daily_mistral | `src/daily_mistral/events.py` | `EventLogger`: append-only JSONL, single write() call per event (atomic), daily rotation, validated `VALID_STEPS`/`VALID_STATUSES`, `generate_run_id()` with 24-bit hex suffix | adapt |
| State Store | daily_rust | `src/state/mod.rs` | `StateStore { state_dir, log_dir }` with `ensure_dirs()`, structured subdirectories (task-locks, task-runs, last-success, last-failure, task-meta) | direct-port |
| Task Metadata | daily_rust | `src/state/mod.rs` | `TaskMeta { last_success_date, last_run_date, consecutive_failures, round_robin_counter, last_interval_run, last_cron_trigger }` | direct-port |
| Secret Redaction | daily_rust/agentflow-harness-rust | `src/policy.rs` | `sanitize_log()` uses regex to redact sensitive patterns before logging (referenced in PolicyGuard) | adapt |
| Retention/Cleanup | daily_rust | `src/state/mod.rs` | `cleanup_task_runs(keep_days)`: prunes old per-run records by mtime; `cleanup_logs(keep_days)`: prunes old log files | direct-port |
| SQLite Schema | agent-dashboard | `crates/agentflow-store/` | Schema: `id TEXT, schema_version INT, trace_id TEXT, occurred_at TEXT, event_type TEXT, payload TEXT`; versioned for forward compatibility | direct-port |
| Artifact Management | agent-dashboard | `crates/agentflow-core/src/domain_event.rs` | `ArtifactStored { workflow_run_id, step_run_id, artifact_id, name, kind: ArtifactKind }` event + typed artifact kinds | adapt |

---

## 9. Governance Layer

| PEARL Component | Reference Project | File/Module | Pattern/Evidence | Reuse Level |
|---|---|---|---|---|
| Guard Engine (Pre) | small_daily_tasks | `hooks/pre_bash_guard.py` | Pre-execution guard: regex pattern matching, structured decision output via `output_decision()`, YAML rule source with fallback to `FALLBACK_RULES` | adapt |
| Guard Rules (YAML) | small_daily_tasks | `hooks/pre_bash_guard.py` | Loads `config/hook-rules.yaml` with graceful fallback; rule fields: `id`, `pattern`, `flags`, `reason`, `tag` | adapt |
| Cloud API Guard | small_daily_tasks | `hooks/pre_bash_guard.py` | `CLOUD_LLM_HOSTS` (6 hosts): blocks curl/wget/Invoke-RestMethod to `api.anthropic.com`, `api.openai.com`, `api.mistral.ai`, `api.groq.com`, etc. | adapt |
| ReDoS Defense | small_daily_tasks | `hooks/pre_bash_guard.py` | `MAX_COMMAND_CHARS = 16_384`: rejects commands exceeding limit before regex evaluation | direct-port |
| Command Guard | daily_rust/agentflow-harness-rust | `src/policy.rs` | `PolicyGuard.validate_command()`: blocked command patterns (case-insensitive) + allowlist enforcement | adapt |
| Path Guard | daily_rust/agentflow-harness-rust | `src/policy.rs` | `PolicyGuard.validate_path()`: workspace root boundary, path traversal prevention, deny_paths list | adapt |
| Quality Gate | daily_rust/agentflow-harness-rust | `src/quality_gate.rs` | `evaluate_quality_gate()`: `require_tasks` + `allow_partial` + `degraded_visible`; returns `QualityGateResult { passed, status, missing_required, failed_critical }` | adapt |
| Quality Refinement | daily_mistral | `src/daily_mistral/loop_engine.py` | `refine()`: evaluator-optimizer loop with `CriticScore`, anti-oscillation (best-version retention, monotonic improvement, deadline, budget) | reference-only |
| Output Validation | daily_mistral | `src/daily_mistral/output_validator.py` | jsonschema validation, placeholder detection, Simplified Chinese detection, URL validation, repair feedback | adapt |
| RBAC Policy | llamaindex_daily | `src/policy_engine/policy.py` | `PolicyEngine` with `Role` (ADMIN/STAFF/GUEST), `RiskLevel` (LOW-CRITICAL), `PolicyRule` (allowed_tools, allowed_risk_levels, require_approval) | adapt |
| Profile Degradation | daily_rust | `src/health/mod.rs` | `check_repeated_failures()`: Normal -> Degraded (3+ failures) -> Recovery (5+ failures); auto-restore when failures decay | adapt |
| Stale Lock Recovery | daily_rust | `src/health/mod.rs` | `recover_stale_locks()`: checks `is_alive()` + timeout heuristic (2x timeout), then `force_kill_tree()` + `release_lock()` | direct-port |
| Audit Trail | agent-dashboard | `crates/agentflow-store/src/event_store.rs` | Append-only event store with temporal ordering; `list_by_trace()` retrieves complete audit history | direct-port |
| Repair Transaction | (none) | -- | No reference implements isolated workspace -> apply -> verify -> promote/rollback | new-build |
| Specification Governance | AgentFlow-Notify | `speckit.constitution` | Sections 3-7: spec-first governance, required coverage, planning requirements, task requirements, change control | adapt |

---

## 10. Cross-Cutting Infrastructure

| PEARL Component | Reference Project | File/Module | Pattern/Evidence | Reuse Level |
|---|---|---|---|---|
| Workspace Layout | agent-dashboard | `Cargo.toml` | `[workspace] resolver = "2" members = ["crates/agentflow-core", "crates/agentflow-schema", "crates/agentflow-store", "crates/agentflow-runtime", "apps/server"]`; shared deps via `[workspace.dependencies]` | direct-port |
| Workspace Layout (extended) | daily_rust | `Cargo.toml` | Workspace with `rust-autoscheduler` + `agentflow-harness-rust` sub-crate; demonstrates scheduler + workflow engine coexistence | adapt |
| Error Handling | agent-dashboard | `crates/agentflow-core/` | `DomainError` enum with variants `InvalidWorkflowTransition { from, to }` and `InvalidStepTransition { from, to }` | adapt |
| Exit Codes | daily_rust | `src/state/mod.rs` | `RunStatus { Pending, Running, Success, Failed, Timeout, Killed }` - structured exit classification | adapt |
| Notification Hub | daily_rust | `src/scheduler/mod.rs` + `src/health/mod.rs` | `NtfyNotifier` for profile change notifications; workflow engine `send_ntfy` tool for completion notifications | adapt |
| Structured Logging | agent-dashboard | `Cargo.toml` | `tracing = "0.1"`, `tracing-subscriber = { features = ["env-filter", "fmt"] }` | direct-port |
| Async Runtime | agent-dashboard + daily_rust | `Cargo.toml` | `tokio = { features = ["full"] }` (agent-dashboard); `tokio = { features = ["rt-multi-thread", "macros", "time", "process", "sync", "signal"] }` (daily_rust) | direct-port |
| Serialization | agent-dashboard | `Cargo.toml` | `serde = { features = ["derive"] }`, `serde_json = "1"`, `serde_yaml = "0.9"` | direct-port |
| Time Handling | daily_rust | `Cargo.toml` | `chrono = { features = ["serde"] }`, `chrono-tz` for timezone-aware scheduling | direct-port |
| UUID Generation | agent-dashboard | `Cargo.toml` | `uuid = { version = "1", features = ["v7", "serde"] }` - UUIDv7 for time-sortable identifiers | direct-port |
| SQLite Driver | agent-dashboard | `Cargo.toml` | `rusqlite = { version = "0.32", features = ["bundled", "chrono", "uuid"] }` - bundled (no system dependency) | direct-port |
| HTTP Framework | agent-dashboard | `Cargo.toml` | `axum = { version = "0.8", features = ["macros"] }` with `tower-http` (trace, cors) | direct-port |
| CLI Framework | daily_rust | `Cargo.toml` | `clap = { features = ["derive"] }` | direct-port |
| Process Management | daily_rust | `Cargo.toml` | `nix` (Unix process groups), Windows Job Objects via `windows` crate | direct-port |
| Cron Parsing | daily_rust | `Cargo.toml` | `cron = "0.12"` for schedule expression parsing | direct-port |

---

## 11. Runtime Health and Fallback

| PEARL Component | Reference Project | File/Module | Pattern/Evidence | Reuse Level |
|---|---|---|---|---|
| Health Monitor | daily_rust | `src/health/mod.rs` | `HealthMonitor` with `check_once()`: revalidate_config + recover_stale_locks + check_repeated_failures + check_log_size; runs on 60s loop | direct-port |
| Failure Decay | daily_rust | `src/health/mod.rs` | `failure_is_fresh(record, now, decay_hours)`: failures older than `failure_decay_hours` stop counting; prevents permanent degradation from stuck tasks | direct-port |
| Profile Escalation | daily_rust | `src/health/mod.rs` | Normal -> Degraded (3+ fresh failures) -> Recovery (5+ fresh failures); auto-restore to Normal when all failures stale | adapt |
| Degraded Fallback (workflow) | daily_rust/agentflow-harness-rust | `src/workflow_engine.rs` | `degraded_workflow_for()` mapping; auto-fallback on primary quality gate failure; single notification per fallback chain | adapt |
| Backend Health | daily-digest-prompt | `workflows/index.yaml` | `wf-20260404-backend-health-tracking`: tracks per-backend success/failure rate; auto-disables backends with >60% failure rate | reference-only |
| Concurrency Throttling | daily_rust | `src/scheduler/mod.rs` | `effective_parallel(profile)`: profile-specific cap that can be lower than global max; degraded profile limits to 1 task | adapt |
| Heavy Task Gating | daily_rust | `src/scheduler/mod.rs` | `profile_cfg.allow_heavy_tasks`: degraded profiles can skip heavy tasks; recovery profiles restrict to `allow_only` list | adapt |

---

## Summary Statistics

| Reuse Level | Count | Description |
|---|---|---|
| `direct-port` | 24 | Can be ported to Rust with minimal design changes |
| `adapt` | 30 | Pattern applies but needs significant adaptation for PEARL context |
| `reference-only` | 7 | Provides design inspiration but implementation differs substantially |
| `new-build` | 5 | No reference exists; must be designed and built from scratch |

### Critical New-Build Components (no reference pattern available)

1. **Precision Decision Engine** - Full P0/P1/P2/P3 step classification with precision scoring before execution
2. **Plan Compiler** - Unified validation of plans against policy, budget, capability, verifier presence
3. **Assurance Engine** - Separates execution completion from verified success with pluggable verifiers
4. **Durable Queue with Retry** - Task queue with retry semantics and dead-letter handling
5. **Repair Transaction** - Isolated workspace apply -> verify -> promote/rollback for self-healing

### Reference Project Coverage

| Reference Project | Rows Referenced | Primary Contribution |
|---|---|---|
| agent-dashboard | 16 | SQLite event store, typed domain events, workflow/step state machines, UUIDv7 |
| daily_rust | 18 | Scheduler engine, process supervisor, state store, health monitor |
| daily_rust/agentflow-harness-rust | 10 | Workflow engine, checkpoint/resume, quality gates, policy guard |
| daily_mistral | 7 | Planner-executor, DAG validation, event logger, loop engine, output validator |
| llamaindex_daily | 3 | Workflow orchestrator (DAG + parallel), RBAC policy engine |
| daily-digest-prompt (DDP) | 8 | YAML SoT, capability registry, skill/tool/workflow scale |
| small_daily_tasks | 5 | Guard engine (YAML rules, cloud API blocking, ReDoS protection) |
| AgentFlow-Notify | 3 | Specification-driven governance model |
| RAG_Skill | 1 | Monorepo packaging pattern |
