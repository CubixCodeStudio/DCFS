-- DCFS: durable object locators
-- Version: 0005
--
-- Additive: relaxes three NOT NULLs and adds two nullable columns, so that a
-- webhook-backed object — which has no guild or channel of its own, and whose
-- nonce travels inside the ciphertext — can be recorded. Nothing is read,
-- rewritten or dropped, and applying it twice is a no-op.
--
-- Rollback: drop the two columns. Restoring the NOT NULLs needs the affected
-- rows filled in first.

ALTER TABLE stored_objects ALTER COLUMN guild_id DROP NOT NULL;
ALTER TABLE stored_objects ALTER COLUMN channel_id DROP NOT NULL;
ALTER TABLE stored_objects ALTER COLUMN nonce DROP NOT NULL;

-- The last CDN URL seen for the attachment. A cache, not the locator: Discord's
-- links expire, so a stale one is refreshed from the message it belongs to.
ALTER TABLE stored_objects ADD COLUMN IF NOT EXISTS url TEXT;
ALTER TABLE stored_objects ADD COLUMN IF NOT EXISTS backend TEXT;
