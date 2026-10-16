-- C 组 sidecar：outbox 增加 meta_json 列，持久化 EntryMeta 供重试时还原
-- 结构化元数据（投票/转发图/预渲染标记）。历史行 meta_json 为空串，
-- 重试时反序列化失败则视为无 meta（行为退化为纯文本，可接受）。
ALTER TABLE outbox ADD COLUMN meta_json TEXT NOT NULL DEFAULT '';
