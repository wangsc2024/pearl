-- The event ledger: the source of truth (ADR-0001).
--
-- "Append-only" is enforced here as well as in the API, because a migration script, a
-- debugging session, or a future crate with its own Connection would bypass the API but
-- not a trigger.

CREATE TABLE IF NOT EXISTS events (
    id              TEXT PRIMARY KEY,
    schema_version  INTEGER NOT NULL,
    occurred_at     TEXT    NOT NULL,
    trace_id        TEXT    NOT NULL,
    task_id         TEXT,
    run_id          TEXT,
    attempt_id      TEXT,
    worker_id       TEXT,
    event_type      TEXT    NOT NULL,
    payload         TEXT    NOT NULL
);

-- (trace_id, id) serves the common "replay this task's history in order" query.
CREATE INDEX IF NOT EXISTS idx_events_trace ON events (trace_id, id);
CREATE INDEX IF NOT EXISTS idx_events_task  ON events (task_id, id);
CREATE INDEX IF NOT EXISTS idx_events_type  ON events (event_type, id);

-- Article 6 / ADR-0001: history is immutable. A correction is a new event.
CREATE TRIGGER IF NOT EXISTS events_forbid_update
BEFORE UPDATE ON events
BEGIN
    SELECT RAISE(ABORT, 'event ledger is append-only: UPDATE is forbidden');
END;

CREATE TRIGGER IF NOT EXISTS events_forbid_delete
BEFORE DELETE ON events
BEGIN
    SELECT RAISE(ABORT, 'event ledger is append-only: DELETE is forbidden');
END;
