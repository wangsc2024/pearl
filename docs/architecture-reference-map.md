# PEARL v2 Architecture Reference Map

This document maps each PEARL v2 component (from Section 16 architecture diagram) to specific files, patterns, and implementations found in the reference projects: deepseek-harness, daily-digest-prompt (DDP), daily_agent, and AgentFlow-Notify.

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
| Constitution CI Gate | daily-digest-prompt | `hooks/pre_bash_guard.py` | `check_bash_command()` returns `(blocked, reason, guard_tag)` - machine enforcement pattern; `FALLBACK_BASH_RULES` as built-in rules with YAML override | adapt |
| ADR Process | AgentFlow-Notify | `speckit.constitution` Section 7 | "When requirements change after spec/plan/tasks are finalized, the affected artifact must be revised and re-approved in the same order" | adapt |

---

## 2. Control Plane

| PEARL Component | Reference Project | File/Module | Pattern/Evidence | Reuse Level |
|---|---|---|---|---|
| Scheduler | deepseek-harness | `packages/schedule/schedule/src/index.ts` | `ScheduleRuntime` with `start()`/`dispose()` lifecycle; `createAtScheduleRecord` (one-shot), `createEveryScheduleRecord` (interval), `createAfterScheduleRecord` (relative); event-log-driven scheduling | adapt |
| Scheduler (cron) | daily_agent | `crates/da-core/src/config.rs` | `ScheduleCfg` with cron expressions; `cron = "0.12"` workspace dependency; `timezone: "Asia/Taipei"` | direct-port |
| Planner | deepseek-harness | `packages/plan/plan-mode/` | Planning mode package (separate from execution) | reference-only |
| Router | daily-digest-prompt | `config/scoring.yaml` | `confidence_multipliers: {tier1: 1.0, tier2: 0.8, tier3: 0.6}` - three-tier routing confidence; `allowed_tools_table` maps task type to tool permissions | adapt |
| Router (mental model) | daily-digest-prompt | `config/scoring.yaml` | `mental_model_routing.models` with keyword-triggered prompt injection (`first_principles`, `inversion`, `bayesian`, `analogical`, `systems_thinking`, `metacognition`) | reference-only |
| Budget | daily-digest-prompt | `config/scoring.yaml` | `max_tasks_per_run: 3` - per-run task budget; quality_feedback enforcer with `multiplier_penalty: 0.7` | reference-only |
| Policy | daily-digest-prompt | `config/scoring.yaml` | `rules:` section - forced rules that override scoring (e.g., "tasks with KB/RAG must include Write in allowedTools") | adapt |
| SLO | daily-digest-prompt | `workflows/index.yaml` | Priority P0/P1 classification per workflow; success rate targets in `system-success-rate-improvement-workflow.yaml` entry | reference-only |

---

## 3. Precision Decision Engine (P0-P3)

| PEARL Component | Reference Project | File/Module | Pattern/Evidence | Reuse Level |
|---|---|---|---|---|
| P0 Scoring (Mechanical) | daily-digest-prompt | `config/scoring.yaml` | Full deterministic formula: `priority_scores` x `confidence_multipliers` x `description_bonus` x `time_proximity_bonus` x `label_count_bonus` x `recency_penalty` x `time_fatigue_factor` x `citizen_impact_bonus` | direct-port |
| P0 Tiebreaker | daily-digest-prompt | `config/scoring.yaml` | `tiebreaker.order: [due_time_asc, priority_desc, label_count_desc, task_id_asc]` - fully deterministic ordering | direct-port |
| P0 Config Validation | daily_agent | `crates/da-core/src/config.rs` | `load_yaml()` requires `version:` field; fails without it (mechanical validation) | direct-port |
| P1 Schema Verification | daily-digest-prompt | `workflows/index.yaml` | Entry `wf-20260330-error-object-json-schema`: JSON Schema Draft 2020-12 for error objects; `wf-20260319-results-validation`: results JSON schema | adapt |
| Classification Logic | (none) | -- | No reference implements step-level P0/P1/P2/P3 classification before execution | new-build |

---

## 4. Durable Work Plane

