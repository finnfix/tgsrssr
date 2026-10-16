-- Content filtering (per-feed): master switch, mode (blacklist/whitelist),
-- comma-separated keywords matched against entry titles.
-- Filtering is active only when filter_enabled = 1 AND filter_keywords is non-empty.
ALTER TABLE feed_configs ADD COLUMN filter_enabled INTEGER NOT NULL DEFAULT 0;
ALTER TABLE feed_configs ADD COLUMN filter_mode TEXT NOT NULL DEFAULT 'blacklist';
ALTER TABLE feed_configs ADD COLUMN filter_keywords TEXT NOT NULL DEFAULT '';
