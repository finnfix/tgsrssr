use teloxide::prelude::*;
use teloxide::types::ParseMode;
use teloxide::utils::command::BotCommands;

use crate::db;
use crate::fetcher;
use crate::i18n::{self, Lang};
use crate::push::{self, ContentMode};
use crate::state::AppState;
use crate::try_or_reply;

/// Bot command definitions (unit variants; args parsed from raw text).
///
/// The `description` literals below are the **default (English)** command-menu
/// texts: they feed `Command::bot_commands()`, which main.rs registers with an
/// empty `language_code` (the fallback every client uses when it has no
/// dedicated list). Chinese menu texts live in `menu_desc_zh()` and are
/// registered under the two-letter code `zh` at startup (the Bot API rejects
/// subtag forms like `zh-hans`, so one `zh` list covers Simplified and
/// Traditional clients alike).
#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "RSS subscription bot commands")]
pub enum Command {
    #[command(description = "Help")]
    Start,
    #[command(description = "Subscriptions")]
    Set,
    #[command(description = "Subscription list")]
    List,
    #[command(description = "Add subscription")]
    Sub,
    #[command(description = "Test push")]
    Test,
    #[command(description = "Find message source")]
    Find,
    #[command(description = "Platform login")]
    Cookies,
    #[command(description = "Get logs")]
    Loget,
    #[command(description = "About")]
    About,
}

/// Chinese command-menu descriptions, keyed by command name (leading `/`
/// ignored — teloxide 0.17 derive emits both spellings across versions).
///
/// Simplified and Traditional clients share this one list (see
/// `MENU_LANGUAGES` in main.rs). Returns `None` for an unknown command so the
/// caller can fall back to the default English text rather than dropping it.
pub fn menu_desc_zh(cmd: &str) -> Option<&'static str> {
    Some(match cmd.trim_start_matches('/') {
        "start" => "帮助",
        "set" => "订阅管理",
        "list" => "订阅列表",
        "sub" => "添加订阅",
        "test" => "测试推送",
        "find" => "查询消息源",
        "cookies" => "平台验证",
        "loget" => "获取日志",
        "about" => "关于",
        _ => return None,
    })
}

/// Extract the argument portion after the command token, e.g. "/set foo" -> "foo".
fn cmd_args(msg: &Message) -> String {
    let text = msg.text().unwrap_or_default();
    match text.find(char::is_whitespace) {
        Some(idx) => text[idx..].trim().to_string(),
        None => String::new(),
    }
}

/// Link preview is always disabled for command responses. Responses may
/// contain hyperlinked feed URLs that don't need preview cards (commands run
/// in private chats as well as in groups/channels).
/// The implementation is consolidated in push::no_preview (re-exported here so existing call paths stay unchanged).
pub use crate::push::no_preview;

pub async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    state: AppState,
) -> ResponseResult<()> {
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);

    // Permission check: silently ignore non-whitelisted users.
    if !state.config.is_allowed(user_id) {
        return Ok(());
    }

    let args = cmd_args(&msg);
    let lang = user_lang(&state, user_id, Some(&msg)).await;

    // Admin-only commands: /cookies and /loget. Non-admins are silently
    // ignored (consistent with the non-whitelisted behaviour).
    if matches!(cmd, Command::Cookies | Command::Loget) && !state.config.is_admin(user_id) {
        return Ok(());
    }

    match cmd {
        Command::Start => handle_start(&bot, &msg, &state, user_id).await?,
        Command::Set => handle_set(&bot, &msg, &state, &args, user_id, lang).await?,
        Command::List => handle_list(&bot, &msg, &state, &args, lang).await?,
        Command::Sub => handle_sub(&bot, &msg, &state, &args, lang).await?,
        Command::Test => handle_test(&bot, &msg, &state, &args, lang).await?,
        Command::Find => handle_find(&bot, &msg, &state, &args, lang).await?,
        Command::Cookies => handle_cookies(&bot, &msg, &state, lang).await?,
        Command::Loget => handle_loget(&bot, &msg, &state, &args, lang).await?,
        Command::About => handle_start(&bot, &msg, &state, user_id).await?,
    }
    Ok(())
}

/// Resolve user language: DB preference first, then Telegram language_code, default En.
/// Delegates to the shared `callbacks::resolve_lang` primitive so callback and
/// command paths stay in sync (previously they diverged: command replies
/// honoured the client language_code while callback panels fell straight to
/// English for users who had not yet run /start).
pub async fn user_lang(state: &AppState, user_id: i64, msg: Option<&Message>) -> Lang {
    let code = msg.and_then(|m| m.from.as_ref()).and_then(|u| u.language_code.as_deref());
    crate::callbacks::resolve_lang(state, user_id, code).await
}

// ===== /start & /about =====

/// Version string injected by build.rs: `<semver>` for release builds
/// (Docker image, no .git present) or `<semver>+<git-short-hash>` for
/// local dev builds.  The semver part always comes from Cargo.toml.
pub const VERSION: &str = env!("APP_VERSION");

fn about_text(lang: Lang) -> String {
    i18n::t(lang, "start_text").replace("{}", VERSION)
}

async fn handle_start(bot: &Bot, msg: &Message, state: &AppState, user_id: i64) -> ResponseResult<()> {
    // Detect language from Telegram client and persist (first /start only sets it;
    // subsequent /start reads DB preference if already set).
    let code = msg.from.as_ref().and_then(|u| u.language_code.as_deref());
    let detected = Lang::from_language_code(code);
    let key = Lang::setting_key(user_id);
    let lang = match db::get_setting(&state.db, &key).await {
        Ok(Some(v)) => Lang::from_db_value(&v),
        Ok(None) => {
            // First time: persist detected language
            let _ = db::set_setting(&state.db, &key, detected.db_value()).await;
            detected
        }
        Err(e) => {
            // A transient read error must not overwrite the stored
            // preference; use the detected language for this reply only.
            tracing::warn!(user_id = user_id, error = %e, "language preference read failed, using detected value without persisting");
            detected
        }
    };
    bot.send_message(msg.chat.id, about_text(lang)).await?;
    Ok(())
}

// /about shares about_text() with /start; behavior is identical, so handle_start is reused directly.

// ===== /set =====

