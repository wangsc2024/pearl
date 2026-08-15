//! Typed identifiers.
//!
//! All ledger-bearing identifiers are UUIDv7 so that lexicographic order equals
//! chronological order. ADR-0001 relies on this: the event ledger needs no separate
//! sequence counter to establish "what happened first".

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// Declares a newtype over `Uuid` with UUIDv7 generation and string round-tripping.
macro_rules! uuid_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Generates a new time-ordered identifier.
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// Wraps an existing UUID, e.g. when rehydrating from the ledger.
            pub const fn from_uuid(id: Uuid) -> Self {
                Self(id)
            }

            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }

            /// Parses the canonical hyphenated form.
            pub fn parse(s: &str) -> Result<Self, uuid::Error> {
                Ok(Self(Uuid::parse_str(s)?))
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl From<Uuid> for $name {
            fn from(id: Uuid) -> Self {
                Self(id)
            }
        }
    };
}

uuid_id!(EventId, "Identifies one immutable ledger record.");
uuid_id!(
    TraceId,
    "Correlates every event belonging to one logical task, across runs and attempts."
);
uuid_id!(RunId, "Identifies one execution of a task.");
uuid_id!(AttemptId, "Identifies one try within a run.");
uuid_id!(LeaseId, "Identifies one worker's claim on a task.");
uuid_id!(
    CheckpointId,
    "Identifies one committed durable step boundary."
);

/// A task identifier supplied by the submitter rather than generated.
///
/// Task ids are author-chosen because they participate in idempotency keys
/// (Article 5): `todoist:complete:{task_id}:{run_id}` must be stable across retries,
/// which a freshly generated UUID would not be.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(String);

impl TaskId {
    /// Accepts a lowercase dotted/dashed identifier.
    ///
    /// The character restriction exists so that a task id can be embedded in an
    /// idempotency key without escaping, and in a filesystem path without surprises.
    pub fn parse(raw: impl Into<String>) -> Result<Self, InvalidTaskId> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(InvalidTaskId::Empty);
        }
        if raw.len() > 200 {
            return Err(InvalidTaskId::TooLong(raw.len()));
        }
        let first_ok = raw
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        if !first_ok {
            return Err(InvalidTaskId::BadLeadingChar(raw));
        }
        let body_ok = raw
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
        if !body_ok {
            return Err(InvalidTaskId::IllegalChar(raw));
        }
        Ok(Self(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a task id was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InvalidTaskId {
    #[error("task id must not be empty")]
    Empty,
    #[error("task id is {0} characters, maximum is 200")]
    TooLong(usize),
    #[error("task id '{0}' must start with a lowercase letter or digit")]
    BadLeadingChar(String),
    #[error("task id '{0}' may only contain [a-z0-9._-]")]
    IllegalChar(String),
}

/// Identifies a worker process that can hold leases.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkerId(String);

impl WorkerId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Derives an identifier from host and pid, which is what a reaper needs to
    /// decide whether the holder of an expired lease is plausibly still alive.
    pub fn from_host_pid(host: &str, pid: u32) -> Self {
        Self(format!("{host}:{pid}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuidv7_ids_sort_chronologically() {
        let mut ids = Vec::new();
        for _ in 0..50 {
            ids.push(EventId::new());
            // UUIDv7 has millisecond precision; without a gap, ordering within the
            // same millisecond is decided by the random tail, not by time.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "generation order must equal sort order");
    }

    #[test]
    fn ids_round_trip_through_string() {
        let id = RunId::new();
        assert_eq!(RunId::parse(&id.to_string()).unwrap(), id);
    }

    #[test]
    fn task_id_accepts_idempotency_safe_forms() {
        for good in ["digest", "daily.digest", "todoist_task-42", "t1"] {
            assert!(TaskId::parse(good).is_ok(), "{good} should be accepted");
        }
    }

    #[test]
    fn task_id_rejects_forms_that_would_break_keys_or_paths() {
        assert_eq!(TaskId::parse(""), Err(InvalidTaskId::Empty));
        // Uppercase and ':' are rejected because ':' is the idempotency key separator.
        assert!(matches!(
            TaskId::parse("Digest"),
            Err(InvalidTaskId::BadLeadingChar(_))
        ));
        assert!(matches!(
            TaskId::parse("a:b"),
            Err(InvalidTaskId::IllegalChar(_))
        ));
        assert!(matches!(
            TaskId::parse("../escape"),
            Err(InvalidTaskId::BadLeadingChar(_))
        ));
    }

    #[test]
    fn worker_id_from_host_pid_is_stable() {
        assert_eq!(WorkerId::from_host_pid("box", 1234).as_str(), "box:1234");
    }
}