| PEARL Component | Reference Project | File/Module | Pattern/Evidence | Reuse Level |
|---|---|---|---|---|
| Task/Job Registry | deepseek-harness | `packages/jobs/jobs/src/index.ts` | `JobRegistry` abstract class: `start(spec: JobStart): JobId`, `list(caller?)`, `get(id, caller?)`, `read(id, caller?)`, `kill(id, caller?, reason?)`, `wait(id, timeoutMs, caller?, signal?)` | adapt |
| Job Settlement | deepseek-harness | `packages/jobs/jobs/src/index.ts` | "Settlement is first-wins: one terminal record, released waiters, and one round of contained listener notification" | adapt |
| Job Ownership | deepseek-harness | `packages/jobs/jobs/src/index.ts` | "Owned-job access is fenced by the owner's session id. Ids are predictable, so authorization -- not secrecy -- is the boundary" | adapt |
| Job Controller | deepseek-harness | `packages/jobs/jobs/src/index.ts` | `attachController(name)`: "start refuses work while no attached job controller serves the spec's owner" - admission control | adapt |
| Task Lock (Lease) | daily_agent | `crates/da-core/src/state.rs` | `try_acquire_lock(task_id, timeout)` with stale detection: `mtime > timeout * 2` -> reclaim; returns `LockGuard` (drop removes lock file) | direct-port |
| Resource Lock | daily_agent | `crates/da-core/src/state.rs` | `try_acquire_resource_lock(lock_key, group, timeout)` returns `ResourceLockGuard`; drop records `released_at` for cooldown | direct-port |
| Cooldown | daily_agent | `crates/da-core/src/state.rs` | `wait_resource_cooldown(group, cooldown)`: blocks if `elapsed < cooldown` since last release | direct-port |
| Atomic State Write | daily_agent | `crates/da-core/src/state.rs` | `save()`: unique tmp filename (pid + seq) -> `std::fs::write` -> `std::fs::rename`; concurrent writes tested with 8 threads x 20 iterations | direct-port |
| Read-Modify-Write | daily_agent | `crates/da-core/src/state.rs` | `update_locked()`: file lock (`create_new` + retry + stale recovery at 30s) wrapping load -> mutate -> save | direct-port |
| Queue/Retry | (none) | -- | No reference implements a full durable queue with retry semantics | new-build |
| Checkpoint | (none) | -- | No reference implements workflow step checkpointing with resume | new-build |

---

## 5. Planner-Executor

| PEARL Component | Reference Project | File/Module | Pattern/Evidence | Reuse Level |
|---|---|---|---|---|
| Workflow Engine | deepseek-harness | `packages/workflow/workflow/src/index.ts` | `WorkflowEngine.start(request: WorkflowStartRequest): WorkflowRun`; lifecycle event emission via `emitWorkflowEvent(name, ...args)` with listener containment (try/catch per listener) | adapt |
| Workflow Meta | deepseek-harness | `packages/workflow/workflow/src/types.ts` | `WorkflowMeta { name, description, whenToUse?, phases? }`; `WorkflowPhase { title, detail?, provider?, model? }` | adapt |
| Workflow Result | deepseek-harness | `packages/workflow/workflow/src/types.ts` | `WorkflowResult { value, stopReason: 'completed'|'cancelled'|'error', error?, agentsStarted }` | adapt |
| Error Codes | deepseek-harness | `packages/workflow/workflow/src/index.ts` | `WorkflowErrorCode`: `SCRIPT_PARSE`, `META_INVALID`, `INVALID_ARGUMENT`, `AGENT_CAP`, `ITEM_CAP`, `CANCELLED`, etc.; `WorkflowError.fatal` flag drives propagation vs null-mapping | adapt |
| Plan Compiler | (none) | -- | No reference validates plans against policy/budget/capability/verifier presence | new-build |
| Assurance Engine | (none) | -- | No reference separates execution completion from verified success | new-build |

---

## 6. Execution Runtime