async fn handle_set(
    bot: &Bot,
    msg: &Message,
    state: &AppState,
    args: &str,
    user_id: i64,
    lang: Lang,
) -> ResponseResult<()> {
    let args = args.trim();
    if args.is_empty() {
        // Show channel list (paginated button panel, shared with the callback UI)
        crate::callbacks::render_channel_list(
            bot,
            crate::callbacks::PageTarget::Send(msg.chat.id),
            state,
            user_id,
            0,
            lang,
        ).await?;
    } else {
        // Direct channel page
        let chat_id = try_or_reply!(bot, msg.chat.id, lang, resolve_chat_id(state, args, user_id, lang).await);
        try_or_reply!(bot, msg.chat.id, lang, ensure_chat_registered(bot, state, &chat_id, user_id).await);
        crate::callbacks::render_channel_page(
            bot,
            crate::callbacks::PageTarget::Send(msg.chat.id),
            &chat_id,
            state,
            0,
            0,
            lang,
        ).await?;
    }
    Ok(())
}

// ===== /list =====

async fn handle_list(bot: &Bot, msg: &Message, state: &AppState, args: &str, lang: Lang) -> ResponseResult<()> {
    let target = match resolve_target_input(msg, args) {
        Some(t) => t,
        None => {
            bot.send_message(msg.chat.id, i18n::t(lang, "list_usage"))
                .parse_mode(ParseMode::Html)
                .await?;
            return Ok(());
        }
    };
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);
    let chat_id = try_or_reply!(bot, msg.chat.id, lang, resolve_chat_id(state, &target, user_id, lang).await);
    try_register_chat(bot, state, &chat_id, user_id).await;
    let subs = try_or_reply!(bot, msg.chat.id, lang, db::get_subscriptions(&state.db, &chat_id).await);

    let chat = try_or_reply!(bot, msg.chat.id, lang, db::get_chat(&state.db, &chat_id).await);
    let title = crate::callbacks::chat_display_title(chat.as_ref().and_then(|c| c.title.as_deref()), &chat_id);

    if subs.is_empty() {
        let text = i18n::t(lang, "list_empty")
            .replacen("{}", &push::format_chat_title_html(&title, &chat_id), 1)
            .replacen("{}", &push::format_chat_id_html(&chat_id), 1);
        bot.send_message(msg.chat.id, text)
            .parse_mode(ParseMode::Html)
            .await?;
        return Ok(());
    }

    let active_count = subs.iter().filter(|s| s.active == 1).count();
    let paused_count = subs.len() - active_count;

    let header = i18n::t(lang, "list_header")
        .replacen("{}", &push::format_chat_title_html(&title, &chat_id), 1)
        .replacen("{}", &push::format_chat_id_html(&chat_id), 1)
        .replacen("{}", &subs.len().to_string(), 1)
        .replacen("{}", &active_count.to_string(), 1)
        .replacen("{}", &paused_count.to_string(), 1);
    let paused_suffix = i18n::t(lang, "list_paused_suffix");
    let lines: Vec<String> = subs
        .iter()
        .enumerate()
        .map(|(i, sub)| {
            let name = sub.feed_title.as_deref().unwrap_or(&sub.feed_url);
            let status = if sub.active == 0 { paused_suffix } else { "" };
            format!(
                "{}. <a href=\"{}\">{}</a>{}\n",
                i + 1,
                push::escape_attr(&sub.feed_url),
                push::escape_html(name),
                status
            )
        })
        .collect();

    // Pack lines into chunks for sending: Telegram's message limit is 4096 (UTF-16 code units); leave headroom.
    // With many sources a single message would exceed the limit and fail to send (with no response visible to the user).
    const LIST_CHUNK_LIMIT: usize = 4000;
    let mut chunks: Vec<String> = Vec::new();
    let mut current = header;
    for line in lines {
        // Truncate when a single line itself exceeds the limit (very long title + URL),
        // so a send failure doesn't abort the remaining chunks.
        let line = if push::utf16_len(&line) > LIST_CHUNK_LIMIT {
            format!("{}…", push::take_utf16_prefix(&line, LIST_CHUNK_LIMIT - 1))
        } else {
            line
        };
        let candidate = format!("{}{}", current, line);
        if push::utf16_len(&candidate) <= LIST_CHUNK_LIMIT {
            current = candidate;
        } else {
            chunks.push(std::mem::take(&mut current));
            current = line;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }

    let mut first_msg: Option<teloxide::types::MessageId> = None;
    for (i, chunk) in chunks.iter().enumerate() {
        let mut req = bot.send_message(msg.chat.id, chunk)
            .parse_mode(ParseMode::Html)
            .link_preview_options(no_preview());
        // All chunks uniformly reply to the first message (flat structure, not chained).
        if let Some(rid) = first_msg {
            req = req.reply_parameters(teloxide::types::ReplyParameters::new(rid));
        }
        let m = req.await?;
        if first_msg.is_none() {
            first_msg = Some(m.id);
        }
        if i < chunks.len() - 1 {
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        }
    }
    Ok(())
}

// ===== /sub =====

