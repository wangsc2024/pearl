//! The projections' schema, one file per version.
//!
//! ADR-0001: the ledger is truth, these tables are a derived cache. That makes them the
//! *droppable* half of the database — and yet still the half that needs migrating, because
//! dropping them means replaying every event, which is neither free nor something to make a
//! routine part of shipping a new column.
//!
//! Versions are recorded under a component of their own, separate from the ledger's, so that
//! `pearl-events` can open a database without waiting on a projection change it has no
//! opinion about.

use pearl_events::Migration;

/// The name this schema keeps its version under.
pub const COMPONENT: &str = "projections";

/// Every projection migration, in order.
///
/// Append only. Editing a shipped file changes nothing for databases that already applied it,
/// so the two would drift apart with no way to tell.
pub const MIGRATIONS: &[Migration] = &[
    Migration::new(
        1,
        "baseline",
        include_str!("../../../migrations/projections/0001_baseline.sql"),
    ),
    // Both of the changes that used to force a database rebuild, now expressed as the column
    // additions they always were.
    Migration::new(
        2,
        "task-plan",
        include_str!("../../../migrations/projections/0002_task_plan.sql"),
    )
    .adding_columns(&[("tasks", "plan")]),
    Migration::new(
        3,
        "schedule-task-spec",
        include_str!("../../../migrations/projections/0003_schedule_task_spec.sql"),
    )
    .adding_columns(&[
        ("schedules", "spec_path"),
        ("schedules", "timezone"),
        ("schedules", "misfire_policy"),
        ("schedules", "last_triggered_at"),
    ]),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StateStore;

    #[test]
    fn every_migration_is_numbered_in_sequence() {
        for (index, migration) in MIGRATIONS.iter().enumerate() {
            assert_eq!(
                migration.version,
                index as u32 + 1,
                "migration '{}' is out of sequence",
                migration.name
            );
        }
    }

    #[test]
    fn a_fresh_store_lands_on_the_latest_version() {
        let store = StateStore::open_in_memory().unwrap();
        let latest = MIGRATIONS.last().unwrap().version;
        assert_eq!(store.schema_version().unwrap(), latest);
    }

    /// The columns the migrations add have to actually be there, or every write below would
    /// fail at runtime with "no such column" rather than here.
    #[test]
    fn the_columns_the_migrations_declare_exist_afterwards() {
        let store = StateStore::open_in_memory().unwrap();
        let conn = store.ledger().connection();
        for migration in MIGRATIONS {
            for (table, column) in migration.adds_columns {
                let mut stmt = conn
                    .prepare(&format!("PRAGMA table_info({table})"))
                    .unwrap();
                let names: Vec<String> = stmt
                    .query_map([], |row| row.get::<_, String>(1))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect();
                assert!(
                    names.contains(&column.to_string()),
                    "{table}.{column} is missing after migration '{}'",
                    migration.name
                );
            }
        }
    }
}
