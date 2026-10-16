-- 009: 微博订阅 URL 格式迁移
-- 旧格式 weibo://user/<uid> 不再支持，统一迁移为 https://weibo.com/u/<uid>
-- （原生抓取识别 https://weibo.com/u/<uid> 主页链接）
-- uid 取 weibo://user/ 之后、首个 ? 或 # 之前的部分

UPDATE subscriptions
SET feed_url = 'https://weibo.com/u/' ||
    CASE
        WHEN instr(substr(feed_url, 14), '?') > 0
            THEN substr(substr(feed_url, 14), 1, instr(substr(feed_url, 14), '?') - 1)
        WHEN instr(substr(feed_url, 14), '#') > 0
            THEN substr(substr(feed_url, 14), 1, instr(substr(feed_url, 14), '#') - 1)
        ELSE substr(feed_url, 14)
    END
WHERE feed_url LIKE 'weibo://user/%';

UPDATE feed_configs
SET feed_url = 'https://weibo.com/u/' ||
    CASE
        WHEN instr(substr(feed_url, 14), '?') > 0
            THEN substr(substr(feed_url, 14), 1, instr(substr(feed_url, 14), '?') - 1)
        WHEN instr(substr(feed_url, 14), '#') > 0
            THEN substr(substr(feed_url, 14), 1, instr(substr(feed_url, 14), '#') - 1)
        ELSE substr(feed_url, 14)
    END
WHERE feed_url LIKE 'weibo://user/%';

UPDATE sent_entries
SET feed_url = 'https://weibo.com/u/' ||
    CASE
        WHEN instr(substr(feed_url, 14), '?') > 0
            THEN substr(substr(feed_url, 14), 1, instr(substr(feed_url, 14), '?') - 1)
        WHEN instr(substr(feed_url, 14), '#') > 0
            THEN substr(substr(feed_url, 14), 1, instr(substr(feed_url, 14), '#') - 1)
        ELSE substr(feed_url, 14)
    END
WHERE feed_url LIKE 'weibo://user/%';

UPDATE sent_messages
SET feed_url = 'https://weibo.com/u/' ||
    CASE
        WHEN instr(substr(feed_url, 14), '?') > 0
            THEN substr(substr(feed_url, 14), 1, instr(substr(feed_url, 14), '?') - 1)
        WHEN instr(substr(feed_url, 14), '#') > 0
            THEN substr(substr(feed_url, 14), 1, instr(substr(feed_url, 14), '#') - 1)
        ELSE substr(feed_url, 14)
    END
WHERE feed_url LIKE 'weibo://user/%';