async fn handle_sub(bot: &Bot, msg: &Message, state: &AppState, args: &str, lang: Lang) -> ResponseResult<()> {
    let parts: Vec<&str> = args.split_whitespace().collect();

    // Determine target channel + URL list (same determination as /test):
    // - First token looks like a channel reference (@name or negative ID) -> explicit channel + the rest are URLs
    // - Inside a group/channel without a channel argument -> the current chat is the target
    // - "/sub <url>..." in a group no longer mistakes the first URL for a channel name
    let (channel_input, urls): (String, &[&str]) = if !parts.is_empty() && looks_like_chat_ref(parts[0]) {
        (parts[0].to_string(), &parts[1..])
    } else if !parts.is_empty() && is_group_chat(msg) {
        (msg.chat.id.0.to_string(), &parts[..])
    } else {
        bot.send_message(
            msg.chat.id,
            i18n::t(lang, "sub_usage"),
        ).parse_mode(ParseMode::Html).await?;
        return Ok(());
    };

    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);
    let chat_id = try_or_reply!(bot, msg.chat.id, lang, resolve_chat_id(state, &channel_input, user_id, lang).await);
    try_or_reply!(bot, msg.chat.id, lang, ensure_chat_registered(bot, state, &chat_id, user_id).await);
    try_or_reply!(bot, msg.chat.id, lang, db::ensure_chat_config(&state.db, &chat_id).await);

    let mut results: Vec<String> = Vec::new();
    let mut added = 0usize;

    for url_raw in urls {
        let url_raw = url_raw.trim();
        if url_raw.is_empty() {
            continue;
        }
        // Normalize the platform source URL (unify host/path form) to prevent the same logical
        // source from being subscribed via different variants (e.g. t.me/foo vs t.me/s/foo),
        // which would cause duplicate pushes.
        let url_str = fetcher::normalize_feed_url(url_raw);
        let url_str = url_str.as_str();

        // Check whether it's already subscribed first: if active, skip the fetch and notify
        // directly, avoiding a pointless network request (for Weibo/X sources this also
        // helps with risk control).
        let existing = try_or_reply!(bot, msg.chat.id, lang, db::get_subscription_by_url(&state.db, &chat_id, url_str).await);
        if let Some(ref sub) = existing {
            if sub.active == 1 {
                let display_title = sub.feed_title.clone().unwrap_or_else(|| url_str.to_string());
                results.push(
                    i18n::t(lang, "sub_already")
                        .replacen("{}", &push::escape_attr(url_str), 1)
                        .replacen("{}", &push::escape_html(&display_title), 1)
                );
                continue;
            }
        }
        // Already disabled -> go through the fetch + activate flow below; the reply text is marked "re-enabled"
        let was_inactive = existing.is_some();

        // Platform sources (Weibo/X) require an admin-configured global cookie to work;
        // intercept when not configured and report "the admin has not opened this platform"
        // (fetching would inevitably fail and the error would be unintuitive).
        if let Some(p) = crate::platforms::platform_for_url(url_str) {
            if p.needs_cookie && state.get_cookie(p.id).await.is_none() {
                results.push(
                    i18n::t(lang, "sub_platform_closed")
                        .replacen("{}", p.label, 1)
                );
                continue;
            }
        }

        // Fetch feed first (rsstt-style validation): a feed we cannot fetch now is
        // NOT added. Otherwise the title falls back to the URL and, worse, the
        // "mark existing entries as read" step is skipped — the first successful
        // cycle would then dump the whole feed history into the channel.
        let fetch_result = fetcher::fetch_feed(state, url_str, None, None, None).await;
        let fr = match fetch_result {
            Ok(fr) => fr,
            Err(e) => {
                // The raw error already carries the source URL (e.g. "HTTP 403 ... for <url>"),
                // so the reply text doesn't repeat the URL; the error content is wrapped in a
                // blockquote on the next line.
                results.push(
                    i18n::t(lang, "sub_fetch_failed")
                        .replacen("{}", &push::escape_html(&format!("{:#}", e)), 1)
                );
                continue;
            }
        };
        let title = fr.feed_title.clone();

        // Don't use try_or_reply!: a mid-batch DB error must not abort the whole command
        // (otherwise all accumulated results are lost and remaining URLs are never processed).
        // Record the error in results and continue.
        // Insert the subscription and mark all current entries as sent in a
        // single transaction (only future entries are pushed). The previous
        // two-step flow rolled back by deleting the returned row id, which for
        // a re-enabled existing subscription was the user's original row —
        // a mark failure deleted it. The transaction makes the rollback
        // implicit and cannot touch pre-existing data.
        let ids: Vec<String> = fr.entries.iter().map(fetcher::entry_id).collect();
        if let Err(e) = db::add_subscription_with_sent(&state.db, &chat_id, url_str, title.as_deref(), &ids).await {
            results.push(
                i18n::t(lang, "sub_add_failed")
                    .replacen("{}", &push::escape_html(&format!("{:#}", e)), 1)
            );
            continue;
        }

        let display_title = title.unwrap_or_else(|| url_str.to_string());
        added += 1;
        let suffix = if was_inactive { i18n::t(lang, "sub_reenabled") } else { "" };
        results.push(format!(
            "• <a href=\"{}\">{}</a>{}",
            push::escape_attr(url_str),
            push::escape_html(&display_title),
            suffix
        ));
    }

    if results.is_empty() {
        bot.send_message(msg.chat.id, i18n::t(lang, "sub_no_valid")).await?;
        return Ok(());
    }

    let chat = try_or_reply!(bot, msg.chat.id, lang, db::get_chat(&state.db, &chat_id).await);
    let title = crate::callbacks::chat_display_title(chat.as_ref().and_then(|c| c.title.as_deref()), &chat_id);

    let header = if added > 0 {
        i18n::t(lang, "sub_added")
            .replacen("{}", &added.to_string(), 1)
            .replacen("{}", &push::escape_html(&title), 1)
    } else {
        i18n::t(lang, "sub_status")
            .replacen("{}", &push::escape_html(&title), 1)
    };
    let text = format!("{}\n{}", header, results.join("\n"));
    // Unified blockquote folding rule (fold at >110 chars), consistent with pushes/notifications.
    let text = push::normalize_blockquote_fold(&text);

    bot.send_message(msg.chat.id, &text)
        .parse_mode(ParseMode::Html)
        .link_preview_options(no_preview())
        .await?;
    Ok(())
}

// ===== /test =====

/// Report a /test outcome by editing the progress message in place. If the
/// edit fails (message too old / flood-limited), fall back to sending a brand
/// new message so the result is never permanently hidden behind the
/// "正在抓取…" progress text that would otherwise stay frozen.
async fn test_report(bot: &Bot, chat: teloxide::types::ChatId, progress_id: teloxide::types::MessageId, text: String) {
    if bot.edit_message_text(chat, progress_id, text.clone()).await.is_err() {
        // Degrade to a fresh message when the edit fails (message too old /
        // flood), so the result is never stuck invisible behind the
        // "fetching…" progress line. link_preview disabled to match the
        // progress message (pure-text result; avoids link-preview spam).
        bot.send_message(chat, text).link_preview_options(no_preview()).await.ok();
    }
}

