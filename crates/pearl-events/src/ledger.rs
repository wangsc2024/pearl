//! The append-only event ledger.
//!
//! ADR-0001 makes this table the source of truth. "Append-only" is enforced in two
//! independent places, deliberately:
//!
//! 1. **API shape** — this module exposes no update or delete operation.
//! 2. **Database triggers** — `UPDATE` and `DELETE` on `events` raise `ABORT`.
//!
//! The second layer exists because the first only protects callers who go through this
//! crate. A migration script, a debugging session, or a future crate with its own
//! `Connection` would bypass the API but not the trigger.

use crate::event::{EventEnvelope, PearlEvent};
use chrono::{DateTime, Utc};
use pearl_core::{EventId, TaskId, TraceId, WorkerId};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::path::Path;

/// DDL for the ledger. Applied idempotently on open.
const SCHEMA: &str = r#"
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
"#;

/// The event ledger.
pub struct EventLedger {
    conn: Connection,
}

impl EventLedger {
    /// Opens (or creates) a ledger on disk.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LedgerError> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// Opens an in-memory ledger, for tests.
    pub fn open_in_memory() -> Result<Self, LedgerError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, LedgerError> {
        // WAL: concurrent readers alongside a single writer, which is exactly the
        // worker-plus-inspector access pattern. NORMAL synchronous is safe under WAL
        // for process crash (the case Article 6 cares about); only OS/power loss can
        // lose the tail, and that is an accepted trade for write throughput.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Borrows the underlying connection.
    ///
    /// Exposed so that `pearl-state` can append an event and update its materialized
    /// tables inside one transaction — ADR-0001 requires those two writes to be atomic,
    /// which is impossible across two connections.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Appends one event.
    pub fn append(&self, envelope: &EventEnvelope) -> Result<(), LedgerError> {
        append_with(&self.conn, envelope)
    }

    /// Appends several events atomically.
    ///
    /// Either the whole batch lands or none of it does; a partially written lifecycle
    /// would be indistinguishable from a crash mid-sequence during replay.
    pub fn append_batch(&mut self, envelopes: &[EventEnvelope]) -> Result<(), LedgerError> {
        let tx = self.conn.transaction()?;
        for envelope in envelopes {
            append_in_tx(&tx, envelope)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every event for one trace, in occurrence order.
    pub fn read_trace(&self, trace_id: TraceId) -> Result<Vec<EventEnvelope>, LedgerError> {
        self.query(
            "SELECT payload, id, schema_version, occurred_at, trace_id, attempt_id, worker_id
             FROM events WHERE trace_id = ?1 ORDER BY id ASC",
            params![trace_id.to_string()],
        )
    }

    /// Every event for one task, in occurrence order.
    pub fn read_task(&self, task_id: &TaskId) -> Result<Vec<EventEnvelope>, LedgerError> {
        self.query(
            "SELECT payload, id, schema_version, occurred_at, trace_id, attempt_id, worker_id
             FROM events WHERE task_id = ?1 ORDER BY id ASC",
            params![task_id.as_str()],
        )
    }

    /// The whole ledger in order. This is the replay source.
    pub fn read_all(&self) -> Result<Vec<EventEnvelope>, LedgerError> {
        self.query(
            "SELECT payload, id, schema_version, occurred_at, trace_id, attempt_id, worker_id
             FROM events ORDER BY id ASC",
            params![],
        )
    }

    /// Events of one type, in order.
    pub fn read_by_type(&self, event_type: &str) -> Result<Vec<EventEnvelope>, LedgerError> {
        self.query(
            "SELECT payload, id, schema_version, occurred_at, trace_id, attempt_id, worker_id
             FROM events WHERE event_type = ?1 ORDER BY id ASC",
            params![event_type],
        )
    }

    pub fn count(&self) -> Result<u64, LedgerError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// The most recent event id, used as a resume cursor.
    pub fn latest_event_id(&self) -> Result<Option<EventId>, LedgerError> {
        let raw: Option<String> = self
            .conn
            .query_row("SELECT id FROM events ORDER BY id DESC LIMIT 1", [], |r| {
                r.get(0)
            })
            .optional()?;
        match raw {
            Some(s) => Ok(Some(EventId::parse(&s)?)),
            None => Ok(None),
        }
    }

    fn query<P: rusqlite::Params>(
        &self,
        sql: &str,
        p: P,
    ) -> Result<Vec<EventEnvelope>, LedgerError> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(p, |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for payload in rows {
            out.push(decode(&payload?)?);
        }
        Ok(out)
    }
}

/// Appends via a bare connection.
pub fn append_with(conn: &Connection, envelope: &EventEnvelope) -> Result<(), LedgerError> {
    let row = encode(envelope)?;
    conn.execute(
        "INSERT INTO events
            (id, schema_version, occurred_at, trace_id, task_id, run_id, attempt_id, worker_id, event_type, payload)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            envelope.id.to_string(),
            envelope.schema_version,
            envelope.occurred_at.to_rfc3339(),
            envelope.trace_id.to_string(),
            row.task_id,
            row.run_id,
            envelope.attempt_id.map(|a| a.to_string()),
            envelope.worker_id.as_ref().map(WorkerId::to_string),
            row.event_type,
            row.payload,
        ],
    )?;
    Ok(())
}

/// Appends inside an existing transaction, so a caller can bundle the ledger write with
/// its own state write.
pub fn append_in_tx(tx: &Transaction<'_>, envelope: &EventEnvelope) -> Result<(), LedgerError> {
    append_with(tx, envelope)
}

/// The denormalized columns derived from an envelope.
///
/// `task_id` and `run_id` are lifted out of the payload into their own columns purely so
/// they can be indexed; the payload remains the authoritative copy.
struct EventRow {
    event_type: &'static str,
    payload: String,
    task_id: Option<String>,
    run_id: Option<String>,
}

/// Splits an envelope into its row representation.
fn encode(envelope: &EventEnvelope) -> Result<EventRow, LedgerError> {
    Ok(EventRow {
        event_type: envelope.event_type(),
        payload: serde_json::to_string(envelope)?,
        task_id: envelope.event.task_id().map(|t| t.as_str().to_string()),
        run_id: envelope.event.run_id().map(|r| r.to_string()),
    })
}

/// Rebuilds an envelope from its stored payload.
fn decode(payload: &str) -> Result<EventEnvelope, LedgerError> {
    Ok(serde_json::from_str(payload)?)
}

/// Convenience: builds and appends an event in one call.
pub fn record(
    ledger: &EventLedger,
    trace_id: TraceId,
    occurred_at: DateTime<Utc>,
    event: PearlEvent,
) -> Result<EventEnvelope, LedgerError> {
    let envelope = EventEnvelope::new(trace_id, occurred_at, event);
    ledger.append(&envelope)?;
    Ok(envelope)
}

/// Ledger failures.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("event payload serialization failed: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("stored event id is not a valid uuid: {0}")]
    BadEventId(#[from] uuid::Error),
}

impl LedgerError {
    /// Whether this error is the append-only trigger firing.
    ///
    /// Callers use this to distinguish "someone tried to rewrite history" from an
    /// ordinary storage fault; the former is a Constitution violation, not a bug.
    pub fn is_append_only_violation(&self) -> bool {
        match self {
            LedgerError::Sqlite(e) => e.to_string().contains("append-only"),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pearl_core::{PrecisionClass, QualitySpec, RunId, TaskState};

    fn ts() -> DateTime<Utc> {
        DateTime::from_timestamp(1_786_838_400, 0).unwrap()
    }

    fn task() -> TaskId {
        TaskId::parse("daily.digest").unwrap()
    }

    fn created() -> PearlEvent {
        PearlEvent::TaskCreated {
            task_id: task(),
            task_type: "digest".into(),
            precision_class: Some(PrecisionClass::P1),
            quality: QualitySpec::mechanical(),
        }
    }

    #[test]
    fn append_then_read_round_trips() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let trace = TraceId::new();
        let written = record(&ledger, trace, ts(), created()).unwrap();

        let read = ledger.read_trace(trace).unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0], written);
    }

    #[test]
    fn update_is_refused_by_the_database() {
        let ledger = EventLedger::open_in_memory().unwrap();
        record(&ledger, TraceId::new(), ts(), created()).unwrap();

        let err = ledger
            .connection()
            .execute("UPDATE events SET event_type = 'tampered'", [])
            .unwrap_err();
        assert!(
            err.to_string().contains("append-only"),
            "expected append-only abort, got: {err}"
        );
    }

    #[test]
    fn delete_is_refused_by_the_database() {
        let ledger = EventLedger::open_in_memory().unwrap();
        record(&ledger, TraceId::new(), ts(), created()).unwrap();

        let err = ledger
            .connection()
            .execute("DELETE FROM events", [])
            .unwrap_err();
        assert!(err.to_string().contains("append-only"));
        assert_eq!(ledger.count().unwrap(), 1, "the event must survive");
    }

    #[test]
    fn append_only_violation_is_distinguishable() {
        let ledger = EventLedger::open_in_memory().unwrap();
        record(&ledger, TraceId::new(), ts(), created()).unwrap();
        let err: LedgerError = ledger
            .connection()
            .execute("DELETE FROM events", [])
            .unwrap_err()
            .into();
        assert!(err.is_append_only_violation());
    }

    #[test]
    fn events_read_back_in_generation_order() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let trace = TraceId::new();
        let mut expected = Vec::new();
        for i in 0..10u32 {
            let env = record(
                &ledger,
                trace,
                ts(),
                PearlEvent::TaskPlanned {
                    task_id: task(),
                    step_count: i,
                },
            )
            .unwrap();
            expected.push(env.id);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let actual: Vec<_> = ledger
            .read_trace(trace)
            .unwrap()
            .iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn traces_are_isolated_from_each_other() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let a = TraceId::new();
        let b = TraceId::new();
        record(&ledger, a, ts(), created()).unwrap();
        record(&ledger, b, ts(), created()).unwrap();
        record(&ledger, b, ts(), created()).unwrap();

        assert_eq!(ledger.read_trace(a).unwrap().len(), 1);
        assert_eq!(ledger.read_trace(b).unwrap().len(), 2);
        assert_eq!(ledger.count().unwrap(), 3);
    }

    #[test]
    fn task_index_finds_events_by_task() {
        let ledger = EventLedger::open_in_memory().unwrap();
        record(&ledger, TraceId::new(), ts(), created()).unwrap();
        record(
            &ledger,
            TraceId::new(),
            ts(),
            PearlEvent::TaskStateChanged {
                task_id: task(),
                from: TaskState::Created,
                to: TaskState::Planning,
                reason: None,
            },
        )
        .unwrap();

        assert_eq!(ledger.read_task(&task()).unwrap().len(), 2);
        let other = TaskId::parse("other.task").unwrap();
        assert!(ledger.read_task(&other).unwrap().is_empty());
    }

    #[test]
    fn read_by_type_filters() {
        let ledger = EventLedger::open_in_memory().unwrap();
        record(&ledger, TraceId::new(), ts(), created()).unwrap();
        record(
            &ledger,
            TraceId::new(),
            ts(),
            PearlEvent::TaskPlanned {
                task_id: task(),
                step_count: 2,
            },
        )
        .unwrap();

        assert_eq!(ledger.read_by_type("task.created").unwrap().len(), 1);
        assert_eq!(ledger.read_by_type("task.planned").unwrap().len(), 1);
        assert!(ledger.read_by_type("task.completed").unwrap().is_empty());
    }

    #[test]
    fn batch_append_is_atomic() {
        let mut ledger = EventLedger::open_in_memory().unwrap();
        let trace = TraceId::new();
        let good = EventEnvelope::new(trace, ts(), created());
        // Reusing the same id forces a primary-key conflict on the second insert.
        let dup = good.clone();

        let err = ledger.append_batch(&[good, dup]).unwrap_err();
        assert!(matches!(err, LedgerError::Sqlite(_)));
        assert_eq!(
            ledger.count().unwrap(),
            0,
            "a failed batch must leave nothing behind"
        );
    }

    #[test]
    fn batch_append_writes_all_on_success() {
        let mut ledger = EventLedger::open_in_memory().unwrap();
        let trace = TraceId::new();
        let batch: Vec<_> = (0..5)
            .map(|i| {
                EventEnvelope::new(
                    trace,
                    ts(),
                    PearlEvent::TaskPlanned {
                        task_id: task(),
                        step_count: i,
                    },
                )
            })
            .collect();

        ledger.append_batch(&batch).unwrap();
        assert_eq!(ledger.count().unwrap(), 5);
    }

    #[test]
    fn latest_event_id_tracks_the_tail() {
        let ledger = EventLedger::open_in_memory().unwrap();
        assert_eq!(ledger.latest_event_id().unwrap(), None);

        let first = record(&ledger, TraceId::new(), ts(), created()).unwrap();
        assert_eq!(ledger.latest_event_id().unwrap(), Some(first.id));

        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = record(&ledger, TraceId::new(), ts(), created()).unwrap();
        assert_eq!(ledger.latest_event_id().unwrap(), Some(second.id));
    }

    #[test]
    fn ledger_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let trace = TraceId::new();

        {
            let ledger = EventLedger::open(&path).unwrap();
            record(&ledger, trace, ts(), created()).unwrap();
        }

        // Article 6: after a restart the system must still know what happened.
        let reopened = EventLedger::open(&path).unwrap();
        assert_eq!(reopened.count().unwrap(), 1);
        assert_eq!(reopened.read_trace(trace).unwrap().len(), 1);
    }

    #[test]
    fn run_id_is_indexed_for_run_scoped_events() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let run_id = RunId::new();
        record(
            &ledger,
            TraceId::new(),
            ts(),
            PearlEvent::RunStarted {
                task_id: task(),
                run_id,
                config_revision: "system@builtin".into(),
                config_hash: "deadbeef".into(),
            },
        )
        .unwrap();

        let stored: String = ledger
            .connection()
            .query_row("SELECT run_id FROM events LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, run_id.to_string());
    }
}
