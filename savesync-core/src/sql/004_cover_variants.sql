-- SaveSync schema v4: wide cover variant and the single featured banner.
--
-- `wide_cover_path` mirrors `cover_path` for the 920x430 ("capsule ancha")
-- variant, so a game can carry a portrait and a wide cover at the same time.
-- `featured` marks the one game whose wide art is shown as the library banner.
--
-- This migration only runs once (version < 4), so a plain ALTER TABLE is
-- sufficient, exactly like 002_add_cover_path.sql.

ALTER TABLE games ADD COLUMN wide_cover_path TEXT;
ALTER TABLE games ADD COLUMN featured INTEGER NOT NULL DEFAULT 0;

-- Only one game may be featured at a time. Enforcing it in the schema keeps the
-- invariant true even if a future caller forgets to clear the previous banner.
CREATE UNIQUE INDEX IF NOT EXISTS idx_games_single_featured
    ON games (featured)
    WHERE featured = 1;
