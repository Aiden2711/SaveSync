-- SaveSync schema v3: add ludusavi_manifest_cache table for ETag-based caching.
-- Idempotent: only creates the table if it does not already exist.

CREATE TABLE IF NOT EXISTS ludusavi_manifest_cache (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    etag TEXT NOT NULL,
    yaml TEXT NOT NULL,
    fetched_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_ludusavi_manifest_cache_fetched_at
    ON ludusavi_manifest_cache (fetched_at);