-- 008: 通用键值设置表（运行时可变配置，如平台登录 cookie）
-- 键命名约定：cookie:<platform>（如 cookie:weibo）
CREATE TABLE IF NOT EXISTS settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