async fn handle_test(bot: &Bot, msg: &Message, state: &AppState, args: &str, lang: Lang) -> ResponseResult<()> {
    let parts: Vec<&str> = args.split_whitespace().collect();

    // First token is a channel ref (@name or negative numeric ID) -> explicit target;
    // otherwise, inside a group/channel, the current chat is the target.
    let (channel_input, rest): (String, &[&str]) = if !parts.is_empty() && looks_like_chat_ref(parts[0]) {
        (parts[0].to_string(), &parts[1..])
    } else if !parts.is_empty() && is_group_chat(msg) {
        (msg.chat.id.0.to_string(), &parts[..])
    } else {
        bot.send_message(msg.chat.id, i18n::t(lang, "test_usage"))
            .parse_mode(ParseMode::Html)
            .await?;
        return Ok(());
    };

    if rest.is_empty() {
        bot.send_message(msg.chat.id, i18n::t(lang, "test_usage"))
            .parse_mode(ParseMode::Html)
            .await?;
        return Ok(());
    }

    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);
    let chat_id = try_or_reply!(bot, msg.chat.id, lang, resolve_chat_id(state, &channel_input, user_id, lang).await);
    try_or_reply!(bot, msg.chat.id, lang, ensure_chat_registered(bot, state, &chat_id, user_id).await);

    // Second token is the full feed URL.
    let feed_url = rest[0].to_string();
    if !feed_url.starts_with("http://") && !feed_url.starts_with("https://") {
        bot.send_message(msg.chat.id, i18n::t(lang, "test_bad_url")).await?;
        return Ok(());
    }
    // Normalize before any lookup: subscriptions are stored under their
    // normalized URL (normalize_feed_url collapses platform host/path/case
    // variants). Matching /test's raw input against the stored value would
    // miss an existing subscription (e.g. t.me/Foo/... vs t.me/foo), silently
    // falling back to default config instead of that source's override — so the
    // test result would not reflect what the real push actually does.
    let feed_url = fetcher::normalize_feed_url(&feed_url);
    // Platform sources (Weibo/X) require an admin-configured global cookie to work; intercept when not configured.
    if let Some(p) = crate::platforms::platform_for_url(&feed_url) {
        if p.needs_cookie && state.get_cookie(p.id).await.is_none() {
            bot.send_message(
                msg.chat.id,
                i18n::t(lang, "sub_platform_closed").replacen("{}", p.label, 1),
            ).await?;
            return Ok(());
        }
    }
    // No longer silently truncating with .min(9): an out-of-range index is reported explicitly
    // by the "this feed only has N entries" error below, avoiding requesting entry 50 but silently
    // sending entry 9.
    // #32: an index was provided but failed to parse -> report an explicit error instead of
    // silently falling back to entry 0.
    let entry_index: usize = match rest.get(1) {
        None => 0,
        Some(s) => match s.parse::<usize>() {
            Ok(n) => n,
            Err(_) => {
                bot.send_message(msg.chat.id, i18n::t(lang, "test_bad_index").replacen("{}", &push::escape_html(s), 1))
                    .parse_mode(ParseMode::Html)
                    .await?;
                return Ok(());
            }
        },
    };

    tracing::info!(
        user_id = user_id, channel = %channel_input, feed_url = %feed_url,
        entry_index = entry_index, target_chat = %chat_id,
        "/test: start"
    );

    // Send a progress message; we'll EDIT it with the final result.
    // #33: the progress message contains the source URL; disable link preview to avoid generating a preview card.
    let progress_msg = bot.send_message(msg.chat.id, i18n::t(lang, "test_fetching").replacen("{}", &feed_url, 1))
        .link_preview_options(no_preview())
        .await?;
    let progress_id = progress_msg.id;

    let fr = match fetcher::fetch_feed(state, &feed_url, None, None, None).await {
        Ok(fr) => fr,
        Err(e) => {
            tracing::warn!(feed_url = %feed_url, error = %e, "/test: fetch failed");
            test_report(bot, msg.chat.id, progress_id, i18n::t(lang, "test_fetch_failed").replace("{:#}", &format!("{:#}", e))).await;
            return Ok(());
        }
    };

    tracing::info!(
        feed_url = %feed_url, entries = fr.entries.len(),
        feed_title = ?fr.feed_title,
        "/test: fetch ok"
    );

    let entry = match fr.entries.get(entry_index) {
        Some(e) => e,
        None => {
            test_report(bot, msg.chat.id, progress_id, i18n::t(lang, "test_index_oob").replacen("{}", &fr.entries.len().to_string(), 1).replacen("{}", &entry_index.to_string(), 1)).await;
            return Ok(());
        }
    };

    // Entry detail log: id, title, content_type, body length, sidecar metadata.
    // Group C: poll/retweet-image/prerendered markers are now carried by EntryMeta; no longer checking in-body markers.
    let body_len = entry.content.as_ref().and_then(|c| c.body.as_ref()).map(|b| b.len()).unwrap_or(0);
    let content_type_str = entry.content.as_ref().map(|c| c.content_type.as_str()).unwrap_or("(none)");
    // Group C sidecar: look up structured metadata by entry_id (populated for platform sources, always empty for RSS feeds).
    // Consistent with the scheduler's main push path, use fetcher::entry_id (guards against drift in the fallback branch when entry.id is empty).
    let meta = fr.metas.get(&fetcher::entry_id(entry));
    let has_poll = meta.and_then(|m| m.poll.as_ref()).is_some();
    let rt_img_count = meta.map(|m| m.retweet_images.len()).unwrap_or(0);
    tracing::info!(
        entry_id = %entry.id, entry_title = ?entry.title,
        content_type = content_type_str, body_len = body_len,
        has_poll = has_poll, rt_img_count = rt_img_count,
        prerendered = meta.map(|m| m.prerendered_html).unwrap_or(false),
        "/test: entry selected"
    );

    // If this feed is subscribed in the channel, use its title + effective
    // config; otherwise fall back to the feed's own title and chat defaults.
    // /test does NOT require a subscription — any valid feed URL works.
    // Transient DB errors don't abort /test: degrade to no subscription (use the feed's own title).
    let subs = db::get_subscriptions(&state.db, &chat_id).await.unwrap_or_default();
    let sub = subs.iter().find(|s| s.feed_url == feed_url);
    let feed_title = sub
        .and_then(|s| s.feed_title.clone())
        .or_else(|| fr.feed_title.clone())
        .unwrap_or_default();
    let eff = db::get_effective_config(&state.db, &chat_id, &feed_url).await.ok();
    let caption_above_media = eff.as_ref().map(|e| e.caption_above_media).unwrap_or(false);
    let opts = match &eff {
        Some(e) => push::RenderOpts {
            mode: ContentMode::from_str(&e.content_mode),
            source: push::SourceDisplay::from_str(&e.source_display),
            link_preview: e.link_preview,
        },
        None => push::RenderOpts {
            mode: ContentMode::TitleLink,
            source: push::SourceDisplay::Source,
            link_preview: true,
        },
    };

    tracing::info!(
        mode = ?opts.mode, source = ?opts.source, link_preview = opts.link_preview,
        caption_above_media = caption_above_media, subscribed = sub.is_some(),
        "/test: effective config"
    );

    // chat_id is guaranteed parseable by resolve_chat_id; if it still fails to parse, report an
    // explicit error instead of silently falling back to the current private chat (otherwise the
    // test push would be sent to the private chat by mistake with no indication at all).
    let target_chat: i64 = match chat_id.parse() {
        Ok(id) => id,
        Err(_) => {
            test_report(bot, msg.chat.id, progress_id, i18n::t(lang, "test_bad_chat").replacen("{}", &chat_id, 1)).await;
            return Ok(());
        }
    };

    // Media pipeline for test pushes too (download + rich send + text fallback).
    // Title+link / title-only modes: never fetch media (consistent with scheduled pushes).
    let transport = crate::media::MediaTransport::new(&state.config);
    let prepared = if opts.mode.skip_media() {
        tracing::info!("/test: skip_media=true, no media fetch");
        crate::media::PreparedMedia::empty()
    } else {
        crate::media::prepare_entry_media(
            state,
            entry,
            &transport,
            meta,
            lang,
            opts.mode.single_media(),
        )
        .await
    };

    tracing::info!(
        media_items = prepared.items.len(), warnings = prepared.warnings.len(),
        oversized_videos = prepared.oversized_videos.len(),
        "/test: media prepared"
    );

    // Media-only mode but no media: inform the user explicitly
    if opts.mode == ContentMode::MediaOnly && prepared.items.is_empty() {
        test_report(bot, msg.chat.id, progress_id, i18n::t(lang, "test_media_only_empty").to_string()).await;
        return Ok(());
    }

    let author = fetcher::entry_author(entry);
    let result = if prepared.items.is_empty() {
        tracing::info!("/test: text-only path (no media items)");
        match push::format_entry_parts(entry, &feed_title, author.as_deref(), opts, meta) {
            Some((body, footer)) => {
                tracing::info!(body_len = body.len(), footer_len = footer.len(), "/test: formatted parts");
                match push::send_entry_msg(bot, target_chat, &body, &footer, opts.link_preview).await {
                    Ok(m) => {
                        tracing::info!(msg_id = m.id.0, "/test: text sent");
                        // Poll: consistent with the plain-text branch of send_entry_with_media
                        let mut poll_failed = false;
                        match push::extract_poll(meta) {
                            Some((question, options, is_multi)) => {
                                tracing::info!(
                                    question = %question, options = options.len(),
                                    is_multi = is_multi,
                                    "/test: poll extracted, sending"
                                );
                                if let Err(e) = push::send_poll(bot, target_chat, &question, &options, is_multi, Some(m.id)).await {
                                    tracing::warn!(chat_id = target_chat, error = %e, "/test: poll send FAILED");
                                    poll_failed = true;
                                } else {
                                    tracing::info!("/test: poll sent ok");
                                }
                            }
                            None => {
                                tracing::info!("/test: no poll in entry");
                            }
                        }
                        Ok(push::SendOutcome {
                            delivered_tg_link: push::tg_message_link(&m),
                            entry_link: fetcher::entry_link(entry),
                            media_failed: poll_failed,
                            ..Default::default()
                        })
                    }
                    Err(e) => Err(e),
                }
            }
            None => {
                tracing::info!("/test: format_entry_parts returned None (nothing to send)");
                Ok(push::SendOutcome::default())
            }
        }
    } else {
        tracing::info!(media_count = prepared.items.len(), "/test: rich media path");
        push::send_entry_with_media(
            bot,
            &state.db,
            target_chat,
            push::PushCtx {
                entry,
                feed_title: &feed_title,
                author: author.as_deref(),
                opts,
                caption_above_media,
                aspect_threshold: state.config.long_image_aspect_ratio,
                media: &prepared.items,
                meta,
            },
            &transport,
        )
        .await
    };

    match result {
        Ok(outcome) => {
            tracing::info!(
                delivered = ?outcome.delivered_tg_link, media_failed = outcome.media_failed,
                "/test: done ok"
            );
            let mut text = i18n::t(lang, "test_sent").to_string();
            if outcome.media_failed {
                text.push_str(i18n::t(lang, "test_media_failed"));
            }
            if !prepared.warnings.is_empty() {
                text.push_str(&i18n::t(lang, "test_warnings").replacen("{}", &prepared.warnings.join("\n"), 1));
            }
            test_report(bot, msg.chat.id, progress_id, text).await;
        }
        Err(e) => {
            tracing::warn!(chat_id = target_chat, feed_url = %feed_url, error = %e, "/test: push FAILED");
            test_report(bot, msg.chat.id, progress_id, i18n::t(lang, "test_push_failed").replace("{:#}", &format!("{:#}", e))).await;
        }
    }
    Ok(())
}

