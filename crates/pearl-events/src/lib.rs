//! # pearl-events
//!
//! The append-only event ledger — the source of truth for all task history (ADR-0001).
//!
//! Everything else in PEARL is a projection of this table. Materialized state can be
//! dropped and rebuilt; the ledger cannot be rewritten. `UPDATE` and `DELETE` on
//! `events` are refused by database triggers, not merely absent from the API.
//!
//! ```
//! use pearl_events::{EventLedger, PearlEvent, record};
//! use pearl_core::{TaskId, TaskPlan, TraceId, PrecisionClass, QualitySpec};
//! use chrono::Utc;
//!
//! let ledger = EventLedger::open_in_memory().unwrap();
//! let trace = TraceId::new();
//! record(&ledger, trace, Utc::now(), PearlEvent::TaskCreated {
//!     task_id: TaskId::parse("daily.digest").unwrap(),
//!     task_type: "digest".into(),
//!     precision_class: Some(PrecisionClass::P1),
//!     quality: QualitySpec::mechanical(),
//!     plan: TaskPlan::empty(),
//! }).unwrap();
//!
//! assert_eq!(ledger.read_trace(trace).unwrap().len(), 1);
//! ```

pub mod event;
pub mod ledger;

pub use event::{EventEnvelope, PearlEvent, RunOutcome};
pub use ledger::{append_in_tx, append_with, record, EventLedger, LedgerError};
