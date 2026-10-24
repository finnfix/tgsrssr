//! Internationalization module.
//!
//! Language detection: Telegram `language_code` field.
//! - `zh-hans` / `zh-hant` / `zh` → Chinese
//! - Everything else → English
//!
//! Storage: settings table key `lang:<user_id>` = "zh" | "en".
//! Switch: /set panel language button.

/// Supported languages.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Zh,
    En,
}

impl Lang {
    /// Parse from Telegram `language_code` (e.g. "zh-hans", "en", "ru").
    pub fn from_language_code(code: Option<&str>) -> Self {
        match code {
            Some(c) => {
                let lower = c.to_lowercase();
                if lower == "zh" || lower.starts_with("zh-") {
                    Lang::Zh
                } else {
                    Lang::En
                }
            }
            None => Lang::En,
        }
    }

    /// Parse from stored DB value.
    pub fn from_db_value(v: &str) -> Self {
        match v {
            "zh" => Lang::Zh,
            _ => Lang::En,
        }
    }

    /// DB storage value.
    pub fn db_value(self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }

    /// Settings key for a user.
    pub fn setting_key(user_id: i64) -> String {
        format!("lang:{user_id}")
    }
}

/// Translate a static string by key.
/// A missing Chinese key falls back to the English text; a missing English key
/// returns the "[?]" marker (caught by the completeness test, which extracts
/// every key from this source).
pub fn t(lang: Lang, key: &str) -> &'static str {
    match (lang, key) {
        // ===== Common =====
        (Lang::Zh, "close") => "关闭",
        (Lang::En, "close") => "Close",
        (Lang::Zh, "back") => "返回",
        (Lang::En, "back") => "Back",
        (Lang::Zh, "prev_page") => "上一页",
        (Lang::En, "prev_page") => "Prev",
        (Lang::Zh, "next_page") => "下一页",
        (Lang::En, "next_page") => "Next",
        (Lang::Zh, "on") => "开",
        (Lang::En, "on") => "On",
        (Lang::Zh, "off") => "关",
        (Lang::En, "off") => "Off",
        (Lang::Zh, "none") => "（无）",
        (Lang::En, "none") => "(none)",
        (Lang::Zh, "error_page") => "出错了：{}",
        (Lang::En, "error_page") => "Error: {}",
        (Lang::Zh, "op_failed_retry") => "操作失败，请稍后重试",
        (Lang::En, "op_failed_retry") => "Operation failed, please try again later",
        (Lang::Zh, "no_permission") => "无权限操作该频道",
        (Lang::En, "no_permission") => "No permission for this channel",
        (Lang::Zh, "sub_not_found") => "订阅不存在",
        (Lang::En, "sub_not_found") => "Subscription not found",
        (Lang::Zh, "op_failed") => "操作失败：{:#}",
        (Lang::En, "op_failed") => "Operation failed: {:#}",
        (Lang::Zh, "set_failed") => "设置失败：{:#}",
        (Lang::En, "set_failed") => "Setting failed: {:#}",
        (Lang::Zh, "invalid_interval") => "无效的间隔值（需 ≥1 分钟）",
        (Lang::En, "invalid_interval") => "Invalid interval (must be at least 1 minute)",

        // ===== /start & /about =====
        (Lang::Zh, "start_text") => "你好，我是 RSS 订阅 Bot，将我加入频道或群组后开始工作。\n版本：v{}",
        (Lang::En, "start_text") => "Hi! I'm an RSS subscription Bot. Add me to a channel or group to get started.\nVersion: v{}",

        // ===== Command descriptions =====
        // The command menu is localized outside this table: the client picks
        // its list by its reported language_code, so translations live in
        // `commands::menu_desc_zh()` (registered per-language at startup in
        // main.rs). The runtime UI strings below cover the panel / reply text,
        // selected by the stored `lang:<user_id>` setting.

        // ===== /list =====
        (Lang::Zh, "list_usage") => "用法：<code>/list &lt;频道&gt;</code>",
        (Lang::En, "list_usage") => "Usage: <code>/list &lt;channel&gt;</code>",
        (Lang::Zh, "list_empty") => "{} ({}) 暂无订阅。",
        (Lang::En, "list_empty") => "{} ({}) has no subscriptions.",
        (Lang::Zh, "list_header") => "{} ({})\n订阅源：{} 个（启用 {}，停用 {}）\n",
        (Lang::En, "list_header") => "{} ({})\nFeeds: {} (active {}, paused {})\n",
        (Lang::Zh, "list_paused_suffix") => " [已停用]",
        (Lang::En, "list_paused_suffix") => " [paused]",

        // ===== /sub =====
        (Lang::Zh, "sub_usage") => "用法：<code>/sub &lt;频道&gt; &lt;RSS链接&gt;</code>",
        (Lang::En, "sub_usage") => "Usage: <code>/sub &lt;channel&gt; &lt;RSS URL&gt;</code>",
        (Lang::Zh, "sub_already") => "• <a href=\"{}\">{}</a>（已在订阅中）",
        (Lang::En, "sub_already") => "• <a href=\"{}\">{}</a> (already subscribed)",
        (Lang::Zh, "sub_check_failed") => "• 检查订阅失败：\n<blockquote>{}</blockquote>",
        (Lang::En, "sub_check_failed") => "• Subscription check failed:\n<blockquote>{}</blockquote>",
        (Lang::Zh, "sub_fetch_failed") => "• 抓取失败：\n<blockquote>{}</blockquote>",
        (Lang::En, "sub_fetch_failed") => "• Fetch failed:\n<blockquote>{}</blockquote>",
        (Lang::Zh, "sub_add_failed") => "• 添加失败：\n<blockquote>{}</blockquote>",
        (Lang::En, "sub_add_failed") => "• Add failed:\n<blockquote>{}</blockquote>",
        (Lang::Zh, "sub_reenabled") => "（已重新启用）",
        (Lang::En, "sub_reenabled") => " (re-enabled)",
        (Lang::Zh, "sub_platform_closed") => "• {} 源暂不可用：管理员未配置该平台的登录凭证",
        (Lang::En, "sub_platform_closed") => "• {} source unavailable: the admin has not configured a login credential for this platform",
        (Lang::Zh, "sub_no_valid") => "没有有效的订阅链接。",
        (Lang::En, "sub_no_valid") => "No valid subscription URLs.",
        (Lang::Zh, "sub_added") => "已添加 {} 个订阅到 {}",
        (Lang::En, "sub_added") => "Added {} subscription(s) to {}",
        (Lang::Zh, "sub_status") => "订阅状态（{}）",
        (Lang::En, "sub_status") => "Subscription status ({})",

        // ===== /test =====
        (Lang::Zh, "test_usage") => "用法：<code>/test &lt;频道&gt; &lt;源地址&gt; [条目序号]</code>",
        (Lang::En, "test_usage") => "Usage: <code>/test &lt;channel&gt; &lt;feed URL&gt; [entry index]</code>",
        (Lang::Zh, "test_bad_url") => "源地址必须以 http:// 或 https:// 开头。",
        (Lang::En, "test_bad_url") => "Feed URL must start with http:// or https://.",
        (Lang::Zh, "test_bad_index") => "条目序号无效：「{}」，请输入从 0 开始的数字。",
        (Lang::En, "test_bad_index") => "Invalid entry index: \"{}\". Please enter a number starting from 0.",
        (Lang::Zh, "test_fetching") => "正在抓取 {} …",
        (Lang::En, "test_fetching") => "Fetching {} …",
        (Lang::Zh, "test_fetch_failed") => "抓取失败：{:#}",
        (Lang::En, "test_fetch_failed") => "Fetch failed: {:#}",
        (Lang::Zh, "test_index_oob") => "该源只有 {} 条内容，无法取第 {} 条。",
        (Lang::En, "test_index_oob") => "This feed only has {} entries, cannot get #{}.",
        (Lang::Zh, "test_bad_chat") => "频道 ID 无效：{}",
        (Lang::En, "test_bad_chat") => "Invalid channel ID: {}",
        (Lang::Zh, "test_media_only_empty") => "该条目无媒体，仅媒体模式下不推送。",
        (Lang::En, "test_media_only_empty") => "This entry has no media; nothing to push in media-only mode.",
        (Lang::Zh, "test_sent") => "测试推送已发送。",
        (Lang::En, "test_sent") => "Test push sent.",
        (Lang::Zh, "test_media_failed") => "\n媒体发送失败。",
        (Lang::En, "test_media_failed") => "\nMedia send failed.",
        (Lang::Zh, "test_warnings") => "\n提醒：\n{}",
        (Lang::En, "test_warnings") => "\nWarnings:\n{}",
        (Lang::Zh, "test_push_failed") => "推送失败：{:#}",
        (Lang::En, "test_push_failed") => "Push failed: {:#}",

        // ===== /find =====
        (Lang::Zh, "find_not_yours") => "未查到（该频道不属于你）。",
        (Lang::En, "find_not_yours") => "Not found (this channel doesn't belong to you).",
        (Lang::Zh, "find_query_failed") => "查询失败，请稍后重试：{:#}",
        (Lang::En, "find_query_failed") => "Query failed, please try again later: {:#}",
        (Lang::Zh, "find_not_registered") => "未查到（该频道不在本 Bot 管理范围内）。",
        (Lang::En, "find_not_registered") => "Not found (channel not managed by this bot).",
        (Lang::Zh, "find_bad_link") => "无法解析该链接，请提供形如 <code>https://t.me/c/…/…</code> 或 <code>https://t.me/用户名/…</code> 的消息链接。",
        (Lang::En, "find_bad_link") => "Cannot parse this link. Please provide a message link like <code>https://t.me/c/…/…</code> or <code>https://t.me/username/…</code>.",
        (Lang::Zh, "find_not_channel") => "转发的消息不是来自频道/超级群，无法查询。请直接提供消息链接。",
        (Lang::En, "find_not_channel") => "The forwarded message is not from a channel/supergroup. Please provide a message link directly.",
        (Lang::Zh, "find_usage") => "用法：<code>/find &lt;TG消息链接&gt;</code>，或直接把频道里的消息转发给我。",
        (Lang::En, "find_usage") => "Usage: <code>/find &lt;TG message link&gt;</code>, or forward a channel message to me.",
        (Lang::Zh, "find_source") => "源：<code>{}</code>",
        (Lang::En, "find_source") => "Source: <code>{}</code>",
        (Lang::Zh, "find_entry") => "\n条目：<a href=\"{}\">{}</a>",
        (Lang::En, "find_entry") => "\nEntry: <a href=\"{}\">{}</a>",
        (Lang::Zh, "find_entry_id") => "\n条目ID：<code>{}</code>",
        (Lang::En, "find_entry_id") => "\nEntry ID: <code>{}</code>",
        (Lang::Zh, "find_sent_at") => "\n发送时间：{} UTC",
        (Lang::En, "find_sent_at") => "\nSent at: {} UTC",
        (Lang::Zh, "find_not_found") => "未查到。",
        (Lang::En, "find_not_found") => "Not found.",
        (Lang::Zh, "find_db_error") => "查询失败，请稍后重试。",
        (Lang::En, "find_db_error") => "Query failed, please try again later.",

        // ===== /loget =====
        (Lang::Zh, "loget_usage") => "用法：<code>/loget 2026-10-08</code>",
        (Lang::En, "loget_usage") => "Usage: <code>/loget 2026-10-08</code>",
        (Lang::Zh, "loget_not_exist") => "{} 的日志不存在（可能已过期清理）",
        (Lang::En, "loget_not_exist") => "Log for {} does not exist (may have been cleaned up)",
        (Lang::Zh, "loget_read_failed") => "日志文件读取失败，请稍后重试",
        (Lang::En, "loget_read_failed") => "Failed to read log file, please try again later",
        (Lang::Zh, "loget_empty") => "{} 的日志文件为空",
        (Lang::En, "loget_empty") => "Log file for {} is empty",

        // ===== pending ops =====
        (Lang::Zh, "rename_prompt") => "请回复新的源名称（当前：{}）。\n点击源页的「关闭」按钮可放弃改名。",
        (Lang::En, "rename_prompt") => "Reply with the new feed name (current: {}).\nTap \"Close\" on the feed page to cancel.",
        (Lang::Zh, "rename_invalid") => "❌ 名称无效（需 1-100 字符），已取消改名。",
        (Lang::En, "rename_invalid") => "❌ Invalid name (1-100 chars required), rename cancelled.",
        (Lang::Zh, "rename_ok") => "✅ 已改名为「{}」",
        (Lang::En, "rename_ok") => "✅ Renamed to \"{}\"",
        (Lang::Zh, "rename_failed") => "❌ 改名失败：{:#}",
        (Lang::En, "rename_failed") => "❌ Rename failed: {:#}",
        (Lang::Zh, "cookie_empty") => "❌ 未收到内容，已取消更新{} cookie。",
        (Lang::En, "cookie_empty") => "❌ No content received, {} cookie update cancelled.",
        (Lang::Zh, "cookie_too_long") => "❌ 内容过长（超过 4096 字符），已取消。",
        (Lang::En, "cookie_too_long") => "❌ Content too long (>4096 chars), cancelled.",
        (Lang::Zh, "cookie_ok") => "✅ {} cookie 已更新，立即生效。",
        (Lang::En, "cookie_ok") => "✅ {} cookie updated, effective immediately.",
        (Lang::Zh, "cookie_save_failed") => "❌ cookie 保存失败：{:#}",
        (Lang::En, "cookie_save_failed") => "❌ Failed to save cookie: {:#}",
        (Lang::Zh, "fkw_empty") => "❌ 未收到内容，已取消设置关键词。",
        (Lang::En, "fkw_empty") => "❌ No content received, keyword setup cancelled.",
        (Lang::Zh, "fkw_too_long") => "❌ 关键词过长（超过 500 字符），已取消。",
        (Lang::En, "fkw_too_long") => "❌ Keywords too long (>500 chars), cancelled.",
        (Lang::Zh, "fkw_cleared") => "✅ 关键词已清空，过滤已关闭。",
        (Lang::En, "fkw_cleared") => "✅ Keywords cleared, filtering disabled.",
        (Lang::Zh, "fkw_ok") => "✅ 关键词已更新为「{}」",
        (Lang::En, "fkw_ok") => "✅ Keywords updated to \"{}\"",
        (Lang::Zh, "fkw_save_failed") => "❌ 关键词保存失败：{:#}",
        (Lang::En, "fkw_save_failed") => "❌ Failed to save keywords: {:#}",
        (Lang::Zh, "cancelled") => "已取消。",
        (Lang::En, "cancelled") => "Cancelled.",
        (Lang::Zh, "timeout_cancelled") => "⏳ 操作已超时，已取消。",
        (Lang::En, "timeout_cancelled") => "⏳ Operation timed out, cancelled.",
        (Lang::Zh, "closed") => "已关闭。",
        (Lang::En, "closed") => "Closed.",
        (Lang::Zh, "new_op_started") => "已取消（开始了新操作）。",
        (Lang::En, "new_op_started") => "Cancelled (a new operation started).",

        // ===== resolve_chat_id =====
        (Lang::Zh, "resolve_no_perm") => "无权限操作该频道（频道未注册或不属于你）",
        (Lang::En, "resolve_no_perm") => "No permission (channel not registered or doesn't belong to you)",
        (Lang::Zh, "resolve_not_found") => "未找到频道 {}，请确认 Bot 已加入该频道且由你添加",
        (Lang::En, "resolve_not_found") => "Channel {} not found. Make sure the bot is in the channel and you added it",

        // ===== load_sub_owned =====
        (Lang::Zh, "load_sub_db_err") => "❌ 查询订阅失败：{:#}，请稍后重试。",
        (Lang::En, "load_sub_db_err") => "❌ Failed to query subscription: {:#}, please try again later.",
        (Lang::Zh, "load_sub_gone") => "❌ 订阅不存在，已取消。",
        (Lang::En, "load_sub_gone") => "❌ Subscription not found, cancelled.",
        (Lang::Zh, "load_sub_no_perm") => "❌ 无权限操作该频道，已取消。",
        (Lang::En, "load_sub_no_perm") => "❌ No permission for this channel, cancelled.",

        // ===== Channel list / panel =====
        (Lang::Zh, "chlist_empty") => "暂无已注册的频道。",
        (Lang::En, "chlist_empty") => "No registered channels.",
        (Lang::Zh, "chlist_multi") => "请选择频道：\n第 {} / {} 页，共 {} 个频道",
        (Lang::En, "chlist_multi") => "Select a channel:\nPage {} / {}, {} channels total",
        (Lang::Zh, "chlist_single") => "请选择频道：\n共 {} 个频道",
        (Lang::En, "chlist_single") => "Select a channel:\n{} channels total",
        (Lang::Zh, "chpage_header") => "{} ({})\n订阅源：{} 个（启用 {}，停用 {}）",
        (Lang::En, "chpage_header") => "{} ({})\nFeeds: {} (active {}, paused {})",
        (Lang::Zh, "chpage_no_subs") => "\n（暂无订阅，使用 /sub 添加）",
        (Lang::En, "chpage_no_subs") => "\n(No subscriptions, use /sub to add)",
        (Lang::Zh, "chpage_multi") => "\n第 {} / {} 页，点击源进入源页：",
        (Lang::En, "chpage_multi") => "\nPage {} / {}, tap a feed to open:",
        (Lang::Zh, "chpage_single") => "\n点击源进入源页：",
        (Lang::En, "chpage_single") => "\nTap a feed to open:",
        (Lang::Zh, "chpage_settings") => "频道设置",
        (Lang::En, "chpage_settings") => "Channel settings",
        (Lang::Zh, "chpage_unsub") => "退订管理",
        (Lang::En, "chpage_unsub") => "Unsubscribe",
        (Lang::Zh, "chpage_resume_all") => "全部启用（{} 个已停用）",
        (Lang::En, "chpage_resume_all") => "Enable all ({} paused)",
        (Lang::Zh, "chpage_back") => "返回频道列表",
        (Lang::En, "chpage_back") => "Back to channel list",
        (Lang::Zh, "resumeall_ok") => "已启用 {} 个源",
        (Lang::En, "resumeall_ok") => "Enabled {} feed(s)",

        // ===== Feed page =====
        (Lang::Zh, "feed_active") => "推送中",
        (Lang::En, "feed_active") => "Active",
        (Lang::Zh, "feed_paused") => "已暂停",
        (Lang::En, "feed_paused") => "Paused",
        (Lang::Zh, "feed_header") => "<b><a href=\"{}\">{}</a></b>\n状态：{}\n",
        (Lang::En, "feed_header") => "<b><a href=\"{}\">{}</a></b>\nStatus: {}\n",
        (Lang::Zh, "feed_orig_name") => "原名称：<code>{}</code>\n",
        (Lang::En, "feed_orig_name") => "Original name: <code>{}</code>\n",
        (Lang::Zh, "feed_config") => "\n生效配置\n推送间隔：{} {}\n内容模式：{} {}\n来源显示：{} {}\n媒体在下：{} {}\n链接预览：{} {}",
        (Lang::En, "feed_config") => "\nEffective config\nInterval: {} {}\nContent mode: {} {}\nSource display: {} {}\nMedia below: {} {}\nLink preview: {} {}",
        (Lang::Zh, "feed_pause") => "暂停推送",
        (Lang::En, "feed_pause") => "Pause",
        (Lang::Zh, "feed_resume") => "恢复推送",
        (Lang::En, "feed_resume") => "Resume",
        (Lang::Zh, "feed_rename") => "改名",
        (Lang::En, "feed_rename") => "Rename",
        (Lang::Zh, "feed_interval") => "推送间隔：{}",
        (Lang::En, "feed_interval") => "Interval: {}",
        (Lang::Zh, "feed_mode") => "内容模式：{}",
        (Lang::En, "feed_mode") => "Content mode: {}",
        (Lang::Zh, "feed_source") => "来源显示：{}",
        (Lang::En, "feed_source") => "Source display: {}",
        (Lang::Zh, "feed_filter_active") => "内容筛选：{}·{}词",
        (Lang::En, "feed_filter_active") => "Filter: {}·{} words",
        (Lang::Zh, "feed_filter") => "内容筛选",
        (Lang::En, "feed_filter") => "Content filter",
        (Lang::Zh, "whitelist") => "白名单",
        (Lang::En, "whitelist") => "Whitelist",
        (Lang::Zh, "blacklist") => "黑名单",
        (Lang::En, "blacklist") => "Blacklist",

        // ===== Settings page =====
        (Lang::Zh, "settings_header") => "{}\n推送间隔：{}\n内容模式：{}\n来源显示：{}\n链接预览：{}",
        (Lang::En, "settings_header") => "{}\nInterval: {}\nContent mode: {}\nSource display: {}\nLink preview: {}",
        (Lang::Zh, "settings_interval") => "推送间隔",
        (Lang::En, "settings_interval") => "Interval",
        (Lang::Zh, "settings_mode") => "内容模式",
        (Lang::En, "settings_mode") => "Content mode",
        (Lang::Zh, "settings_source") => "来源显示",
        (Lang::En, "settings_source") => "Source display",
        // Link-preview label: shared by the channel settings page and the feed page.
        (Lang::Zh, "settings_lp") => "链接预览：{}",
        (Lang::En, "settings_lp") => "Link preview: {}",
        (Lang::Zh, "settings_caption") => "媒体在下：{}",
        (Lang::En, "settings_caption") => "Media below: {}",
        (Lang::Zh, "follow_channel") => "↩️ 🔗 跟随频道",
        (Lang::En, "follow_channel") => "↩️ 🔗 Follow channel",

        // ===== Option pages =====
        (Lang::Zh, "interval_title") => "推送间隔（当前 {}）",
        (Lang::En, "interval_title") => "Interval (current: {})",
        (Lang::Zh, "interval_title_feed") => "推送间隔（当前 {} {}）",
        (Lang::En, "interval_title_feed") => "Interval (current: {} {})",
        (Lang::Zh, "mode_title") => "内容模式（当前 {}）",
        (Lang::En, "mode_title") => "Content mode (current: {})",
        (Lang::Zh, "mode_title_feed") => "内容模式（当前 {} {}）",
        (Lang::En, "mode_title_feed") => "Content mode (current: {} {})",
        (Lang::Zh, "source_title") => "来源显示（当前 {}）",
        (Lang::En, "source_title") => "Source display (current: {})",
        (Lang::Zh, "source_title_feed") => "来源显示（当前 {} {}）",
        (Lang::En, "source_title_feed") => "Source display (current: {} {})",
        (Lang::Zh, "5min") => "5分钟",
        (Lang::En, "5min") => "5 min",
        (Lang::Zh, "10min") => "10分钟",
        (Lang::En, "10min") => "10 min",
        (Lang::Zh, "30min") => "30分钟",
        (Lang::En, "30min") => "30 min",
        (Lang::Zh, "3h") => "3小时",
        (Lang::En, "3h") => "3 hours",
        (Lang::Zh, "6h") => "6小时",
        (Lang::En, "6h") => "6 hours",
        (Lang::Zh, "12h") => "12小时",
        (Lang::En, "12h") => "12 hours",
        (Lang::Zh, "n_min") => "{}分钟",
        (Lang::En, "n_min") => "{} min",
        (Lang::Zh, "mode_title_link") => "标题+链接",
        (Lang::En, "mode_title_link") => "Title+Link",
        (Lang::Zh, "mode_title_summary") => "标题+摘要",
        (Lang::En, "mode_title_summary") => "Title+Summary",
        (Lang::Zh, "mode_title_content") => "标题+内容",
        (Lang::En, "mode_title_content") => "Title+Content",
        (Lang::Zh, "mode_title_only") => "仅标题",
        (Lang::En, "mode_title_only") => "Title only",
        (Lang::Zh, "mode_content_only") => "仅内容",
        (Lang::En, "mode_content_only") => "Content only",
        (Lang::Zh, "mode_media_only") => "仅媒体",
        (Lang::En, "mode_media_only") => "Media only",
        (Lang::Zh, "src_title") => "源标题",
        (Lang::En, "src_title") => "Feed title",
        (Lang::Zh, "src_author") => "仅作者",
        (Lang::En, "src_author") => "Author only",
        (Lang::Zh, "src_none") => "不展示",
        (Lang::En, "src_none") => "Hidden",

        // ===== Filter page =====
        (Lang::Zh, "filter_on") => "已开启",
        (Lang::En, "filter_on") => "Enabled",
        (Lang::Zh, "filter_on_no_keywords") => "已开启（未设置关键词，暂不生效）",
        (Lang::En, "filter_on_no_keywords") => "Enabled (no keywords yet; not applied)",
        (Lang::Zh, "filter_off") => "已关闭",
        (Lang::En, "filter_off") => "Disabled",
        (Lang::Zh, "filter_wl_desc") => "白名单（仅推送含关键词的条目）",
        (Lang::En, "filter_wl_desc") => "Whitelist (only push entries with keywords)",
        (Lang::Zh, "filter_bl_desc") => "黑名单（排除含关键词的条目）",
        (Lang::En, "filter_bl_desc") => "Blacklist (exclude entries with keywords)",
        (Lang::Zh, "filter_panel") => "<b>内容筛选</b>\n\n状态：{}\n方式：{}\n关键词：{}",
        (Lang::En, "filter_panel") => "<b>Content Filter</b>\n\nStatus: {}\nMode: {}\nKeywords: {}",
        (Lang::Zh, "filter_disable") => "关闭筛选",
        (Lang::En, "filter_disable") => "Disable filter",
        (Lang::Zh, "filter_enable") => "开启筛选",
        (Lang::En, "filter_enable") => "Enable filter",
        (Lang::Zh, "filter_to_bl") => "切换为黑名单",
        (Lang::En, "filter_to_bl") => "Switch to blacklist",
        (Lang::Zh, "filter_to_wl") => "切换为白名单",
        (Lang::En, "filter_to_wl") => "Switch to whitelist",
        (Lang::Zh, "filter_set_kw") => "设置关键词",
        (Lang::En, "filter_set_kw") => "Set keywords",
        (Lang::Zh, "fkw_prompt") => "当前关键词：{}\n回复新的关键词列表（逗号分隔），回复「清空」移除过滤。\n点击筛选页的「关闭」按钮可放弃。",
        (Lang::En, "fkw_prompt") => "Current keywords: {}\nReply with new keywords (comma-separated), or \"clear\" to remove.\nTap \"Close\" on the filter page to cancel.",

        // ===== Cookies panel =====
        (Lang::Zh, "ck_panel_title") => "平台登录态管理",
        (Lang::En, "ck_panel_title") => "Platform Cookie Management",
        (Lang::Zh, "ck_no_config") => "{}（无需配置）",
        (Lang::En, "ck_no_config") => "{} (no config needed)",
        (Lang::Zh, "ck_not_set") => "未配置",
        (Lang::En, "ck_not_set") => "Not set",
        (Lang::Zh, "ck_expired") => "已失效",
        (Lang::En, "ck_expired") => "Expired",
        (Lang::Zh, "ck_configured") => "已配置",
        (Lang::En, "ck_configured") => "Configured",
        (Lang::Zh, "ck_no_need") => "{} 通过公开网页预览抓取，无需登录配置",
        (Lang::En, "ck_no_need") => "{} uses public web preview, no login required",
        (Lang::Zh, "ck_private_only") => "请在与 bot 的私聊中管理 cookie（安全考虑）",
        (Lang::En, "ck_private_only") => "Please manage cookies in private chat with the bot (for security)",
        (Lang::Zh, "ck_prompt") => "请回复{}的登录 cookie。\n点击面板的「关闭」按钮可放弃。",
        (Lang::En, "ck_prompt") => "Reply with {} login cookie.\nTap \"Close\" to cancel.",
        (Lang::Zh, "ck_unknown") => "未知平台",
        (Lang::En, "ck_unknown") => "Unknown platform",

        // ===== Unsubscribe =====
        (Lang::Zh, "unsub_ok") => "<a href=\"{}\">{}</a> 退订成功",
        (Lang::En, "unsub_ok") => "<a href=\"{}\">{}</a> unsubscribed",
        (Lang::Zh, "unsub_panel_empty") => "该频道暂无订阅。",
        (Lang::En, "unsub_panel_empty") => "No subscriptions in this channel.",
        (Lang::Zh, "unsub_panel_multi") => "点击要退订的源：\n第 {} / {} 页，共 {} 个源",
        (Lang::En, "unsub_panel_multi") => "Tap a feed to unsubscribe:\nPage {} / {}, {} feeds total",
        (Lang::Zh, "unsub_panel_single") => "点击要退订的源：\n共 {} 个源",
        (Lang::En, "unsub_panel_single") => "Tap a feed to unsubscribe:\n{} feeds total",
        (Lang::Zh, "unsub_all_btn") => "退订全部（含备份）",
        (Lang::En, "unsub_all_btn") => "Unsubscribe all (with backup)",
        // Feed-page back button: reused by render_feed_page as well.
        (Lang::Zh, "unsub_back") => "返回频道页",
        (Lang::En, "unsub_back") => "Back to channel",
        (Lang::Zh, "unsuball_confirm") => "确认退订 {} ({}) 的全部 {} 个订阅源？\n退订前会生成 OPML 备份文件发送给你。\n此操作不可恢复。",
        (Lang::En, "unsuball_confirm") => "Unsubscribe all feeds from {} ({})? {} feeds in total.\nAn OPML backup will be sent to you first.\nThis cannot be undone.",
        (Lang::Zh, "unsuball_confirm_btn") => "确认退订全部",
        (Lang::En, "unsuball_confirm_btn") => "Confirm unsubscribe all",
        (Lang::Zh, "unsuball_processing") => "⏳ 正在退订全部订阅…",
        (Lang::En, "unsuball_processing") => "⏳ Unsubscribing all…",
        (Lang::Zh, "unsuball_dup") => "已在处理中",
        (Lang::En, "unsuball_dup") => "Already processing",
        (Lang::Zh, "unsuball_no_target") => "无法确定目标会话，已中止",
        (Lang::En, "unsuball_no_target") => "Cannot determine target chat, aborted",
        (Lang::Zh, "unsuball_caption") => "{} 的订阅备份（{} 个源）",
        (Lang::En, "unsuball_caption") => "Subscription backup for {} ({} feeds)",
        (Lang::Zh, "unsuball_backup_failed") => "备份文件发送失败，已中止退订",
        (Lang::En, "unsuball_backup_failed") => "Backup file send failed, unsubscribe aborted",
        (Lang::Zh, "unsuball_ok") => "已退订 {} ({}) 的全部 {} 个源，OPML 备份已发送。",
        (Lang::En, "unsuball_ok") => "Unsubscribed all feeds from {} ({}); {} feeds in total. OPML backup sent.",
        (Lang::Zh, "unsuball_view") => "查看频道页",
        (Lang::En, "unsuball_view") => "View channel page",
        (Lang::Zh, "unsuball_toast") => "已退订 {} 个源，备份已发送",
        (Lang::En, "unsuball_toast") => "Unsubscribed {} feeds, backup sent",
        (Lang::Zh, "unsub_no_subs") => "暂无订阅",
        (Lang::En, "unsub_no_subs") => "No subscriptions",

        // ===== Scheduler notifications =====
        (Lang::Zh, "notif_auto_disable") => "{}\n更新失败请及时检查，<code>/set {}</code>",
        (Lang::En, "notif_auto_disable") => "{}\nFetch failures detected, please check. <code>/set {}</code>",
        (Lang::Zh, "notif_platform_down") => "{}访问异常\n<blockquote>{}</blockquote>",
        (Lang::En, "notif_platform_down") => "{} access error\n<blockquote>{}</blockquote>",
        (Lang::Zh, "notif_platform_recovered") => "✅ {}恢复正常",
        (Lang::En, "notif_platform_recovered") => "✅ {} recovered",
        (Lang::Zh, "notif_media_failed") => "媒体发送失败：\n{}",
        (Lang::En, "notif_media_failed") => "Media send failed:\n{}",
        (Lang::Zh, "notif_push_failed") => "推送失败（{}）：\n[{}]\n<blockquote>{}</blockquote>",
        (Lang::En, "notif_push_failed") => "Push failed ({}):\n[{}]\n<blockquote>{}</blockquote>",
        (Lang::Zh, "notif_queued") => "已加入重试队列",
        (Lang::En, "notif_queued") => "queued for retry",
        (Lang::Zh, "notif_abandoned") => "已放弃",
        (Lang::En, "notif_abandoned") => "abandoned",
        (Lang::Zh, "notif_enqueue_failed") => "入队失败，条目已丢失",
        (Lang::En, "notif_enqueue_failed") => "Enqueue failed, entry lost",
        (Lang::Zh, "notif_serialize_failed") => "入队失败（序列化错误），条目已丢失",
        (Lang::En, "notif_serialize_failed") => "Enqueue failed (serialization error), entry lost",
        (Lang::Zh, "notif_timeout_72h") => "重试超时（72小时），条目已放弃。",
        (Lang::En, "notif_timeout_72h") => "Retry timeout (72h), entry abandoned.",
        (Lang::Zh, "notif_media_expired") => "仅媒体模式，但媒体文件已过期无法重试。",
        (Lang::En, "notif_media_expired") => "Media-only mode, but media files expired.",
        (Lang::Zh, "notif_outbox_media_expired") => "重试队列中的条目有媒体文件已过期（原有 {} 个，剩余 {} 个），将按剩余媒体继续发送；若全部过期则按文本发送。\n[{}]",
        (Lang::En, "notif_outbox_media_expired") => "Media files for a queued entry expired (original {}, remaining {}); sending with the remaining media, or as text if none remain.\n[{}]",
        (Lang::Zh, "notif_media_unreadable") => "仅媒体模式，但媒体记录为空或无法读取。",
        (Lang::En, "notif_media_unreadable") => "Media-only mode, but the media record is empty or unreadable.",
        (Lang::Zh, "notif_media_dl_fail") => "仅媒体模式，媒体下载连续失败 {} 次，条目已放弃。",
        (Lang::En, "notif_media_dl_fail") => "Media-only mode, {} consecutive download failures, entry abandoned.",
        (Lang::Zh, "notif_video_oversized") => "视频{}未推送，已发送纯文本消息：\n{}",
        (Lang::En, "notif_video_oversized") => "Video{} not pushed, text message sent:\n{}",
        (Lang::Zh, "notif_video_oversized_no_text") => "视频{}未推送：\n{}",
        (Lang::En, "notif_video_oversized_no_text") => "Video{} not pushed:\n{}",
        (Lang::Zh, "notif_video_size") => " 约 {} MB",
        (Lang::En, "notif_video_size") => " ~{} MB",
        (Lang::Zh, "notif_dl_failed") => "媒体下载失败{}：\n[{}]",
        (Lang::En, "notif_dl_failed") => "Media download failed{}:\n[{}]",
        (Lang::Zh, "notif_dl_note") => "（仅媒体模式，条目未推送，第 {}/{} 次重试）",
        (Lang::En, "notif_dl_note") => "(media-only, entry not pushed, retry {}/{})",
        (Lang::Zh, "label_message") => "消息",
        (Lang::En, "label_message") => "Message",
        (Lang::Zh, "label_entry") => "条目",
        (Lang::En, "label_entry") => "Entry",
        (Lang::Zh, "label_source") => "源",
        (Lang::En, "label_source") => "Source",

        // ===== Entry / platform fallbacks =====
        (Lang::Zh, "title_untitled") => "(无标题)",
        (Lang::En, "title_untitled") => "(Untitled)",
        (Lang::Zh, "weibo_image_label") => "查看图片",
        (Lang::En, "weibo_image_label") => "View image",
        (Lang::Zh, "weibo_deleted_user") => "已注销",
        (Lang::En, "weibo_deleted_user") => "Deleted user",
        (Lang::Zh, "weibo_repost") => "转发",
        (Lang::En, "weibo_repost") => "Repost",
        (Lang::Zh, "weibo_title_fallback") => "(微博)",
        (Lang::En, "weibo_title_fallback") => "(Weibo)",
        (Lang::Zh, "telegram_media_title") => "(媒体消息)",
        (Lang::En, "telegram_media_title") => "(Media message)",
        (Lang::Zh, "x_tweet_title") => "(推文)",
        (Lang::En, "x_tweet_title") => "(Tweet)",

        // ===== Media warnings =====
        (Lang::Zh, "media_image") => "图片",
        (Lang::En, "media_image") => "image",
        (Lang::Zh, "media_video") => "视频",
        (Lang::En, "media_video") => "video",
        (Lang::Zh, "media_file") => "文件",
        (Lang::En, "media_file") => "file",
        (Lang::Zh, "media_oversized_skip") => "媒体超限跳过（{}）",
        (Lang::En, "media_oversized_skip") => "Media skipped, size limit ({})",
        (Lang::Zh, "media_dl_empty") => "媒体下载失败（{}，空响应）",
        (Lang::En, "media_dl_empty") => "Media download failed ({}, empty response)",
        (Lang::Zh, "media_dl_failed") => "媒体下载失败（{}）",
        (Lang::En, "media_dl_failed") => "Media download failed ({})",
        (Lang::Zh, "notif_media_storage") => "媒体存储不可用，本轮已跳过源「{}」，恢复后会自动重试。请检查媒体目录与磁盘空间。",
        (Lang::En, "notif_media_storage") => "Media storage is unavailable; source \"{}\" was deferred this round and will retry automatically. Check the media directory and disk space.",

        // ===== Language switch =====
        (Lang::Zh, "lang_btn") => "语言：中文",
        (Lang::En, "lang_btn") => "Language: English",
        (Lang::Zh, "lang_switched") => "✅ 语言已切换为中文",
        (Lang::En, "lang_switched") => "✅ Language switched to English",

        // Missing ZH translation: fall back to English rather than a marker,
        // so a forgotten key degrades to the other language instead of "[?]".
        (Lang::Zh, _) => t(Lang::En, key),
        // Fallback: return a static marker (should never happen in practice)
        (Lang::En, _) => "[?]",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_code_detection() {
        assert_eq!(Lang::from_language_code(Some("zh-hans")), Lang::Zh);
        assert_eq!(Lang::from_language_code(Some("zh-hant")), Lang::Zh);
        assert_eq!(Lang::from_language_code(Some("zh")), Lang::Zh);
        assert_eq!(Lang::from_language_code(Some("ZH-HANS")), Lang::Zh);
        assert_eq!(Lang::from_language_code(Some("en")), Lang::En);
        assert_eq!(Lang::from_language_code(Some("ru")), Lang::En);
        assert_eq!(Lang::from_language_code(Some("ja")), Lang::En);
        assert_eq!(Lang::from_language_code(None), Lang::En);
    }

    #[test]
    fn db_value_roundtrip() {
        assert_eq!(Lang::from_db_value(Lang::Zh.db_value()), Lang::Zh);
        assert_eq!(Lang::from_db_value(Lang::En.db_value()), Lang::En);
        assert_eq!(Lang::from_db_value("unknown"), Lang::En);
    }

    #[test]
    fn setting_key_format() {
        assert_eq!(Lang::setting_key(12345), "lang:12345");
    }

    #[test]
    fn translation_completeness() {
        // Extract every (Lang::Zh, <quoted key>) pair from this source at
        // compile time and assert both languages resolve to a real
        // translation. The previous version checked five hardcoded keys and
        // compared against the key itself, which always passed once the
        // fallback was "[?]".
        let src = include_str!("i18n.rs");
        let re = regex::Regex::new(r#"\(Lang::Zh,\s*"([a-z0-9_]+)"\)"#).unwrap();
        let mut checked = 0;
        for caps in re.captures_iter(src) {
            let key = &caps[1];
            assert_ne!(t(Lang::Zh, key), "[?]", "Missing ZH translation for {key}");
            assert_ne!(t(Lang::En, key), "[?]", "Missing EN translation for {key}");
            checked += 1;
        }
        assert!(checked > 150, "only {checked} keys extracted");
    }
}