// ===== /cookies =====

/// Platform login-cookie management panel (global, admin-only since
/// v1.11.0). Selecting a platform prompts for the new cookie via reply; the
/// update applies immediately without restart and is persisted across
/// restarts.
async fn handle_cookies(bot: &Bot, msg: &Message, state: &AppState, lang: Lang) -> ResponseResult<()> {
    crate::callbacks::render_cookies_panel(
        bot,
        crate::callbacks::PageTarget::Send(msg.chat.id),
        state,
        lang,
    ).await?;
    Ok(())
}

// ===== /find =====

/// Look up the corresponding RSS feed entry from a TG message. Two input modes:
/// 1. `/find <TG message link>`: parse the t.me link
/// 2. Forward a channel message directly to the bot (no arguments): read chat + message_id from the forward origin
pub async fn handle_find(bot: &Bot, msg: &Message, state: &AppState, args: &str, lang: Lang) -> ResponseResult<()> {
    let link = args.trim();
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);

    // Target (chat_id, message_id): parse the link argument first; when there is no argument, try the forward origin.
    let target: Option<(String, i64)> = if !link.is_empty() {
        match parse_tg_message_link(link) {
            Some((chat_ref, message_id)) => {
                if let Ok(bare) = chat_ref.parse::<i64>() {
                    // t.me/c/<bare>/<msg>: real chat_id = -100<bare>
                    let cid = format!("-100{}", bare);
                    // Ownership check: only channels owned by the caller may be queried.
                    if !db::can_manage_chat(&state.db, user_id, &cid).await.unwrap_or(false) {
                        bot.send_message(msg.chat.id, i18n::t(lang, "find_not_yours")).await?;
                        return Ok(());
                    }
                    Some((cid, message_id))
                } else {
                    // t.me/<username>/<msg>: reverse-look-up chat_id in the DB.
                    // The reverse lookup is scoped to channels owned by the current user (pure ownership isolation).
                    let username = chat_ref.trim_start_matches('@');
                    let chats = match db::list_chats(&state.db, user_id).await {
                        Ok(c) => c,
                        Err(e) => {
                            bot.send_message(msg.chat.id, i18n::t(lang, "find_query_failed").replacen("{}", &format!("{:#}", e), 1)).await?;
                            return Ok(());
                        }
                    };
                    // Telegram usernames are case-insensitive; ignore case when comparing.
                    match chats.iter().find(|c| {
                        c.username
                            .as_deref()
                            .map(|u| u.eq_ignore_ascii_case(username))
                            .unwrap_or(false)
                    }) {
                        Some(c) => Some((c.chat_id.clone(), message_id)),
                        None => {
                            bot.send_message(msg.chat.id, i18n::t(lang, "find_not_registered")).await?;
                            return Ok(());
                        }
                    }
                }
            }
            None => {
                bot.send_message(msg.chat.id, i18n::t(lang, "find_bad_link"))
                    .parse_mode(ParseMode::Html)
                    .await?;
                return Ok(());
            }
        }
    } else {
        // No arguments: check whether this is a message forwarded from a channel/supergroup
        match msg.forward_origin() {
            Some(teloxide::types::MessageOrigin::Channel { chat, message_id, .. }) => {
                let cid = chat.id.0.to_string();
                // Ownership check: only channels owned by the caller may be queried.
                if !db::can_manage_chat(&state.db, user_id, &cid).await.unwrap_or(false) {
                    bot.send_message(msg.chat.id, i18n::t(lang, "find_not_yours")).await?;
                    return Ok(());
                }
                Some((cid, message_id.0 as i64))
            }
            Some(_) => {
                bot.send_message(msg.chat.id, i18n::t(lang, "find_not_channel")).await?;
                return Ok(());
            }
            None => {
                bot.send_message(msg.chat.id, i18n::t(lang, "find_usage"))
                    .parse_mode(ParseMode::Html)
                    .await?;
                return Ok(());
            }
        }
    };

    let (chat_id, message_id) = target.unwrap();

    match db::find_sent_message(&state.db, &chat_id, message_id).await {
        Ok(Some(row)) => {
            let mut text = i18n::t(lang, "find_source").replacen("{}", &push::escape_html(&row.feed_url), 1);
            if let Some(el) = &row.entry_link {
                text.push_str(&i18n::t(lang, "find_entry").replacen("{}", &push::escape_attr(el), 1).replacen("{}", &push::escape_html(el), 1));
            }
            text.push_str(&i18n::t(lang, "find_entry_id").replacen("{}", &push::escape_html(&row.entry_id), 1));
            text.push_str(&i18n::t(lang, "find_sent_at").replacen("{}", &push::escape_html(&row.created_at), 1));
            bot.send_message(msg.chat.id, text)
                .parse_mode(ParseMode::Html)
                .link_preview_options(no_preview())
                .await?;
        }
        Ok(None) => {
            // When forwarding an album (media group), Telegram splits N photos into N separate
            // messages sent to the bot, each triggering an implicit query; but the registry only
            // records the first (anchor) message of the group. So it's normal that non-anchor
            // members of a media group are not found — silently ignore them to avoid flooding
            // "not found" replies. Only applies to implicit forward mode (no link argument);
            // explicit /find link queries still report normally.
            let is_forwarded_group_member = link.is_empty() && msg.media_group_id().is_some();
            if !is_forwarded_group_member {
                bot.send_message(msg.chat.id, i18n::t(lang, "find_not_found")).await?;
            }
        }
        Err(e) => {
            // Distinguish DB errors from "not found": transient failures must not mislead the user.
            tracing::warn!(chat_id = %chat_id, message_id = message_id, error = %e, "find_sent_message query failed");
            bot.send_message(msg.chat.id, i18n::t(lang, "find_db_error")).await?;
        }
    }
    Ok(())
}

