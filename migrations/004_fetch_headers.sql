-- HTTP 条件请求协商头：源未更新时服务端返回 304，省去重复抓取与解析。
ALTER TABLE subscriptions ADD COLUMN http_etag TEXT;
ALTER TABLE subscriptions ADD COLUMN http_last_modified TEXT;
