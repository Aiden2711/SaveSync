-- SaveSync schema v5: normalized per-game metadata cache.
--
-- One row per game, replaced in place when the user fetches metadata again
-- (`game_id` is the primary key, so the newest successful fetch wins). The row
-- is pure enrichment: it never participates in snapshot, restore, or sync.
--
-- `source` is the metadata provider that produced the row and `source_id` its
-- provider-local identifier (a Steam app id, a ScreenScraper game id, ...).
-- Fields the provider does not know stay NULL or `'[]'`.
--
-- Lists are JSON text (`'["Action","Roguelike"]'`), matching how
-- `revisions.parent_ids`, `conflicts.revision_ids`, and
-- `restore_journal.deleted_paths` already store their vectors.
--
-- `ON DELETE CASCADE` mirrors every other game-owned table: deleting a game
-- leaves no orphaned metadata behind.

CREATE TABLE IF NOT EXISTS game_metadata (
    game_id           TEXT PRIMARY KEY NOT NULL REFERENCES games(id) ON DELETE CASCADE,
    source            TEXT NOT NULL,
    source_id         TEXT NOT NULL,
    name              TEXT NOT NULL,
    release_year      INTEGER,
    release_date      TEXT,
    genres            TEXT NOT NULL DEFAULT '[]',
    developers        TEXT NOT NULL DEFAULT '[]',
    publishers        TEXT NOT NULL DEFAULT '[]',
    description_short TEXT,
    description_long  TEXT,
    players           TEXT,
    age_rating        TEXT,
    franchise         TEXT,
    themes            TEXT NOT NULL DEFAULT '[]',
    fetched_at        TEXT NOT NULL
);

-- A provider id is only unique per source, and the lookup a future provider
-- needs ("which game has ScreenScraper id 1234?") is by source and id.
CREATE INDEX IF NOT EXISTS idx_game_metadata_source
    ON game_metadata (source, source_id);