| PEARL Component | Reference Project | File/Module | Pattern/Evidence | Reuse Level |
|---|---|---|---|---|
| Script Runtime (Python) | daily-digest-prompt | `hooks/pre_bash_guard.py` | Python CLI pattern: `read_stdin_json()` -> process -> `output_decision()` JSON output; `main()` entry point | reference-only |
| Script Runtime (Config) | daily_agent | `crates/da-core/src/config.rs` | `EngineKind` enum for execution engine selection; `TaskCfg { engine, workflow, goal }` | adapt |
| Timeout Policy | deepseek-harness | `packages/guard/timeout-policy/src/index.ts` | Cooperative timeout: `using d = deadline(exec.signal, timeoutMs, TOOL_TIMEOUT)`; swap signal onto exec, restore after; produces structured `TOOL_TIMEOUT` error with `toolTimeoutResult(timeoutMs)` | adapt |
| Process Supervisor | daily_agent | `crates/da-core/src/config.rs` | `max_parallel_tasks`, `default_timeout_seconds`, `stop_check_interval_seconds` config; `resource_group_cooldowns` | reference-only |
| Process Supervisor (tree kill) | (none) | -- | No reference implements full process tree cleanup (Job Objects/process groups) | new-build |
| Runtime Adapters | daily_agent | `Cargo.toml` | `tokio = { features = ["process", "signal"] }` for async process management | reference-only |

---

## 7. Capability Fabric

| PEARL Component | Reference Project | File/Module | Pattern/Evidence | Reuse Level |
|---|---|---|---|---|
| Capability Registry | daily-digest-prompt | `workflows/index.yaml` | Typed index (version 1.2.4): entries with `id`, `path`, `type` (workflow_yaml/validation_checklist/output_schema/tool), `task_types`, `priority` (P0/P1), `version`, `created_at`, `summary`, `read_when`, `alignment` | adapt |
| Capability Types | daily-digest-prompt | `workflows/index.yaml` | Evidence of types: `workflow_yaml`, `validation_checklist`, `output_schema`, `tool` - maps to PEARL's script/tool/verifier/skill/agent/workflow/runtime/guard | adapt |
| Capability Routing | daily-digest-prompt | `workflows/index.yaml` | `task_types` field routes capabilities: `["all"]` or specific types like `["system_insight", "self_heal"]` | adapt |
| Skill Structure | daily-digest-prompt | `skills/` directory | 264 skills (e.g., `academic-paper-research`, `arch-evolution`, `auto-task-creator`, `a-tier-task-optimizer`, `backend-manager`) each with `SKILL.md` manifest | reference-only |
| Tool Contract | daily-digest-prompt | `hooks/pre_bash_guard.py` | Python tool pattern: stdin JSON -> processing -> stdout JSON (`output_decision()`); structured imports (`hook_utils`, `tool_passport`) | adapt |
| Verifier Pattern | daily-digest-prompt | `workflows/index.yaml` | `wf-20260411-done-cert-validation`: DONE_CERT quality certification; `wf-20260329-error-object-schema`: error validation checklist | reference-only |

---

## 8. Evidence / State / Memory Layer

| PEARL Component | Reference Project | File/Module | Pattern/Evidence | Reuse Level |
|---|---|---|---|---|
| Event Ledger (append-only) | daily_agent | `crates/da-core/src/state.rs` | `append_jsonl()`: append-only JSONL; `append_jsonl_chained()`: hash chain with `chain = fnv64(previous_line)` for tamper detection | adapt |
| Event Chain Verification | daily_agent | `crates/da-core/src/state.rs` | `verify_chain(file)`: checks each line's `chain` field equals `fnv64(prev_line)`; returns break positions; tolerates legacy lines without chain | direct-port |
| Hash Functions | daily_agent | `crates/da-core/src/lib.rs` | `fnv8(data) -> String`: FNV-1a 8-hex (context manifest/args digest); `fnv64(data) -> String`: FNV-1a 64-bit hex (notification dedup fingerprint) | direct-port |
| State Store | daily_agent | `crates/da-core/src/state.rs` | `StateStore { dir }` with `load<T: DeserializeOwned + Default>()`, `save<T: Serialize>()`, `update_locked()`, `try_acquire_lock()` | direct-port |
| Secret Redaction | daily_agent | `crates/da-core/src/redact.rs` | `redact()` function called before `append_jsonl` write: "secret masking: scan and replace known secret values before logging" | adapt |
| SQLite (materialized state) | (none) | -- | No reference uses SQLite for materialized event state | new-build |
| Memory Store | daily_agent | `crates/da-core/src/lib.rs` | `pub mod memory; pub use memory::MemoryStore;` - separate memory module from state | reference-only |
| Artifact Management | (none) | -- | No reference has formal artifact lifecycle | new-build |
| Cache Layer | daily-digest-prompt | `workflows/index.yaml` | `wf-20260331-cache-ttl-validation`: TTL adjustment + hit rate verification; references `config/cache-policy.yaml` | reference-only |