/// Parse a TG message link, returning (chat reference, message_id).
/// Chat reference: the t.me/c/ form returns the bare numeric id; the t.me/username/ form returns the username.
fn parse_tg_message_link(link: &str) -> Option<(String, i64)> {
    let u = url::Url::parse(link).ok()?;
    let host = u.host_str()?.to_lowercase();
    if host != "t.me" && host != "telegram.me" {
        return None;
    }
    let segs: Vec<&str> = u.path_segments()?.filter(|s| !s.is_empty()).collect();
    if segs.len() < 2 {
        return None;
    }
    // t.me/c/<id>/<msg> or t.me/<username>/<msg>
    let (chat_ref, msg_seg) = if segs[0] == "c" && segs.len() >= 3 {
        (segs[1].to_string(), segs[2])
    } else {
        (segs[0].to_string(), segs[1])
    };
    let message_id: i64 = msg_seg.parse().ok()?;
    Some((chat_ref, message_id))
}

// ===== Helpers =====

/// Retire a pending-op prompt by editing it into a plain status line.
/// The implementation is consolidated in push::retire_prompt (batch A3: cut the
/// callbacks→commands reverse dependency edge; re-exported here so existing call paths stay unchanged).
pub use crate::push::retire_prompt;

/// Load a subscription by id and verify the user still owns its channel.
/// Returns Some(sub) when owned, None when the sub is gone or the user lacks
/// permission (prompt already retired with a message). L8 fix: RenameSub and
/// SetFilterKeywords duplicated this fail-closed load+permission block.
async fn load_sub_owned(
    bot: &Bot,
    state: &AppState,
    user_id: i64,
    sub_id: i64,
    chat: teloxide::types::ChatId,
    prompt_msg_id: i32,
    lang: Lang,
) -> Option<db::SubRow> {
    // Ownership re-check: within the 180s window the bot may have been kicked from the channel
    // -> re-fetch to detect ownership change, preventing writes to someone else's subscription.
    // Fail-closed on DB query errors: don't falsely report "subscription not found"; prompt to retry later.
    let sub = match db::get_subscription_by_id(&state.db, sub_id).await {
        Ok(Some(s)) => Some(s),
        Ok(None) => None,
        Err(e) => {
            retire_prompt(bot, chat, prompt_msg_id, &i18n::t(lang, "load_sub_db_err").replace("{:#}", &format!("{:#}", e))).await;
            return None;
        }
    };
    let Some(sub) = sub else {
        retire_prompt(bot, chat, prompt_msg_id, i18n::t(lang, "load_sub_gone")).await;
        return None;
    };
    if !db::can_manage_chat(&state.db, user_id, &sub.chat_id).await.unwrap_or(false) {
        retire_prompt(bot, chat, prompt_msg_id, i18n::t(lang, "load_sub_no_perm")).await;
        return None;
    }
    Some(sub)
}

