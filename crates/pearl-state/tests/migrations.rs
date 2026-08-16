//! Opening databases that were written by older builds.
//!
//! The `CREATE TABLE IF NOT EXISTS` schema this replaced could add tables but never columns,
//! so both column additions PEARL has shipped — `tasks.plan` and the schedule's task-spec
//! columns — required deleting the database. These tests are the proof that they no longer do,
//! against the three shapes that exist in the wild:
//!
//! 1. **v1** — written before either column, by the first release.
//! 2. **interim** — written by the builds that added the columns in their `CREATE TABLE`
//!    statement without recording a version, so a database created fresh by them has the
//!    columns while one carried over from v1 does not.
//! 3. **current** — created by this build.

use chrono::Utc;
use pearl_core::{AssuranceStep, QualitySpec, TaskId, TaskPlan};
use pearl_state::{migrations, ScheduleRecord, StateStore, TaskSubmission};
use rusqlite::Connection;
use tempfile::TempDir;

/// The projections exactly as the first release wrote them: no `tasks.plan`, and a `schedules`
/// table that knows a task type but not which spec to submit.
const V1_PROJECTIONS: &str = r#"
CREATE TABLE tasks (
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
CREATE TABLE schedules (
    schedule_id     TEXT PRIMARY KEY,
    task_type       TEXT    NOT NULL,
    cron_expr       TEXT,
    interval_secs   INTEGER,
    next_run_at     TEXT,
    enabled         INTEGER NOT NULL DEFAULT 1,
    created_at      TEXT    NOT NULL
);
"#;

/// The interim shape: the columns are present, but nothing recorded that they were added.
const INTERIM_PROJECTIONS: &str = r#"
CREATE TABLE tasks (
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
    last_reason     TEXT,
    plan            TEXT
);
CREATE TABLE schedules (
    schedule_id     TEXT PRIMARY KEY,
    task_type       TEXT    NOT NULL,
    spec_path       TEXT    NOT NULL,
    cron_expr       TEXT,
    interval_secs   INTEGER,
    timezone        TEXT,
    misfire_policy  TEXT    NOT NULL DEFAULT 'skip',
    next_run_at     TEXT,
    last_triggered_at TEXT,
    enabled         INTEGER NOT NULL DEFAULT 1,
    created_at      TEXT    NOT NULL
);
"#;

fn database_with(ddl: &str) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("pearl.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(ddl).unwrap();
    drop(conn);
    (dir, path)
}

fn columns(store: &StateStore, table: &str) -> Vec<String> {
    let conn = store.ledger().connection();
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    let rows = stmt.query_map([], |row| row.get::<_, String>(1)).unwrap();
    rows.map(Result::unwrap).collect()
}

fn latest() -> u32 {
    migrations::MIGRATIONS.last().unwrap().version
}

#[test]
fn a_v1_database_is_upgraded_in_place_rather_than_rebuilt() {
    let (_dir, path) = database_with(V1_PROJECTIONS);
    // A row that must survive: the whole point is not to start over.
    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO schedules (schedule_id, task_type, interval_secs, enabled, created_at)
         VALUES ('legacy.hourly', 'digest', 3600, 1, ?1)",
        [Utc::now().to_rfc3339()],
    )
    .unwrap();
    drop(conn);

    let mut store = StateStore::open(&path).unwrap();

    assert_eq!(store.schema_version().unwrap(), latest());
    assert!(columns(&store, "tasks").contains(&"plan".to_string()));
    assert!(columns(&store, "schedules").contains(&"spec_path".to_string()));

    // The schedule is still there, and says out loud that it predates the column it now has.
    let schedules = store.list_schedules().unwrap();
    assert_eq!(schedules.len(), 1);
    assert_eq!(schedules[0].schedule_id, "legacy.hourly");
    assert_eq!(
        schedules[0].spec_path, "",
        "a row from before the column cannot invent a spec path"
    );
    assert_eq!(schedules[0].misfire_policy, "skip");

    // And the column the upgrade added is usable, which is what "rebuild the database" used
    // to be the only way to achieve.
    let task = store
        .create_task(
            TaskSubmission::new(
                TaskId::parse("after.upgrade").unwrap(),
                "digest",
                None,
                QualitySpec::mechanical(),
            )
            .with_plan(TaskPlan {
                capability: Some("script.task-score".into()),
                assurance: vec![AssuranceStep::script("verifier.task-result")],
                timeout_seconds: Some(30),
                ..TaskPlan::empty()
            }),
            Utc::now(),
        )
        .unwrap();
    assert_eq!(task.plan.capability.as_deref(), Some("script.task-score"));
    let reread = store.get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(reread.plan, task.plan);
}

