-- 005: 自动停用标记，用于网络故障自愈。
-- auto_disabled=1 表示"系统因连续失败自动停用"，定时任务会在冷却期后自动重新启用；
-- 手动暂停（bot 内按钮）置 0，永远不会被自动恢复。
-- disabled_at 记录自动停用时间，冷却期从该时刻起算。
ALTER TABLE subscriptions ADD COLUMN auto_disabled INTEGER NOT NULL DEFAULT 0;
ALTER TABLE subscriptions ADD COLUMN disabled_at TEXT;