---

## 9. Governance Layer

| PEARL Component | Reference Project | File/Module | Pattern/Evidence | Reuse Level |
|---|---|---|---|---|
| Guard Engine (Pre) | daily-digest-prompt | `hooks/pre_bash_guard.py` | Pre-execution guard: regex pattern matching, fail-closed blocking, structured audit via `log_blocked_event()`, YAML rule source with fallback | adapt |
| Guard Rule Structure | daily-digest-prompt | `hooks/pre_bash_guard.py` | Rule fields: `id`, `pattern`/`patterns`, `flags`, `reason`, `guard_tag`, `check` (custom), `contains` (pre-filter), `warn_only` | adapt |
| Guard Tags | daily-digest-prompt | `hooks/pre_bash_guard.py` | `safety-guard`, `state-guard`, `git-guard`, `env-guard`, `exfiltration-guard`, `nul-guard`, `path-leak-guard`, `confirm-guard`, `passport-guard`, `redos-input-guard` | adapt |
| Confirm Patterns | daily-digest-prompt | `hooks/pre_bash_guard.py` | `check_confirm_patterns()`: separate from block rules; returns `[confirm_required]` for operations needing operator confirmation (webhooks, batch state) | adapt |
| ReDoS Defense | daily-digest-prompt | `hooks/pre_bash_guard.py` | `MAX_COMMAND_CHARS = 16_384`: rejects commands exceeding limit before regex evaluation | direct-port |
| Circuit Breaker | daily_agent | `crates/da-core/src/breaker.rs` | `BreakerBank` with `BreakerVerdict { Closed, HalfOpen, Open }`; persistent in `circuit-breakers.json`; `BreakerCfg { failure_threshold, cooldown_s, min_samples, error_rate }` | direct-port |
| Circuit Breaker (rate mode) | daily_agent | `crates/da-core/src/breaker.rs` | B8 rate mode: `min_samples > 0` triggers rate-based evaluation; `error_rate` threshold; counter aging at `AGE_CAP = 200` (halve both counters) | direct-port |
| OODA Loop | daily-digest-prompt | `workflows/index.yaml` | `wf-20260406-ooda-decide-quality-gates`: quality gates for OODA Decide step; references `config/ooda-workflow.yaml` | reference-only |
| Quality Feedback | daily-digest-prompt | `config/scoring.yaml` | `quality_feedback.enforcer`: consecutive low scores trigger confidence penalty (0.7x) or task type pause; source `state/quality-trend.json` | reference-only |
| Audit Trail | daily_agent | `crates/da-core/src/state.rs` | `append_jsonl_chained()` with tamper-evident hash chain; `verify_chain()` for integrity verification | direct-port |
| Repair Transaction | (none) | -- | No reference implements isolated workspace -> apply -> verify -> promote/rollback | new-build |
| Specification Governance | AgentFlow-Notify | `speckit.constitution` | Sections 3-7: spec-first governance, required coverage, planning requirements, task requirements, change control | adapt |

---

## 10. Cross-Cutting Infrastructure