/// Consume a pending multi-step operation with the user's reply text.
pub async fn handle_pending_op(
    bot: &Bot,
    msg: &Message,
    state: &AppState,
    op: crate::state::PendingOp,
    text: &str,
) -> ResponseResult<()> {
    let text = text.trim();
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);
    let lang = user_lang(state, user_id, Some(msg)).await;

    // Audit trail for pending-op consumption: which flow, which user, whether the input is empty.
    let op_name = match &op {
        crate::state::PendingOp::RenameSub { .. } => "rename_sub",
        crate::state::PendingOp::SetCookie { .. } => "set_cookie",
        crate::state::PendingOp::SetFilterKeywords { .. } => "set_filter_keywords",
    };
    tracing::debug!(op = op_name, user_id = user_id, empty = text.is_empty(), "consuming pending op");

    match op {
        crate::state::PendingOp::RenameSub { sub_id, prompt_msg_id, panel_msg_id, ch_page, .. } => {
            if text.is_empty() || text.chars().count() > 100 {
                retire_prompt(bot, msg.chat.id, prompt_msg_id, i18n::t(lang, "rename_invalid")).await;
                return Ok(());
            }
            // Ownership re-check + fail-closed loading (handled uniformly by load_sub_owned).
            let Some(_sub) = load_sub_owned(bot, state, user_id, sub_id, msg.chat.id, prompt_msg_id, lang).await else {
                return Ok(());
            };
            match db::rename_subscription(&state.db, sub_id, text).await {
                Ok(_) => {
                    tracing::info!(sub_id = sub_id, user_id = user_id, "subscription renamed");
                    retire_prompt(bot, msg.chat.id, prompt_msg_id, &i18n::t(lang, "rename_ok").replacen("{}", text, 1)).await;
                    // Refresh the source's feed page panel in place (no new message sent); if the
                    // panel is no longer valid, deliver automatically falls back to sending a new one.
                    crate::callbacks::render_feed_page(
                        bot,
                        crate::callbacks::PageTarget::EditMessage(
                            msg.chat.id,
                            teloxide::types::MessageId(panel_msg_id),
                        ),
                        sub_id,
                        state,
                        ch_page,
                        lang,
                    ).await?;
                }
                Err(e) => {
                    retire_prompt(bot, msg.chat.id, prompt_msg_id, &i18n::t(lang, "rename_failed").replace("{:#}", &format!("{:#}", e))).await;
                }
            }
        }
        crate::state::PendingOp::SetCookie { platform, prompt_msg_id, panel_msg_id, .. } => {
            // Admin eligibility re-check: only admins may configure cookies (a reply from an admin
            // who was removed from ADMIN_USER_IDS after starting the flow is not consumed; the prompt is retired).
            if !state.config.is_admin(user_id) {
                retire_prompt(bot, msg.chat.id, prompt_msg_id, i18n::t(lang, "cancelled")).await;
                return Ok(());
            }
            let label = crate::platforms::find_platform(&platform)
                .map(|p| p.label)
                .unwrap_or(&platform);
            if text.is_empty() {
                retire_prompt(bot, msg.chat.id, prompt_msg_id, &i18n::t(lang, "cookie_empty").replacen("{}", label, 1)).await;
                return Ok(());
            }
            if text.chars().count() > 4096 {
                retire_prompt(bot, msg.chat.id, prompt_msg_id, i18n::t(lang, "cookie_too_long")).await;
                return Ok(());
            }
            match crate::platforms::update_cookie(state, &platform, text).await {
                Ok(_) => {
                    tracing::info!(platform = %platform, user_id = user_id, "cookie updated");
                    retire_prompt(bot, msg.chat.id, prompt_msg_id, &i18n::t(lang, "cookie_ok").replacen("{}", label, 1)).await;
                    // Refresh the /cookies panel in place: only the button
                    // status changes; the header stays untouched because the
                    // retired prompt message already carries the confirmation.
                    crate::callbacks::render_cookies_panel(
                        bot,
                        crate::callbacks::PageTarget::EditMessage(
                            msg.chat.id,
                            teloxide::types::MessageId(panel_msg_id),
                        ),
                        state,
                        lang,
                    ).await?;
                }
                Err(e) => {
                    retire_prompt(bot, msg.chat.id, prompt_msg_id, &i18n::t(lang, "cookie_save_failed").replace("{:#}", &format!("{:#}", e))).await;
                }
            }
        }
        crate::state::PendingOp::SetFilterKeywords { sub_id, prompt_msg_id, panel_msg_id, ch_page, .. } => {
            // Empty input (including non-text messages with text="") cancels, consistent with
            // RenameSub/SetCookie. Otherwise stickers/voice messages etc. would silently clear
            // the keywords and turn filtering off.
            if text.is_empty() {
                retire_prompt(bot, msg.chat.id, prompt_msg_id, i18n::t(lang, "fkw_empty")).await;
                return Ok(());
            }
            // Ownership re-check + fail-closed loading (handled uniformly by load_sub_owned).
            let Some(sub) = load_sub_owned(bot, state, user_id, sub_id, msg.chat.id, prompt_msg_id, lang).await else {
                return Ok(());
            };
            // "清空"/"clear" removes all keywords (turns filtering off); otherwise save as-is.
            let keywords = if text == "清空" || text.eq_ignore_ascii_case("clear") {
                String::new()
            } else {
                if text.chars().count() > 500 {
                    retire_prompt(bot, msg.chat.id, prompt_msg_id, i18n::t(lang, "fkw_too_long")).await;
                    return Ok(());
                }
                text.to_string()
            };
            match db::set_feed_filter_keywords(&state.db, &sub.chat_id, &sub.feed_url, &keywords).await {
                Ok(_) => {
                    tracing::info!(sub_id = sub_id, user_id = user_id, cleared = keywords.is_empty(), "filter keywords updated");
                    let status = if keywords.is_empty() {
                        i18n::t(lang, "fkw_cleared").to_string()
                    } else {
                        i18n::t(lang, "fkw_ok").replacen("{}", &keywords, 1)
                    };
                    retire_prompt(bot, msg.chat.id, prompt_msg_id, &status).await;
                    // Refresh the filter sub-page panel in place.
                    crate::callbacks::render_feed_filter(
                        bot,
                        crate::callbacks::PageTarget::EditMessage(
                            msg.chat.id,
                            teloxide::types::MessageId(panel_msg_id),
                        ),
                        sub_id,
                        state,
                        ch_page,
                        lang,
                    ).await?;
                }
                Err(e) => {
                    retire_prompt(bot, msg.chat.id, prompt_msg_id, &i18n::t(lang, "fkw_save_failed").replace("{:#}", &format!("{:#}", e))).await;
                }
            }
        }
    }
    Ok(())
}

