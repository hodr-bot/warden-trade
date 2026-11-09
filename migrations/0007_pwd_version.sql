-- Password version for session invalidation on rotation.
-- Every JWT carries `pv` (the user's pwd_ver at issue time); extractors
-- reject tokens whose version no longer matches, so a password change kills
-- all previously issued sessions (stolen-token survival capped at rotation).
ALTER TABLE users ADD COLUMN IF NOT EXISTS pwd_ver INTEGER NOT NULL DEFAULT 0;
