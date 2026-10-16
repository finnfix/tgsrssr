-- 003: 源级链接预览覆盖
-- feed_configs 新增可空 link_preview（NULL=继承频道级 chat_configs.link_preview，
-- 0=本源关闭，1=本源开启）。与 caption_above_media 等其它源级覆盖列保持一致语义。
ALTER TABLE feed_configs ADD COLUMN link_preview INTEGER;
