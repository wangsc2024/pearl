//! Schema migrations — the database's own version history.
//!
//! The schema used to be a `const SCHEMA: &str` of `CREATE TABLE IF NOT EXISTS` statements
//! re-applied on every open. That is idempotent for new *tables* and silently useless for new
//! *columns*: `CREATE TABLE IF NOT EXISTS` against an existing table does nothing, so adding a
//! column meant deleting every existing database. This module is the version counter that was
//! missing.
//!
//! The runner is generic over which set of migrations it applies, because the file holds two
//! independently owned schemas: the ledger (`pearl-events`) and the projections
//! (`pearl-state`). `pearl-events` must be usable without projections, so the ledger cannot
//! wait on a version line it does not own. Each *component* keeps its own counter.
//!
//! `PRAGMA user_version` is not used: it is one integer per file, there are two version lines,
//! and a table can also record *when* each migration landed — which is what you want when a
//! database misbehaves.

use chrono::Utc;
use rusqlite::{params, Connection};

/// Where the applied versions are recorded.
///
/// Not a projection: `rebuild_from_ledger` must not clear it, or a rebuild would forget the
/// shape of the tables it is rebuilding into.
const REGISTRY: &str = r#"
CREATE TABLE IF NOT EXISTS schema_migrations (
    component   TEXT    NOT NULL,
    version     INTEGER NOT NULL,
    name        TEXT    NOT NULL,
    applied_at  TEXT    NOT NULL,
    PRIMARY KEY (component, version)
);
"#;

/// One forward-only schema change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Migration {
    /// Position in its component's sequence, starting at 1.
    pub version: u32,
    /// What it does, for the record and for error messages.
    pub name: &'static str,
    /// The statements, applied as one transaction.
    pub sql: &'static str,
    /// Columns this migration adds, as `(table, column)`.
    ///
    /// Declared rather than parsed out of the SQL, so the runner can check whether the
    /// migration's goal is already met before running it. That is what makes it safe against
    /// the databases written by builds that added columns without recording a version.
    pub adds_columns: &'static [(&'static str, &'static str)],
}

impl Migration {
    /// A migration that creates things, and so has no columns to reconcile.
    pub const fn new(version: u32, name: &'static str, sql: &'static str) -> Self {
        Self {
            version,
            name,
            sql,
            adds_columns: &[],
        }
    }

    /// Declares the columns this migration adds to existing tables.
    pub const fn adding_columns(
        mut self,
        columns: &'static [(&'static str, &'static str)],
    ) -> Self {
        self.adds_columns = columns;
        self
    }
}

/// What `apply` did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Applied {
    /// Migrations whose statements were executed.
    pub executed: Vec<u32>,
    /// Migrations recorded without executing, because the database already had their columns.
    pub adopted: Vec<u32>,
    /// The component's version after this call.
    pub version: u32,
}

impl Applied {
    /// Whether the database was already up to date.
    pub fn is_noop(&self) -> bool {
        self.executed.is_empty() && self.adopted.is_empty()
    }
}

/// Why a schema could not be brought up to date.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    /// A migration's statements failed.
    #[error("migration {version} ({name}) of '{component}' failed: {detail}")]
    Failed {
        component: String,
        version: u32,
        name: &'static str,
        detail: String,
    },

    /// The migration set itself is malformed — a merge accident, not a schema.
    #[error("migrations for '{component}' are misnumbered: expected version {expected}, found {found} ({name})")]
    Misnumbered {
        component: String,
        expected: u32,
        found: u32,
        name: &'static str,
    },

    /// The database records a version this build does not have.
    ///
    /// Refused rather than opened: a build that does not know about a column would read
    /// rows it cannot interpret and write rows the newer build cannot.
    #[error("database is at version {found} for '{component}' but this build only knows {known}; it was written by a newer PEARL")]
    FromTheFuture {
        component: String,
        found: u32,
        known: u32,
    },

    /// Some of a migration's columns are present and some are not.
    #[error("migration {version} ({name}) of '{component}' adds {total} column(s) but {present} already exist; this database matches no released schema and needs repair by hand")]
    PartiallyPresent {
        component: String,
        version: u32,
        name: &'static str,
        present: usize,
        total: usize,
    },

    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// Brings one component's schema up to date, and returns what that took.
///
/// Each migration runs in its own transaction together with the row recording it, so a
/// migration that lands is a migration that is remembered.
pub fn apply(
    conn: &mut Connection,
    component: &str,
    migrations: &[Migration],
) -> Result<Applied, MigrationError> {
    conn.execute_batch(REGISTRY)?;
    check_numbering(component, migrations)?;

    let known = migrations.last().map(|m| m.version).unwrap_or(0);
    let current = current_version(conn, component)?;
    if current > known {
        return Err(MigrationError::FromTheFuture {
            component: component.to_string(),
            found: current,
            known,
        });
    }

    let mut applied = Applied {
        version: current,
        ..Applied::default()
    };
    for migration in migrations.iter().filter(|m| m.version > current) {
        if already_satisfied(conn, component, migration)? {
            record(conn, component, migration)?;
            applied.adopted.push(migration.version);
        } else {
            execute(conn, component, migration)?;
            applied.executed.push(migration.version);
        }
        applied.version = migration.version;
    }
    Ok(applied)
}

