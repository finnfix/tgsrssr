-- 002: 频道设置重构
-- 1) 新增 link_preview（链接预览开关，频道专属，1=开）
ALTER TABLE chat_configs ADD COLUMN link_preview INTEGER NOT NULL DEFAULT 1;

-- 2) 内容模式旧值映射到新五态：
--    title_summary/full -> title_content；link_only -> title_link
UPDATE chat_configs SET content_mode = 'title_content' WHERE content_mode IN ('title_summary', 'full');
UPDATE chat_configs SET content_mode = 'title_link'    WHERE content_mode = 'link_only';
UPDATE feed_configs SET content_mode = 'title_content' WHERE content_mode IN ('title_summary', 'full');
UPDATE feed_configs SET content_mode = 'title_link'    WHERE content_mode = 'link_only';

-- 3) 推送间隔默认值改为 30 分钟（旧默认 10 从未在 UI 出现，视为未修改的默认行）
UPDATE chat_configs SET interval_minutes = 30 WHERE interval_minutes = 10;
