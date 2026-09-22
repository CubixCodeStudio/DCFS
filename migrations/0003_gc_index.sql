-- DiscordFS: index the object back-reference
-- Version: 0003
--
-- Additive: one index. Garbage collection asks "does any other version still
-- reference this object?" and deletes chunk rows by object id, both of which
-- scanned the whole table without this. No data is read, rewritten or dropped,
-- and applying it twice is a no-op.
--
-- Rollback: DROP INDEX idx_file_chunks_object.

CREATE INDEX IF NOT EXISTS idx_file_chunks_object ON file_chunks (object_id);
