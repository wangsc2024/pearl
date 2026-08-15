-- The projections, as first shipped.
--
-- ADR-0001: the ledger is truth, these tables are a derived cache. Deliberately free of the
-- append-only triggers that guard `events`: these tables are *meant* to be mutable and
-- droppable, because they can be rebuilt by replaying the ledger.
--
-- This file is the baseline, so it is the one migration that must stay safe to run against a
-- database that already has these tables: every statement is `IF NOT EXISTS`. Databases
-- created before the migration runner existed pass through it untouched and then pick up the
-- later files.

CREATE TABLE IF NOT EXISTS tasks (
    task_id         TEXT PRIMARY KEY,
    trace_id        TEXT    NOT NULL,
    task_type       TEXT    NOT NULL,
    state           TEXT    NOT NULL,
    precision_class TEXT,
    exactness_required          INTEGER NOT NULL,
    deterministic_generation    INTEGER NOT NULL,
    deterministic_verification  INTEGER NOT NULL,
    created_at      TEXT    NOT NULL,
    updated_at      TEXT    NOT NULL,
    attempt_count   INTEGER NOT NULL DEFAULT 0,
    last_reason     TEXT
);
CREATE INDEX IF NOT EXISTS idx_tasks_state ON tasks (state);

CREATE TABLE IF NOT EXISTS runs (
    run_id          TEXT PRIMARY KEY,
    task_id         TEXT    NOT NULL,
    trace_id        TEXT    NOT NULL,
    started_at      TEXT    NOT NULL,
    ended_at        TEXT,
    config_revision TEXT    NOT NULL,
    config_hash     TEXT    NOT NULL,
    outcome         TEXT
);
CREATE INDEX IF NOT EXISTS idx_runs_task ON runs (task_id);

CREATE TABLE IF NOT EXISTS attempts (
    attempt_id      TEXT PRIMARY KEY,
    run_id          TEXT    NOT NULL,
    attempt_number  INTEGER NOT NULL,
    started_at      TEXT    NOT NULL,
    ended_at        TEXT,
    outcome         TEXT,
    exit_reason     TEXT
);
CREATE INDEX IF NOT EXISTS idx_attempts_run ON attempts (run_id);

CREATE TABLE IF NOT EXISTS leases (
    lease_id        TEXT PRIMARY KEY,
    task_id         TEXT    NOT NULL,
    worker_id       TEXT    NOT NULL,
    acquired_at     TEXT    NOT NULL,
    leased_until    TEXT    NOT NULL,
    last_heartbeat  TEXT    NOT NULL,
    released_at     TEXT
);
CREATE INDEX IF NOT EXISTS idx_leases_task ON leases (task_id);
-- Partial index: the reaper only ever scans unreleased leases.
CREATE INDEX IF NOT EXISTS idx_leases_open ON leases (leased_until) WHERE released_at IS NULL;

CREATE TABLE IF NOT EXISTS effects (
    idempotency_key TEXT PRIMARY KEY,
    effect          TEXT    NOT NULL,
    requested_at    TEXT    NOT NULL,
    committed_at    TEXT
);

CREATE TABLE IF NOT EXISTS evidence (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id       TEXT    NOT NULL,
    evidence_type TEXT    NOT NULL,
    producer      TEXT    NOT NULL,
    passed        INTEGER NOT NULL,
    recorded_at   TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_evidence_task ON evidence (task_id);

CREATE TABLE IF NOT EXISTS steps (
    step_id         TEXT PRIMARY KEY,
    run_id          TEXT    NOT NULL,
    step_number     INTEGER NOT NULL,
    description     TEXT    NOT NULL,
    status          TEXT    NOT NULL DEFAULT 'pending',
    started_at      TEXT,
    completed_at    TEXT
);
CREATE INDEX IF NOT EXISTS idx_steps_run ON steps (run_id);

CREATE TABLE IF NOT EXISTS checkpoints (
    checkpoint_id   TEXT PRIMARY KEY,
    task_id         TEXT    NOT NULL,
    label           TEXT    NOT NULL,
    payload         TEXT,
    created_at      TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_checkpoints_task ON checkpoints (task_id);

CREATE TABLE IF NOT EXISTS schedules (
    schedule_id     TEXT PRIMARY KEY,
    task_type       TEXT    NOT NULL,
    cron_expr       TEXT,
    interval_secs   INTEGER,
    next_run_at     TEXT,
    enabled         INTEGER NOT NULL DEFAULT 1,
    created_at      TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS capabilities (
    capability_id   TEXT PRIMARY KEY,
    version         INTEGER NOT NULL,
    capability_type TEXT    NOT NULL,
    runtime         TEXT    NOT NULL,
    deterministic   INTEGER NOT NULL,
    registered_at   TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS artifacts (
    artifact_id     TEXT PRIMARY KEY,
    task_id         TEXT    NOT NULL,
    artifact_type   TEXT    NOT NULL,
    path            TEXT    NOT NULL,
    sha256          TEXT    NOT NULL,
    size_bytes      INTEGER NOT NULL,
    created_at      TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_artifacts_task ON artifacts (task_id);

CREATE TABLE IF NOT EXISTS verification_results (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id         TEXT    NOT NULL,
    verifier_id     TEXT    NOT NULL,
    passed          INTEGER NOT NULL,
    detail          TEXT,
    verified_at     TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_verification_task ON verification_results (task_id);

CREATE TABLE IF NOT EXISTS policy_decisions (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id         TEXT,
    decision_type   TEXT    NOT NULL,
    outcome         TEXT    NOT NULL,
    reason          TEXT,
    decided_at      TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS config_revisions (
    revision_id     TEXT PRIMARY KEY,
    config_hash     TEXT    NOT NULL,
    source          TEXT    NOT NULL,
    applied_at      TEXT    NOT NULL,
    payload         TEXT
);

CREATE TABLE IF NOT EXISTS runtime_health (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    subsystem       TEXT    NOT NULL,
    status          TEXT    NOT NULL,
    detail          TEXT,
    recorded_at     TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_runtime_health_subsystem ON runtime_health (subsystem);