/// The highest version recorded for a component, or 0 for a database that has none.
pub fn current_version(conn: &Connection, component: &str) -> Result<u32, MigrationError> {
    conn.execute_batch(REGISTRY)?;
    let version: Option<u32> = conn.query_row(
        "SELECT MAX(version) FROM schema_migrations WHERE component = ?1",
        params![component],
        |row| row.get(0),
    )?;
    Ok(version.unwrap_or(0))
}

/// Versions must run 1, 2, 3… A gap or a repeat means two branches numbered the same file.
fn check_numbering(component: &str, migrations: &[Migration]) -> Result<(), MigrationError> {
    for (index, migration) in migrations.iter().enumerate() {
        let expected = index as u32 + 1;
        if migration.version != expected {
            return Err(MigrationError::Misnumbered {
                component: component.to_string(),
                expected,
                found: migration.version,
                name: migration.name,
            });
        }
    }
    Ok(())
}

/// Whether the database already has every column this migration adds.
///
/// True only for a migration that declares columns and finds all of them: a migration that
/// creates tables declares none, so it always runs (its statements are `IF NOT EXISTS`).
fn already_satisfied(
    conn: &Connection,
    component: &str,
    migration: &Migration,
) -> Result<bool, MigrationError> {
    if migration.adds_columns.is_empty() {
        return Ok(false);
    }
    let mut present = 0;
    for (table, column) in migration.adds_columns {
        if has_column(conn, table, column)? {
            present += 1;
        }
    }
    if present == 0 {
        return Ok(false);
    }
    if present < migration.adds_columns.len() {
        return Err(MigrationError::PartiallyPresent {
            component: component.to_string(),
            version: migration.version,
            name: migration.name,
            present,
            total: migration.adds_columns.len(),
        });
    }
    Ok(true)
}

