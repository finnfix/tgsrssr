-- file_id 缓存：同一媒体内容（按内容哈希）重复出现时，直接复用 Telegram
-- 返回的 file_id 发送，避免重复上传。created_at 用于按保留期清理。
CREATE TABLE IF NOT EXISTS file_id_cache (
    content_hash TEXT PRIMARY KEY,
    file_id      TEXT NOT NULL,
    created_at   TEXT NOT NULL DEFAULT (datetime('now'))
);

-- 已发送消息登记表：TG 消息链接 → RSS 源条目，供 /find 反查。
-- 行数上限由 SENT_MESSAGES_MAX_ROWS 控制（插入时裁剪最旧行）。
CREATE TABLE IF NOT EXISTS sent_messages (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_id     TEXT NOT NULL,
    message_id  INTEGER NOT NULL,
    feed_url    TEXT NOT NULL,
    entry_id    TEXT NOT NULL,
    entry_link  TEXT,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(chat_id, message_id)
);
CREATE INDEX IF NOT EXISTS idx_sent_messages_chat_msg ON sent_messages(chat_id, message_id);