/// True when the message comes from a group/supergroup/channel (not a private chat).
fn is_group_chat(msg: &Message) -> bool {
    msg.chat.is_group() || msg.chat.is_supergroup() || msg.chat.is_channel()
}

/// True when a token looks like a chat reference: @username or a negative numeric ID.
fn looks_like_chat_ref(s: &str) -> bool {
    s.starts_with('@') || (s.starts_with('-') && s[1..].parse::<i64>().is_ok())
}

/// Resolve the target chat argument: explicit arg wins; inside a group/channel an
/// empty arg means "the current chat"; in a private chat an empty arg is invalid.
fn resolve_target_input(msg: &Message, args: &str) -> Option<String> {
    let args = args.trim();
    if !args.is_empty() {
        return Some(args.to_string());
    }
    if is_group_chat(msg) {
        return Some(msg.chat.id.0.to_string());
    }
    None
}

/// Best-effort chat registration: try getChat, but never fail the command.
/// Used by /list so that orphan subscriptions (chat no longer registered /
/// bot removed) can still be listed and managed.
async fn try_register_chat(bot: &Bot, state: &AppState, chat_id: &str, user_id: i64) {
    if let Err(e) = ensure_chat_registered(bot, state, chat_id, user_id).await {
        tracing::debug!(chat_id = %chat_id, error = %e, "soft registration skipped");
    }
}

/// Resolve a chat identifier (numeric ID or @username) to a chat_id string.
/// Resolve a channel reference (numeric ID or @username) to a chat_id string.
/// Pure ownership isolation: only channels where added_by == user_id are accessible.
/// Unregistered numeric IDs are rejected (ownership comes from MyChatMember invitation).
async fn resolve_chat_id(state: &AppState, input: &str, user_id: i64, lang: Lang) -> anyhow::Result<String> {
    let input = input.trim();

    // Try as numeric ID: must be registered AND owned by this user.
    // A positive number is treated as a channel's bare ID (the t.me/c/ link form without the -100
    // prefix) and completed automatically; consistent with /find's bare-ID handling (previously
    // passing a positive number through would wrongly report "no permission").
    let normalized: String = if input.parse::<i64>().map(|v| v > 0).unwrap_or(false) {
        format!("-100{}", input)
    } else {
        input.to_string()
    };
    if normalized.parse::<i64>().is_ok() {
        let owned = db::can_manage_chat(&state.db, user_id, &normalized).await?;
        if !owned {
            anyhow::bail!("{}", i18n::t(lang, "resolve_no_perm"));
        }
        return Ok(normalized);
    }

    // Try as @username — look up in DB (scoped to the caller's owned chats)
    // Telegram usernames are case-insensitive; ignore case when comparing.
    let username = input.trim_start_matches('@');
    let chats = db::list_chats(&state.db, user_id).await?;
    for chat in &chats {
        if chat
            .username
            .as_deref()
            .map(|u| u.eq_ignore_ascii_case(username))
            .unwrap_or(false)
        {
            return Ok(chat.chat_id.clone());
        }
    }

    anyhow::bail!("{}", i18n::t(lang, "resolve_not_found").replacen("{}", input, 1))
}

/// Ensure a chat is registered in the DB.
///
/// Under the current permission model (since v0.9.0), channel ownership is determined by
/// `chats.added_by` recorded from MyChatMember events, and every call site goes through
/// resolve_chat_id first — unregistered channels are already rejected there
/// (can_manage_chat=false → bail). Therefore by the time we reach this function the channel
/// is necessarily already registered; the former "auto-register via getChat" branch (which
/// handled the case where the bot was already in the channel before startup) is unreachable
/// and has been removed (~30 lines of dead code). Only a defensive existence check remains
/// here: if a registration is genuinely missing due to an anomaly, leave a warn instead of
/// staying silent.
async fn ensure_chat_registered(
    _bot: &Bot,
    state: &AppState,
    chat_id: &str,
    _user_id: i64,
) -> anyhow::Result<()> {
    let existing = db::get_chat(&state.db, chat_id).await?;
    if existing.is_none() {
        tracing::warn!(chat_id = %chat_id, "chat not registered (unexpected under current ownership model)");
    }
    Ok(())
}

// ===== /loget =====

/// /loget <YYYY-MM-DD> — send the log file for the specified date.
/// Log files are rotated daily by tracing-appender: {LOG_DIR}/rss-bot.log.YYYY-MM-DD.
/// When the file does not exist (invalid date or already cleaned up), report that it doesn't exist.
async fn handle_loget(bot: &Bot, msg: &Message, state: &AppState, args: &str, lang: Lang) -> ResponseResult<()> {
    use teloxide::requests::Requester;
    use teloxide::types::InputFile;

    let date = args.trim();
    // Validate the date format YYYY-MM-DD
    if date.len() != 10 || !date.chars().enumerate().all(|(i, c)| {
        if i == 4 || i == 7 { c == '-' } else { c.is_ascii_digit() }
    }) {
        bot.send_message(msg.chat.id, i18n::t(lang, "loget_usage"))
            .parse_mode(ParseMode::Html)
            .await?;
        return Ok(());
    }

    let filename = format!("rss-bot.log.{}", date);
    let path = state.config.log_dir.join(&filename);

    if !path.exists() {
        bot.send_message(msg.chat.id, i18n::t(lang, "loget_not_exist").replacen("{}", date, 1))
            .await?;
        return Ok(());
    }

    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "loget: read failed");
            bot.send_message(msg.chat.id, i18n::t(lang, "loget_read_failed"))
                .await?;
            return Ok(());
        }
    };

    if bytes.is_empty() {
        bot.send_message(msg.chat.id, i18n::t(lang, "loget_empty").replacen("{}", date, 1))
            .await?;
        return Ok(());
    }

    let input = InputFile::memory(bytes).file_name(filename.clone());
    bot.send_document(msg.chat.id, input).await?;
    Ok(())
}