#[test]
fn an_interim_database_adopts_the_migrations_whose_columns_it_already_has() {
    let (_dir, path) = database_with(INTERIM_PROJECTIONS);

    let store = StateStore::open(&path).unwrap();

    assert_eq!(store.schema_version().unwrap(), latest());
    // Re-running the ALTERs would have failed with "duplicate column name", which is what
    // makes an unversioned database with the columns already present the awkward case.
    let applied: Vec<(u32, String)> = {
        let conn = store.ledger().connection();
        let mut stmt = conn
            .prepare(
                "SELECT version, name FROM schema_migrations
                 WHERE component = 'projections' ORDER BY version",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(applied.len(), latest() as usize);
    assert_eq!(applied.last().unwrap().1, "schedule-task-spec");
}

#[test]
fn a_current_database_is_untouched_on_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("pearl.db");

    let first = StateStore::open(&path).unwrap();
    let version = first.schema_version().unwrap();
    drop(first);

    let again = StateStore::open(&path).unwrap();
    assert_eq!(again.schema_version().unwrap(), version);
    assert_eq!(version, latest());
}

#[test]
fn the_ledger_and_the_projections_keep_separate_version_lines() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("pearl.db");
    let store = StateStore::open(&path).unwrap();

    assert_eq!(
        store.ledger().schema_version().unwrap(),
        pearl_events::ledger::MIGRATIONS.last().unwrap().version
    );
    assert_eq!(store.schema_version().unwrap(), latest());
}

#[test]
fn a_database_written_by_a_newer_build_is_refused_rather_than_misread() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("pearl.db");
    StateStore::open(&path).unwrap();

    // Something a later PEARL applied that this build has never heard of.
    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO schema_migrations (component, version, name, applied_at)
         VALUES ('projections', 999, 'from-the-future', ?1)",
        [Utc::now().to_rfc3339()],
    )
    .unwrap();
    drop(conn);

    let Err(err) = StateStore::open(&path) else {
        panic!("a database from the future must not open");
    };
    assert!(
        err.to_string().contains("newer PEARL"),
        "expected a refusal naming the cause, got: {err}"
    );
}

#[test]
fn a_rebuild_does_not_forget_which_schema_it_is_rebuilding_into() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("pearl.db");
    let mut store = StateStore::open(&path).unwrap();
    store
        .create_task(
            TaskSubmission::new(
                TaskId::parse("t.one").unwrap(),
                "digest",
                None,
                QualitySpec::mechanical(),
            ),
            Utc::now(),
        )
        .unwrap();

    store.rebuild_from_ledger().unwrap();

    assert_eq!(
        store.schema_version().unwrap(),
        latest(),
        "clearing the projections must not clear their version"
    );
}

#[test]
fn a_schedule_written_today_round_trips_through_the_migrated_schema() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("pearl.db");
    let mut store = StateStore::open(&path).unwrap();
    let now = Utc::now();

    store
        .upsert_schedule(
            &ScheduleRecord::interval("daily.digest", "digest", "specs/digest.yaml", 3600, now)
                .with_misfire("run_once"),
        )
        .unwrap();

    let stored = store.get_schedule("daily.digest").unwrap().unwrap();
    assert_eq!(stored.spec_path, "specs/digest.yaml");
    assert_eq!(stored.misfire_policy, "run_once");
    assert_eq!(stored.interval_secs, Some(3600));
}
