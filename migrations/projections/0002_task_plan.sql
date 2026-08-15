-- The declared execution and verification plan, as submitted (SS 22, SS 32).
--
-- JSON rather than normalised columns because it is read whole, written once, and never
-- queried by field. Nullable because a task submitted without an explicit plan is legitimate:
-- the quality contract alone is enough to route and verify it.

ALTER TABLE tasks ADD COLUMN plan TEXT;
