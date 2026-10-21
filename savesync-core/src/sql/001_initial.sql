-- SaveSync schema v1: local-first safe core.
-- All revisions are immutable; mutable pointers live on `games`.

CREATE TABLE devices (    id            TEXT PRIMARY KEY NOT NULL,
    name          TEXT NOT NULL,
    platform      TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    last_seen_at  TEXT NOT NULL
);

CREATE TABLE games (
    id                       TEXT PRIMARY KEY NOT NULL,
    title                    TEXT NOT NULL,
    adapter_type             TEXT NOT NULL,
    -- Zero auto-enrollment: discovery inserts rows with enrolled = 0.
    enrolled                 INTEGER NOT NULL DEFAULT 0 CHECK (enrolled IN (0, 1)),
    created_at               TEXT NOT NULL,
    updated_at               TEXT NOT NULL,
    active_head_revision_id  TEXT
);

CREATE TABLE locations (
    id       TEXT PRIMARY KEY NOT NULL,
    game_id  TEXT NOT NULL REFERENCES games(id) ON DELETE CASCADE,
    path     TEXT NOT NULL,
    kind     TEXT NOT NULL CHECK (kind IN ('directory', 'file')),
    UNIQUE (game_id, path)
);

CREATE TABLE revisions (
    id             TEXT PRIMARY KEY NOT NULL,
    game_id        TEXT NOT NULL REFERENCES games(id) ON DELETE CASCADE,
    -- JSON array of parent revision ids; multiple parents mark an explicit
    -- conflict-resolution commit.
    parent_ids     TEXT NOT NULL,
    device_id      TEXT NOT NULL REFERENCES devices(id),
    created_at     TEXT NOT NULL,
    manifest_hash  TEXT NOT NULL,
    blob_hash      TEXT,
    message        TEXT
);

CREATE INDEX revisions_by_game ON revisions (game_id, created_at);

CREATE TABLE manifests (
    manifest_hash  TEXT NOT NULL,
    relative_path  TEXT NOT NULL,
    size           INTEGER NOT NULL CHECK (size >= 0),
    sha256         TEXT NOT NULL,
    mtime_ms       INTEGER,
    PRIMARY KEY (manifest_hash, relative_path)
);

-- Registry so empty manifests are distinguishable from unknown hashes.
CREATE TABLE manifest_sets (
    manifest_hash  TEXT PRIMARY KEY NOT NULL,
    created_at     TEXT NOT NULL
);

CREATE INDEX manifests_by_hash ON manifests (manifest_hash);

CREATE TABLE conflicts (
    id                      TEXT PRIMARY KEY NOT NULL,
    game_id                 TEXT NOT NULL REFERENCES games(id) ON DELETE CASCADE,
    revision_ids            TEXT NOT NULL,
    detected_at             TEXT NOT NULL,
    resolved_at             TEXT,
    resolution_revision_id  TEXT
);

CREATE INDEX conflicts_open ON conflicts (game_id, resolved_at);

CREATE TABLE restore_journal (
    id                TEXT PRIMARY KEY NOT NULL,
    game_id           TEXT NOT NULL REFERENCES games(id) ON DELETE CASCADE,
    revision_id       TEXT NOT NULL,
    target_root       TEXT NOT NULL,
    staging_path      TEXT NOT NULL,
    backup_path       TEXT NOT NULL,
    backup_manifest   TEXT NOT NULL,
    state             TEXT NOT NULL
                      CHECK (state IN ('prepared', 'applied', 'rolled_back', 'rollback_failed')),
    deleted_paths     TEXT NOT NULL DEFAULT '[]',
    error             TEXT,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL
);

CREATE INDEX restore_journal_by_game ON restore_journal (game_id, created_at);

-- At most one prepared (in-flight) restore per game.
CREATE UNIQUE INDEX restore_journal_one_in_flight
    ON restore_journal (game_id) WHERE state = 'prepared';

CREATE TABLE settings (
    key    TEXT PRIMARY KEY NOT NULL,
    value  TEXT NOT NULL
);