| PEARL Component | Reference Project | File/Module | Pattern/Evidence | Reuse Level |
|---|---|---|---|---|
| Workspace Layout | daily_agent | `Cargo.toml` | `[workspace] resolver = "2" members = ["crates/da-core", "crates/da-llm", ...]`; shared deps via `[workspace.dependencies]` | direct-port |
| Error Handling | daily_agent | `crates/da-core/src/error.rs` | Dedicated error module in core crate | adapt |
| Degradation Journal | daily_agent | `crates/da-core/src/lib.rs` | `pub mod degradation; pub use degradation::{DegradationEntry, DegradationJournal}` | adapt |
| Environment Control | daily_agent | `crates/da-core/src/lib.rs` | `pub mod envctl;` - runtime environment switches | adapt |
| Exit Codes | daily_agent | `crates/da-core/src/exit.rs` | Structured exit code module | adapt |
| LLM Lease | daily_agent | `crates/da-core/src/lib.rs` | `pub mod llm_lease;` - LLM resource leasing | reference-only |
| Notification Hub | daily_agent | `crates/da-core/src/notify.rs` | `Notification { title, body, priority, tags, task_result, result_summary }`; hub primary + ntfy fallback; "LLM only drafts, mechanical layer sends" | adapt |
| Result Summary | daily_agent | `crates/da-core/src/notify.rs` | `ResultSummary { kind: "list"|"stats"|"evidence"|"digest", title?, items: Vec<ResultItem>, total? }` | adapt |
| Structured Logging | daily_agent | `Cargo.toml` | `tracing = "0.1"`, `tracing-subscriber = { features = ["env-filter"] }` | direct-port |
| Async Runtime | daily_agent | `Cargo.toml` | `tokio = { features = ["rt-multi-thread", "macros", "time", "process", "sync", "signal"] }` | direct-port |
| Serialization | daily_agent | `Cargo.toml` | `serde`, `serde_json`, `serde_yaml` (version 0.9) | direct-port |
| Time Handling | daily_agent | `Cargo.toml` | `chrono = { features = ["serde"] }`, `chrono-tz = "0.9"` | direct-port |
| CLI Framework | daily_agent | `Cargo.toml` | `clap = { features = ["derive"] }` | direct-port |
| UUID Generation | daily_agent | `Cargo.toml` | `uuid = { features = ["v4"] }` | direct-port |

---

## 11. Runtime Health and Fallback

| PEARL Component | Reference Project | File/Module | Pattern/Evidence | Reuse Level |
|---|---|---|---|---|
| Backend Health | daily-digest-prompt | `workflows/index.yaml` | `wf-20260404-backend-health-tracking`: tracks per-backend success/failure rate; auto-disables backends with >60% failure rate; references `tools/backend_preflight.py` | reference-only |
| Fallback Chain | daily_agent | `crates/da-core/src/notify.rs` | Hub -> ntfy fallback pattern; mechanical decision based on delivery success | adapt |
| Runtime Profile | daily-digest-prompt | `workflows/index.yaml` | Multiple workflows reference `state/quality-trend.json` for runtime health decisions | reference-only |
| Cooperative Routing | daily-digest-prompt | `workflows/index.yaml` | `wf-20260628-fugu-gate-cooperative-routing`: Thinker-Worker-Verifier pattern with kill switch (`DDP_FUGU_MODE=off`) | reference-only |

---

## Summary Statistics

| Reuse Level | Count | Description |
|---|---|---|
| `direct-port` | 16 | Can be ported to Rust with minimal design changes |
| `adapt` | 26 | Pattern applies but needs significant adaptation for PEARL context |
| `reference-only` | 14 | Provides design inspiration but implementation differs substantially |
| `new-build` | 9 | No reference exists; must be designed and built from scratch |

### Critical New-Build Components (no reference pattern available)

1. **SQLite Event Ledger** - Append-only event store with materialized views
2. **Precision Decision Engine** - P0/P1/P2/P3 step classification before execution
3. **Plan Compiler** - Validates plans against policy, budget, capability, verifier
4. **Assurance Engine** - Separates execution completion from verified success
5. **Durable Queue with Retry** - Task queue with retry semantics and dead-letter
6. **Workflow Checkpoint/Resume** - Step-level checkpointing with crash recovery
7. **Process Supervisor (tree kill)** - Full process tree management (Job Objects/process groups)
8. **Repair Transaction** - Isolated apply -> verify -> promote/rollback
9. **Artifact Lifecycle** - Formal artifact management and storage
