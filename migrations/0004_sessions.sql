-- DCFS: usable sessions, and dropping a table that was never used
-- Version: 0004
--
-- Additive except for one guarded drop: `upload_jobs` was created by an earlier
-- 0001 and never written to, and abandoned uploads are now found by their age
-- instead. The drop only runs if the table is empty, so it cannot lose data,
-- and 0001 no longer creates it.
--
-- Rollback: drop the added columns and the index; nothing read upload_jobs.

-- Sessions hold a hash, never the token itself: a copy of this table must not
-- be usable as a credential.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS token_hash TEXT;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS label TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_token ON sessions (token_hash);

-- The queries are dynamic because PL/pgSQL plans a static one even on the
-- branch it will not take, which fails on a database where 0001 never created
-- the table.
DO $$
DECLARE rows_present bigint;
BEGIN
    IF to_regclass('upload_jobs') IS NOT NULL THEN
        EXECUTE 'SELECT count(*) FROM upload_jobs' INTO rows_present;
        IF rows_present = 0 THEN
            EXECUTE 'DROP TABLE upload_jobs';
        END IF;
    END IF;
END $$;
