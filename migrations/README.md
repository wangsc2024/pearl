# Database migrations

The SQLite file PEARL writes holds two things: the event ledger (truth, append-only) and the
projections derived from it (a cache). Both are versioned here, and both are applied on open —
there is no separate migrate step to forget.

## Why this directory exists

The schema used to be a `const SCHEMA: &str` of `CREATE TABLE IF NOT EXISTS` statements applied
on every open. That is idempotent for *new* tables and silently useless for *new columns*:
`CREATE TABLE IF NOT EXISTS` on an existing table does nothing at all, so adding a column meant
every existing database had to be deleted. Two changes already paid that price — `tasks.plan`
and the schedule's task-spec columns — which is what made a real migration runner necessary
rather than merely tidy.

## Layout

```
migrations/
  ledger/       component "ledger"      — owned by pearl-events
  projections/  component "projections" — owned by pearl-state
```

Two components rather than one list, because the ledger is usable on its own: `pearl-events`
knows nothing about projections and must be able to open a database without them. Each
component keeps its own version counter in `schema_migrations`, so the two advance
independently.

`PRAGMA user_version` is not used. It is a single integer per file, and there are two
independent version lines; a table also records *when* each migration landed, which is the kind
of thing you want when a database misbehaves.

## Rules

- **Files are append-only.** Once a numbered file has shipped, editing it changes nothing for
  databases that already applied it, so the two would diverge silently. Add the next number.
- **Number from 1, with no gaps.** The runner rejects a set that skips or repeats a version,
  because that is a merge accident, not a schema.
- **One file, one transaction.** A migration either lands whole or not at all.
- **A database from the future is refused, not opened.** If a file records a version this build
  does not know, a newer PEARL wrote it and this build would misread it.

## Adding a column

```sql
-- migrations/projections/0004_example.sql
ALTER TABLE tasks ADD COLUMN example TEXT;
```

Then register it in `crates/pearl-state/src/migrations.rs`, declaring the column it adds:

```rust
Migration::new(4, "example", include_str!(".../0004_example.sql"))
    .adding_columns(&[("tasks", "example")]),
```

The declaration is what makes the migration safe to apply to a database that already has the
column — the ones created by the interim builds that wrote columns without recording a version.
The runner checks `PRAGMA table_info` first: all columns present means the migration's goal is
already met, so it is recorded and not run. Some present but not all is an error, because that
is a shape no migration produced.

`NOT NULL` columns need a `DEFAULT`: SQLite cannot add a `NOT NULL` column to a table with rows
otherwise, and existing rows have to be given some value. Pick one a reader can recognise as
"this row predates the column" and make the code that reads it say so.
