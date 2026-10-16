-- Outbox: persistent retry queue for failed sends.
-- Entries are retried with exponential backoff (capped at 2h intervals),
-- abandoned after 72h total elapsed time.
CREATE TABLE IF NOT EXISTS outbox (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_id TEXT NOT NULL,
    feed_url TEXT NOT NULL,
    entry_id TEXT NOT NULL,
    feed_title TEXT NOT NULL DEFAULT '',
    entry_json TEXT NOT NULL,
    media_json TEXT NOT NULL DEFAULT '[]',
    content_mode TEXT NOT NULL DEFAULT 'TitleSummary',
    source_display TEXT NOT NULL DEFAULT 'Source',
    link_preview INTEGER NOT NULL DEFAULT 1,
    caption_above_media INTEGER NOT NULL DEFAULT 0,
    author TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_retry_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    last_error TEXT,
    UNIQUE(chat_id, feed_url, entry_id)
);
