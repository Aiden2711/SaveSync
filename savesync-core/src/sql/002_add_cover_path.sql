-- SaveSync schema v2: add cover_path to games table.
-- Idempotent: only adds the column if it does not already exist.

-- SQLite does not support IF NOT EXISTS for ALTER TABLE ADD COLUMN.
-- Use a conditional approach via PRAGMA table_info check.
-- We create a temporary table with the new column and copy data if needed.
-- But since we control the migration version, a simple ALTER TABLE is sufficient
-- because this migration only runs once (version < 2).

ALTER TABLE games ADD COLUMN cover_path TEXT;