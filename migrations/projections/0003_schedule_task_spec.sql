-- A schedule points at a task spec, not at a task (SS 47).
--
-- A schedule that named a task id would re-run one task id forever, overwriting the previous
-- occurrence's run and evidence. So each occurrence is submitted afresh from a declaration,
-- and the schedule has to record which declaration.
--
-- `spec_path` is NOT NULL because a schedule with nothing to submit is not a schedule. Rows
-- that predate this column have no answer to give, so they get `''`, which no filesystem
-- resolves: the daemon reports such a schedule as unreadable and carries on, rather than
-- firing something it had to invent. The alternative — deleting those rows — would silently
-- discard a schedule the operator created.
--
-- `misfire_policy` decides what a daemon does about occurrences that came due while it was
-- down. `skip` is the safe default for a row that never declared one: firing a backlog is
-- the surprising behaviour, not suppressing it.

ALTER TABLE schedules ADD COLUMN spec_path TEXT NOT NULL DEFAULT '';
ALTER TABLE schedules ADD COLUMN timezone TEXT;
ALTER TABLE schedules ADD COLUMN misfire_policy TEXT NOT NULL DEFAULT 'skip';
ALTER TABLE schedules ADD COLUMN last_triggered_at TEXT;
