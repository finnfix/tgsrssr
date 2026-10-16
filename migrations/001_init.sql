-- 001_init.sql — initial schema

CREATE TABLE IF NOT EXISTS chats (
    chat_id      TEXT PRIMARY KEY,
    chat_type    TEXT NOT NULL,
    title        TEXT,
    username     TEXT,
    is_forum     INTEGER DEFAULT 0,
    added_by     TEXT,
    enabled      INTEGER DEFAULT 1,
    created_at   TEXT DEFAULT (datetime('now')),
    updated_at   TEXT DEFAULT (datetime('now')),
    last_seen_at TEXT
);

CREATE TABLE IF NOT EXISTS subscriptions (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_id           TEXT NOT NULL,
    feed_url          TEXT NOT NULL,
    feed_title        TEXT,
    title_overridden  INTEGER DEFAULT 0,
    active            INTEGER DEFAULT 1,
    error_count       INTEGER DEFAULT 0,
    last_fetch_at     TEXT,
    last_error        TEXT,
    created_at        TEXT DEFAULT (datetime('now')),
    UNIQUE(chat_id, feed_url)
);

CREATE TABLE IF NOT EXISTS chat_configs (
    chat_id             TEXT PRIMARY KEY,
    interval_minutes    INTEGER DEFAULT 10,
    content_mode        TEXT DEFAULT 'title_link',
    source_display      TEXT DEFAULT 'source',
    caption_above_media INTEGER DEFAULT 0,
    updated_at          TEXT DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS feed_configs (
    chat_id             TEXT NOT NULL,
    feed_url            TEXT NOT NULL,
    interval_minutes    INTEGER,
    content_mode        TEXT,
    source_display      TEXT,
    caption_above_media INTEGER,
    updated_at          TEXT DEFAULT (datetime('now')),
    PRIMARY KEY (chat_id, feed_url)
);

CREATE TABLE IF NOT EXISTS sent_entries (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_id   TEXT NOT NULL,
    feed_url  TEXT NOT NULL,
    entry_id  TEXT NOT NULL,
    sent_at   TEXT DEFAULT (datetime('now')),
    UNIQUE(chat_id, feed_url, entry_id)
);

CREATE INDEX IF NOT EXISTS idx_chats_type ON chats(chat_type);
CREATE INDEX IF NOT EXISTS idx_chats_added_by ON chats(added_by);
CREATE INDEX IF NOT EXISTS idx_subscriptions_chat ON subscriptions(chat_id);
CREATE INDEX IF NOT EXISTS idx_sent_entries_sent_at ON sent_entries(sent_at);
CREATE INDEX IF NOT EXISTS idx_sent_entries_lookup ON sent_entries(chat_id, feed_url, entry_id);
