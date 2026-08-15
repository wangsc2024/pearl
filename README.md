# PEARL v2

**Constitutional Deterministic Agent Framework**

> 能算的不要猜，能驗的不要信，不能驗的不要假裝確定。
>
> What can be computed must not be guessed.
> What can be verified must not be trusted.
> What cannot be verified must not be claimed as certain.

PEARL is a **deterministic-first autonomous execution framework**. It inverts the usual
agent-first design: mechanical scripts are the primary execution unit, LLMs are admitted
only where determinism is impossible, and no work is reported as successful unless a
machine verified it.

## Documents

| Document | Purpose |
|---|---|
| [`系統開發需求書.md`](系統開發需求書.md) | System requirements specification (73 sections) |
| [`CONSTITUTION.md`](CONSTITUTION.md) | The twelve binding articles, each with its machine check |
| [`docs/system-analysis.md`](docs/system-analysis.md) | Analysis of reference projects and the phased roadmap |
| [`docs/architecture-reference-map.md`](docs/architecture-reference-map.md) | Component-by-component mapping to reference implementations |
| [`docs/adr/`](docs/adr/) | Architecture decision records |
| [`schemas/`](schemas/) | JSON Schemas for tasks, capabilities, evidence and events |

## Implementation status

Phase 0 and Phase 1 of the roadmap in `docs/system-analysis.md` §D are implemented.
**The kernel contains no LLM coupling** — that is Phase 2 onward.

### Delivered

| Crate | Responsibility |
|---|---|
| `pearl-core` | Identifiers (UUIDv7), injectable clock, Config Resolution with `config_hash`, precision classes, Exactness Gate, evidence model, idempotency keys, task state machine |
| `pearl-events` | Append-only event ledger. `UPDATE`/`DELETE` are refused by SQLite triggers, not merely absent from the API |
| `pearl-state` | Materialized projections. Every mutation appends its event and updates the projection in one transaction; full rebuild-from-ledger |
| `pearl-lease` | Worker leases: claim, heartbeat, expiry, reclamation |
| `pearl-queue` | Durable queue as a view over `READY`; deterministic backoff, dead-lettering |
| `pearl-process-supervisor` | Spawn, timeout, cancel, whole-tree kill via process groups |
| `pearl-governance` | The Constitution CI gate |
| `pearl-cli` | Operator surface (`pearl`) |

### Not yet built

`pearl-planner`, `pearl-plan-compiler`, `pearl-executor`, `pearl-assurance`,
`pearl-precision`, `pearl-policy`, `pearl-guard`, `pearl-router`, `pearl-workflow`,
`pearl-runtime`, `pearl-capabilities`, `pearl-evidence`, `pearl-scheduler`.

Three Constitution articles (3, 11, 12) are enforced by review rather than by machine
because the crate that owns their check is not yet written. See the status column in
`CONSTITUTION.md`.

## Build

```bash
cargo build --workspace
cargo test --workspace          # 232 tests
cargo clippy --workspace --all-targets -- -D warnings
```

## Usage

```bash
# Submit a task
pearl --db pearl.db task submit task.yaml

# Inspect it, with its runs and config provenance
pearl --db pearl.db task inspect daily.digest

# Read the event history
pearl --db pearl.db event log daily.digest

# Rebuild all state from the ledger alone
pearl --db pearl.db event replay

# Queue and lease operations
pearl --db pearl.db queue status
pearl --db pearl.db lease reap

# Fail the build on any Constitution violation
pearl constitution check capabilities/

# Kernel health
pearl --db pearl.db doctor
```

Add `--json` to any command for machine-readable output on stdout. Diagnostics always go
to stderr, so the two never mix (§26).

### Exit codes

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | operational error |
| 2 | **Constitution violation** |

Exit 2 is distinct so CI can tell "this change violates an article" from "the disk is
full".

## What the tests actually prove

These are not smoke tests; each one fails if the corresponding article stops holding.

| Article | Proven by |
|---|---|
| 1 — determinism first | `check_no_llm_for_deterministic` rejects a manifest declaring `deterministic: true` with `kind: agent` |
| 2 — exactness needs a verifier | A task demanding exactness with no verification cannot reach `VERIFIED_SUCCESS`; it goes to `UNVERIFIED`, which is resolvable rather than terminal |
| 4 — provable success | `VERIFIED_SUCCESS` is refused with no evidence, empty evidence, or evidence containing a failure |
| 5 — idempotency | Requesting the same effect key twice performs it once and records `effect.deduplicated` |
| 6 — persistent state | Projections are wiped and fully restored from the ledger; three consecutive replays agree |
| 8 — no self-certification | Human approval alone cannot certify success; at least one machine-produced item is required |
| 9 — cancellability | A three-generation process tree is spawned and every pid is confirmed dead via `/proc` after cancellation |
| 9 — timeout enforcement | A process that traps `SIGTERM` is still killed |
| 10 — single source of truth | A run without `config_revision` and `config_hash` is rejected at insert |

## Design notes

Two bugs were found by these tests during implementation, both recorded as ADRs:

- **[ADR-0002](docs/adr/ADR-0002-events-must-be-self-sufficient.md)** — the replay test
  caught `task.created` recording only one of three quality flags, making the ledger lossy.
  A rebuilt task would have been evaluated under a different Exactness Gate verdict than
  the same task before the rebuild.
- The lease reaper originally returned crashed tasks straight to `READY`, but
  `RUNNING → READY` is not a legal transition, so crashes mid-run were silently skipped
  instead of reclaimed. The destination now depends on how far the task got: `LEASED →
  READY` (nothing ran), `RUNNING → RETRY_WAIT` (work ran, so retry accounting applies).

## License

MIT