/// Whether a table has a column. A table that does not exist has no columns.
fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, MigrationError> {
    // `PRAGMA table_info` takes an identifier, which cannot be bound as a parameter. The
    // table names come from `Migration::adds_columns`, which is `&'static` data compiled into
    // the binary, so there is no untrusted input to inject here — but keep it that way.
    debug_assert!(
        table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "migration table names are identifiers"
    );
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Runs a migration's statements and records it, atomically.
fn execute(
    conn: &mut Connection,
    component: &str,
    migration: &Migration,
) -> Result<(), MigrationError> {
    let tx = conn.transaction()?;
    tx.execute_batch(migration.sql)
        .map_err(|e| MigrationError::Failed {
            component: component.to_string(),
            version: migration.version,
            name: migration.name,
            detail: e.to_string(),
        })?;
    insert_row(&tx, component, migration)?;
    tx.commit()?;
    Ok(())
}

/// Records a migration whose effect the database already had.
fn record(
    conn: &mut Connection,
    component: &str,
    migration: &Migration,
) -> Result<(), MigrationError> {
    let tx = conn.transaction()?;
    insert_row(&tx, component, migration)?;
    tx.commit()?;
    Ok(())
}

fn insert_row(
    conn: &Connection,
    component: &str,
    migration: &Migration,
) -> Result<(), MigrationError> {
    conn.execute(
        "INSERT INTO schema_migrations (component, version, name, applied_at)
         VALUES (?1,?2,?3,?4)",
        params![
            component,
            migration.version,
            migration.name,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CREATE: &str = "CREATE TABLE IF NOT EXISTS thing (id TEXT PRIMARY KEY);";
    const ADD: &str = "ALTER TABLE thing ADD COLUMN extra TEXT;";

    fn set() -> Vec<Migration> {
        vec![
            Migration::new(1, "create", CREATE),
            Migration::new(2, "extra", ADD).adding_columns(&[("thing", "extra")]),
        ]
    }

    fn columns(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        let rows = stmt.query_map([], |row| row.get::<_, String>(1)).unwrap();
        rows.map(Result::unwrap).collect()
    }

    #[test]
    fn a_fresh_database_gets_every_migration() {
        let mut conn = Connection::open_in_memory().unwrap();
        let applied = apply(&mut conn, "test", &set()).unwrap();
        assert_eq!(applied.executed, vec![1, 2]);
        assert!(applied.adopted.is_empty());
        assert_eq!(applied.version, 2);
        assert!(columns(&conn, "thing").contains(&"extra".to_string()));
    }

    #[test]
    fn a_second_open_does_nothing() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply(&mut conn, "test", &set()).unwrap();
        let again = apply(&mut conn, "test", &set()).unwrap();
        assert!(again.is_noop(), "{again:?}");
        assert_eq!(again.version, 2);
    }

    /// The case this module exists for: a database created before the column, which the old
    /// `CREATE TABLE IF NOT EXISTS` schema could never have fixed.
    #[test]
    fn a_database_from_before_the_column_gains_it() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(CREATE).unwrap();
        conn.execute("INSERT INTO thing (id) VALUES ('kept')", [])
            .unwrap();

        let applied = apply(&mut conn, "test", &set()).unwrap();

        assert_eq!(applied.executed, vec![1, 2]);
        assert!(columns(&conn, "thing").contains(&"extra".to_string()));
        // The row survives: a migration adds a column, it does not rebuild the table.
        let kept: String = conn
            .query_row("SELECT id FROM thing", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, "kept");
    }

    /// The interim shape: a build wrote the column but recorded no version. Re-running the
    /// `ALTER` would fail with "duplicate column name", so the migration is adopted instead.
    #[test]
    fn a_database_that_already_has_the_column_adopts_the_migration() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE thing (id TEXT PRIMARY KEY, extra TEXT);")
            .unwrap();

        let applied = apply(&mut conn, "test", &set()).unwrap();

        assert_eq!(applied.executed, vec![1], "the baseline is a no-op DDL");
        assert_eq!(applied.adopted, vec![2], "the column was already there");
        assert_eq!(applied.version, 2);
    }

    #[test]
    fn a_half_present_migration_is_refused_rather_than_guessed_at() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE thing (id TEXT PRIMARY KEY, a TEXT);")
            .unwrap();
        let migrations = vec![
            Migration::new(1, "create", CREATE),
            Migration::new(2, "two", "ALTER TABLE thing ADD COLUMN b TEXT;")
                .adding_columns(&[("thing", "a"), ("thing", "b")]),
        ];

        let err = apply(&mut conn, "test", &migrations).unwrap_err();
        assert!(
            matches!(
                err,
                MigrationError::PartiallyPresent {
                    present: 1,
                    total: 2,
                    ..
                }
            ),
            "got {err}"
        );
    }

    #[test]
    fn a_database_from_a_newer_build_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply(&mut conn, "test", &set()).unwrap();

        // This build only knows migration 1.
        let older = vec![Migration::new(1, "create", CREATE)];
        let err = apply(&mut conn, "test", &older).unwrap_err();
        assert!(
            matches!(
                err,
                MigrationError::FromTheFuture {
                    found: 2,
                    known: 1,
                    ..
                }
            ),
            "got {err}"
        );
    }

    #[test]
    fn a_misnumbered_set_is_refused_before_anything_runs() {
        let mut conn = Connection::open_in_memory().unwrap();
        let gap = vec![
            Migration::new(1, "create", CREATE),
            Migration::new(3, "skipped-two", ADD),
        ];
        let err = apply(&mut conn, "test", &gap).unwrap_err();
        assert!(
            matches!(
                err,
                MigrationError::Misnumbered {
                    expected: 2,
                    found: 3,
                    ..
                }
            ),
            "got {err}"
        );
        // Nothing ran: the set is rejected as a whole.
        assert_eq!(current_version(&conn, "test").unwrap(), 0);
    }

    #[test]
    fn a_failing_migration_leaves_no_trace_of_itself() {
        let mut conn = Connection::open_in_memory().unwrap();
        let broken = vec![
            Migration::new(1, "create", CREATE),
            Migration::new(2, "broken", "ALTER TABLE absent ADD COLUMN x TEXT;"),
        ];
        let err = apply(&mut conn, "test", &broken).unwrap_err();
        assert!(
            matches!(err, MigrationError::Failed { version: 2, .. }),
            "got {err}"
        );
        // The first migration stands; the second is not recorded, so a fixed build retries it.
        assert_eq!(current_version(&conn, "test").unwrap(), 1);
    }

    #[test]
    fn components_have_independent_version_lines() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply(&mut conn, "one", &set()).unwrap();
        assert_eq!(current_version(&conn, "two").unwrap(), 0);

        let other = vec![Migration::new(
            1,
            "other",
            "CREATE TABLE IF NOT EXISTS other (id TEXT);",
        )];
        apply(&mut conn, "two", &other).unwrap();
        assert_eq!(current_version(&conn, "one").unwrap(), 2);
        assert_eq!(current_version(&conn, "two").unwrap(), 1);
    }

    #[test]
    fn an_empty_set_is_valid_and_leaves_the_database_at_zero() {
        let mut conn = Connection::open_in_memory().unwrap();
        let applied = apply(&mut conn, "test", &[]).unwrap();
        assert!(applied.is_noop());
        assert_eq!(applied.version, 0);
    }
}
