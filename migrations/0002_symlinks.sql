-- DCFS: symbolic links
-- Version: 0002
--
-- Additive: adds a nullable column and widens an existing CHECK constraint so
-- 'symlink' is allowed alongside the kinds 0001 created. No data is read,
-- rewritten or dropped, and applying it twice is a no-op.
--
-- Rollback: drop the column and restore the narrower constraint. Do that only
-- once no rows have kind = 'symlink', or the constraint will not validate.

ALTER TABLE nodes ADD COLUMN IF NOT EXISTS link_target BYTEA;

ALTER TABLE nodes DROP CONSTRAINT IF EXISTS nodes_kind_check;
ALTER TABLE nodes ADD CONSTRAINT nodes_kind_check
    CHECK (kind IN ('file', 'directory', 'symlink'));

-- A link must carry a target, and nothing else may.
ALTER TABLE nodes DROP CONSTRAINT IF EXISTS nodes_link_target_check;
ALTER TABLE nodes ADD CONSTRAINT nodes_link_target_check
    CHECK ((kind = 'symlink') = (link_target IS NOT NULL));
