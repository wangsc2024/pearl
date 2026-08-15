# Where each automation sits on the ladder

The rule this port follows, from 系統開發需求書:

```
Mechanical  →  Workflow  →  LLM-assisted  →  Autonomous Agent
```

Read it as a ladder you climb only when forced, one rung at a time:

| the work is | so it is | and success is established by |
| --- | --- | --- |
| deterministic | mechanical execution | the code, which is the specification |
| precisely verifiable | an LLM may generate it | **mechanical verification** |
| not fully determinable | LLM reasoning | evidence + policy + assurance |
| high risk and not mechanically confirmable | — | **nothing may declare success automatically** |

The last row is the one that has teeth. Everywhere else the question is "how do we check
this?"; there the answer is "we cannot", and the honest response is to refuse to auto-complete
rather than to lower the standard until something passes.

## How each rung is expressed in PEARL

Not as a comment. Each rung is a set of declarations that the gates already enforce:

**Rung 1 — mechanical.** `type: script`, a mechanical `runtime`, `quality.deterministic: true`.
Article 1's check rejects this combination against an agent runtime, so the claim cannot be
made falsely. On the task: `precision_class: p0` with `deterministic_generation: true`.

**Rung 2 — workflow.** Still no model. The escalation is in *composition*, not in capability:
several rung-1 capabilities wired by `input_from`, with a `verify` step and `effect` steps
declared separately from the work. `max_llm_calls: 0` in the budget is the mechanical assertion
that nothing here reasons — a `plan` or agent step in such a workflow will not compile.

**Rung 3 — LLM-assisted.** An `agent` capability with `deterministic: false`, and — the part
that makes this rung legal rather than merely convenient — a mechanical verifier downstream
that can decide whether the output is right. Article 8: the thing that generates is never the
thing that judges. If no mechanical check exists, this is not rung 3.

**Rung 4 — reasoning under evidence.** `exactness_required: true` with
`deterministic_verification: true` where a verifier exists, and assurance steps with
`evidence_required: true`. The verdict rests on recorded evidence rather than on the agent's
own report.

**Rung 5 — refuse to self-certify.** `exactness_required: true` with
`deterministic_verification: false` and no verifier that could settle it. The Exactness Gate
then *blocks* auto-completion: the task lands in `UNVERIFIED`, which is a terminal state
requiring a human, not a failure. `pearl doctor` counts these and says so. For work that must
stop and ask before acting, `execution.kind: human_gate`.

`UNVERIFIED` is worth being clear about, because it looks like a defect and is not: it is the
system declining to claim something it cannot show. A run of these tasks that ends with
`UNVERIFIED` counts is working correctly.

## The classification

### Rung 1 — mechanical

| automation | capability | status |
| --- | --- | --- |
| `knowledge_hygiene` | `script.knowledge-health` | implemented, verified end to end |
| `website_curator_optimize` | `script.website-curator-optimize` | declared |
| `output_watchdog` | `script.output-reconcile` | declared |
| `ntfy_review` | `script.notification-review` | declared |

The two reconciliation tasks belong here and did not in DDP, where they scraped log files. A
query over `effects` and the ledger is arithmetic; parsing logs to guess whether something
happened is not.

### Rung 2 — workflow, still mechanical

| automation | workflow | status |
| --- | --- | --- |
| `pingtung_event_digest` | `ddp.pingtung-event-digest` | compiles; collector declared |
| `daily_blog_digest` | (to write) | collector declared |

Both are fetch → verify → push. The push being a separate `effect.notify` step is what makes
the digest retryable and what makes "was it sent?" a query rather than an inference.

### Rung 3 — LLM generates, machine verifies

| automation | shape | status |
| --- | --- | --- |
| `zen_koan` | select → **compose** → assemble → verify → push | implemented except the model call |
| `ai_news_daily`, `tech_research`, `life_news_daily`, `ai_deep_research_continue` | collect → synthesise → verify | not started |

`zen_koan` is the worked example. The model writes prose, which is not a formula — but the
prompt asks for three sections of stated lengths, so *whether it complied* is mechanical. The
assembler counts each section and reports `complete`; the workflow asserts `complete: true`.
That is what keeps a one-line response from reaching anyone, which is the failure DDP actually
hit.

Note what the floors are: the lower bounds the prompt asks for. Choosing them independently of
the prompt is how the first draft of this port came to reject a real koan.

### Rung 4 — reasoning, judged on evidence

The Cursor-agent automations that analyse and propose: `log_audit`, `skill_audit`,
`mcp_servers_audit`, `kb_system_optimize`, `thought_distillation`, `backlog_research`,
`research_video_digest`, `research_video_brief`, `youtube_research_daily`, `jingtu`,
`phantom_butterfly_optimize`, `future_plan_optimize`, `future_plan_bridge`,
`website_curator`, `new_asset_onboard`, `pending_task_executor`.

None started. Each needs a prompt and an assurance step whose `require_keys` state what the
analysis must contain — the verifier cannot judge whether an insight is good, but it can insist
the claims cite what they rest on, which is what Article 4 asks of this rung.

### Rung 5 — must not self-certify

| automation | why |
| --- | --- |
| `kb_publish` | writes to a knowledge base and a working tree. Whether a published document is *correct* is not mechanically decidable, and the write is not trivially reversible. |
| `ithome_ai_presentation` | produces something published under a person's name. |
| `self_heal` | changes the system that is judging the change. CONSTITUTION Article 12 already forbids landing an architecture-level change without the full Finding → ADR → Verification chain. |
| `tools_forge`, `task_forge`, `skill_forge` | create new capabilities. A capability that certified its own creation would make the registry self-extending, which is exactly what §31 forbids the executor from doing. |

These get `deterministic_verification: false` and no verifier that pretends to settle them, so
the Exactness Gate stops them at `UNVERIFIED` and a human decides. `kb_publish` additionally
splits: the preflight checks are rung 1 and can pass on their own; the publish is the part that
stops. That split is not a workaround — it is the useful half being allowed to run while the
part nobody can mechanically vouch for waits.

## Two things this ordering rules out

**Skipping a rung.** An automation does not get an agent because an agent is easier to write
than the arithmetic. `knowledge_hygiene` is file ages and a regex; asking a model would be an
Article 1 violation, and the manifest could not honestly declare it.

**Climbing to escape verification.** Moving work up a rung does not lower what is required of
it — it raises it. Rung 3 needs a mechanical verifier that rung 1 did not, because rung 1's
correctness is in code that can be read. The rung above never asks for less evidence than the
one below.
