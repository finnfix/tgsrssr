use teloxide::prelude::*;
use teloxide::types::{
    CallbackQuery, ChatId, ChatMemberStatus, InlineKeyboardButton,
    InlineKeyboardMarkup, ParseMode,
};
use tracing::{error, info};

use crate::db;
use crate::i18n::{self, Lang};
use crate::state::AppState;

pub mod settings;
pub mod unsub;

/// Items per page for the channel list and the per-channel source list.
const PAGE_SIZE: usize = 10;

/// Resolve a user's language for a callback flow: explicit DB preference
/// first, then the Telegram client `language_code`, then English. Shared by
/// every callback entry point so fallback behaviour matches the command path
/// (`commands::user_lang`) — previously five call sites fell straight to
/// `Lang::En` and ignored `language_code`, so a new user who had not run
/// /start yet saw English panels while command replies were in Chinese.
pub async fn resolve_lang(state: &AppState, user_id: i64, client_lang_code: Option<&str>) -> Lang {
    if let Ok(Some(v)) = db::get_setting(&state.db, &Lang::setting_key(user_id)).await {
        return Lang::from_db_value(&v);
    }
    Lang::from_language_code(client_lang_code)
}

/// Chat display title with fallback: when the title is absent, show the raw
/// chat id instead, so panels/headers never render an empty name. Exactly
/// matches the previous inline `…and_then(|c| c.title)…unwrap_or(chat_id)`
/// pattern (no extra trimming/empty-string handling) to keep behaviour
/// unchanged. (Previously six sites repeated it inline.)
pub(crate) fn chat_display_title(title: Option<&str>, chat_id: &str) -> String {
    title.unwrap_or(chat_id).to_string()
}

/// Build the pagination nav row (prev / indicator / next). M6 fix: three
/// call sites duplicated this exact logic; only the callback URL differs,
/// supplied via `mk_url`. Returns None when there is only one page.
fn nav_row(page: usize, total_pages: usize, lang: Lang, mk_url: impl Fn(usize) -> String) -> Option<Vec<InlineKeyboardButton>> {
    if total_pages <= 1 {
        return None;
    }
    let mut nav: Vec<InlineKeyboardButton> = Vec::new();
    if page > 0 {
        nav.push(InlineKeyboardButton::callback(i18n::t(lang, "prev_page"), mk_url(page - 1)));
    }
    nav.push(InlineKeyboardButton::callback(
        format!("{} / {}", page + 1, total_pages),
        "c|page",
    ));
    if page + 1 < total_pages {
        nav.push(InlineKeyboardButton::callback(i18n::t(lang, "next_page"), mk_url(page + 1)));
    }
    Some(nav)
}

/// Where a rendered page should go: a brand-new message, an in-place edit of
/// the message the callback button was pressed on, or an in-place edit of an
/// explicitly addressed message (used by flows outside a callback, e.g. the
/// rename reply refreshing the source page it came from).
pub enum PageTarget<'a> {
    Send(ChatId),
    Edit(&'a CallbackQuery),
    EditMessage(ChatId, teloxide::types::MessageId),
}

/// Link preview is always disabled for panel/UI messages. These are functional
/// UI (not content) rendered in the user's private chat with the bot, and a
/// preview card for the hyperlinked feed URLs is pure noise there.
/// Uses push::no_preview uniformly (batch A3: no longer forwarded through commands).
use crate::push::no_preview;
use settings::{fmt_content_mode, fmt_interval, fmt_source_display, load_chat_config_checked, render_feed_interval, render_feed_mode, render_feed_source, render_feed_filter_impl, render_settings_interval, render_settings_mode, render_settings_page, render_settings_source};
// render_feed_filter is pub (called from commands/mod.rs); re-export it.
pub use settings::render_feed_filter;

/// #15: Fallback keyboard for empty-state/error pages — at least provide a "Close" button,
/// so users are not left facing a dead buttonless message they cannot exit.
fn close_kb(lang: Lang) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(i18n::t(lang, "close"), "c|cancelop")]])
}

/// Deliver an HTML page to the target. For `Edit`/`EditMessage`, edits the
/// original message in place; falls back to sending a new message when the
/// original can no longer be edited (e.g. too old).
async fn deliver(
    bot: &Bot,
    target: &PageTarget<'_>,
    text: &str,
    kb: Option<InlineKeyboardMarkup>,
) -> ResponseResult<()> {
    match target {
        PageTarget::Send(chat) => {
            let mut req = bot
                .send_message(*chat, text)
                .parse_mode(ParseMode::Html)
                .link_preview_options(no_preview());
            if let Some(k) = kb {
                req = req.reply_markup(k);
            }
            req.await?;
        }
        PageTarget::Edit(q) => {
            if let Some(m) = &q.message {
                let chat = m.chat().id;
                let mid = m.id();
                bot.answer_callback_query(q.id.clone()).await.ok();
                edit_or_send(bot, chat, mid, text, kb).await?;
            } else {
                tracing::warn!(callback_id = %q.id, "deliver: q.message is None, cannot render page");
            }
        }
        PageTarget::EditMessage(chat, mid) => {
            edit_or_send(bot, *chat, *mid, text, kb).await?;
        }
    }
    Ok(())
}

/// Render a generic error page (escaped message + close button).
/// `err_display` is the already-formatted error text (callers may use
/// `{:#}` for anyhow error chains); escaping happens here.
async fn deliver_error_page(bot: &Bot, target: &PageTarget<'_>, lang: Lang, err_display: &str) -> ResponseResult<()> {
    deliver(
        bot,
        target,
        &i18n::t(lang, "error_page").replacen("{}", &crate::push::escape_html(err_display), 1),
        Some(close_kb(lang)),
    )
    .await
}

/// Edit a message in place; fall back to sending a new message when the
/// original can no longer be edited (too old / inaccessible).
async fn edit_or_send(
    bot: &Bot,
    chat: ChatId,
    mid: teloxide::types::MessageId,
    text: &str,
    kb: Option<InlineKeyboardMarkup>,
) -> ResponseResult<()> {
    let mut req = bot
        .edit_message_text(chat, mid, text)
        .parse_mode(ParseMode::Html)
        .link_preview_options(no_preview());
    if let Some(k) = kb.clone() {
        req = req.reply_markup(k);
    }
    match req.await {
        Ok(_) => {}
        Err(e) => {
            // Content identical — nothing to update. Prefer matching the typed
            // ApiError::MessageNotModified; keep the substring check as a
            // fallback in case the error surfaces without the typed variant, so
            // an edit-to-identical-content never falls through to sending a
            // duplicate panel message (spamming on every paginate/toggle).
            if matches!(
                e,
                teloxide::RequestError::Api(teloxide::ApiError::MessageNotModified)
            ) || e.to_string().contains("not modified")
            {
                return Ok(());
            }
            // Original message can't be edited (too old / inaccessible): send new.
            let mut s = bot
                .send_message(chat, text)
                .parse_mode(ParseMode::Html)
                .link_preview_options(no_preview());
            if let Some(k) = kb {
                s = s.reply_markup(k);
            }
            s.await?;
        }
    }
    Ok(())
}

/// Ownership guard level for callback ops (single-source definition, shared by the guard and
/// the dispatch, eliminating drift from maintaining two copies).
/// - Chat: parts[2] is the chat_id; verify ownership directly;
/// - Sub: parts[2] is the sub_id; reverse-look-up the subscription to get chat_id, then verify;
/// - None: no target channel involved (list page/cancel/cookies panel/silent pagination); no guard.
enum GuardTarget {
    Chat,
    Sub,
    None,
}

/// The single mapping from op → guard level. Any new op involving channel/source management
/// must be explicitly classified here, otherwise it is treated as None and skips the guard
/// (a fail-open hazard).
fn guard_target(op: &str) -> GuardTarget {
    match op {
        // chat level: parts[2] = chat_id
        "ch" | "cfg" | "cfgi" | "cfgm" | "cfgs" | "cfglp" | "resumeall"
        | "cfgiv" | "cfgmv" | "cfgsv" | "unsubpg" | "unsuballask" | "unsuball" => GuardTarget::Chat,
        // sub level: parts[2] = sub_id
        "src" | "srcpause" | "srcresume" | "srcrename" | "srcfip" | "srcfmp"
        | "srcfsp" | "srcflp" | "srcfilter" | "srcftoggle" | "srcfmode" | "srcfkw"
        | "srcfi" | "srcfm" | "srcfs" | "unsub" => GuardTarget::Sub,
        // no target channel involved
        _ => GuardTarget::None,
    }
}

/// Handle incoming callback queries.
pub async fn handle_callback(
    bot: Bot,
    q: CallbackQuery,
    state: AppState,
) -> ResponseResult<()> {
    // Hoist the callback ID to the top of the function: subsequent branches use qid uniformly
    // (cloning when ownership is needed), avoiding q.id.clone() scattered everywhere.
    // CallbackQueryId is not Copy; clone it to keep q intact.
    let qid = q.id.clone();
    let data = match &q.data {
        Some(d) => d.clone(),
        None => {
            // #34: even empty data needs an answer, to stop the client's loading spinner.
            bot.answer_callback_query(qid).await.ok();
            return Ok(());
        }
    };

    let user_id = q.from.id.0 as i64;
    // Silently ignore non-whitelisted users.
    if !state.config.is_allowed(user_id) {
        bot.answer_callback_query(qid).await.ok();
        return Ok(());
    }

    let lang = resolve_lang(&state, user_id, q.from.language_code.as_deref()).await;

    let parts: Vec<&str> = data.split('|').collect();
    if parts.is_empty() || parts[0] != "c" {
        // #34: callbacks with a prefix other than this bot's also need an answer, to stop the client's loading spinner.
        bot.answer_callback_query(qid).await.ok();
        return Ok(());
    }

    // Ownership guard (pure personal isolation, no admin bypass): callback data comes from the
    // client, and chat_id/sub_id can be enumerated and forged, so ownership must be verified
    // before any management operation runs.
    // For chat-level routes parts[2] is the chat_id; for sub-level routes parts[2] is the sub_id,
    // requiring a subscription reverse-lookup to obtain the chat_id. chlist/cancelop/ck/page etc.
    // don't involve a target channel and are skipped.
    //
    // Single-sourcing the guard tiers: the op → guard level is defined solely by guard_target()
    // (previously the two chat/sub arrays had to be manually kept in sync with the match branches
    // below; forgetting to add a guard for a new op would silently pass it through — fail-open).
    // When adding an op, just classify it into the right tier inside guard_target(); unclassified
    // ops are uniformly treated as having no target (no guard), so ops involving channel/source
    // management must be classified explicitly.
    let op = parts.get(1).copied().unwrap_or("");
    let guard_chat_id: Option<String> = match guard_target(op) {
        GuardTarget::Chat => parts.get(2).map(|s| s.to_string()),
        GuardTarget::Sub => {
            match parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                Some(sub_id) => {
                    match db::get_subscription_by_id(&state.db, sub_id).await {
                        Ok(Some(s)) => Some(s.chat_id),
                        // Subscription doesn't exist: let it through to each branch's "subscription
                        // not found" notice (no operation target, safe).
                        Ok(None) => None,
                        // DB query error: reject directly (fail-closed); the guard must not be skipped
                        // (the former .ok().flatten() treated transient DB errors as None, letting the guard be bypassed).
                        Err(e) => {
                            tracing::warn!(
                                user_id = user_id,
                                op = %parts.get(1).copied().unwrap_or("?"),
                                error = %e,
                                "ownership guard: subscription lookup failed, rejecting (fail-closed)"
                            );
                            bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed_retry")).await.ok();
                            return Ok(());
                        }
                    }
                }
                None => None,
            }
        }
        GuardTarget::None => None,
    };
    if let Some(gcid) = &guard_chat_id {
        let allowed = db::can_manage_chat(&state.db, user_id, gcid).await.unwrap_or(false);
        if !allowed {
            // callback data comes from the client and chat_id/sub_id can be enumerated and forged;
            // rejection events are a key signal for security auditing and must be logged.
            tracing::warn!(
                user_id = user_id,
                op = %parts.get(1).copied().unwrap_or("?"),
                target_chat = %gcid,
                "ownership guard rejected callback"
            );
            bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "no_permission")).await.ok();
            return Ok(());
        }
    }

    // Dispatch the op inside an async block so that a `?` in any branch
    // returns to this wrapper instead of exiting handle_callback directly.
    // Without the wrapper, a failing `render_*` (Flood / transient network
    // error) short-circuited past the fallback answer below, leaving the
    // client's loading spinner on forever with no feedback — contradicting
    // the "every path clears the spinner" guarantee.
    let res: ResponseResult<()> = async {
        match parts.get(1).copied() {
        Some("ch") => {
            // c|ch|<chat_id>[|<page>[|<chlist_page>]]
            if let Some(target) = parts.get(2) {
                let page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                render_channel_page(&bot, PageTarget::Edit(&q), target, &state, page, chlist_page, lang).await?;
            }
        }
        Some("chlist") => {
            // Back to channel list — edit in place. c|chlist[|<page>]
            let page = parts.get(2).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
            render_channel_list(&bot, PageTarget::Edit(&q), &state, user_id, page, lang).await?;
        }
        Some("lang") => {
            // Toggle language preference and re-render channel list.
            let key = Lang::setting_key(user_id);
            // Read the current preference strictly: a transient DB error must
            // abort with a retry toast rather than resolve to the fallback and
            // *write* a toggle computed from it (the "DB read error must not be
            // treated as a neutral value" contract — here it would silently
            // clamp an unset user to a stored language).
            let cur = match db::get_setting(&state.db, &key).await {
                Ok(Some(v)) => Lang::from_db_value(&v),
                Ok(None) => Lang::from_language_code(q.from.language_code.as_deref()),
                Err(e) => {
                    tracing::warn!(user_id = user_id, error = %e, "lang toggle: preference read failed, aborted");
                    bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed_retry")).await.ok();
                    return Ok(());
                }
            };
            let next = match cur {
                Lang::Zh => Lang::En,
                Lang::En => Lang::Zh,
            };
            // Write strictly too: only announce + re-render from `next` once it
            // is actually persisted, so the toast and the panel never disagree
            // (a swallowed write error used to render the old language while
            // the toast claimed the switch succeeded).
            if let Err(e) = db::set_setting(&state.db, &key, next.db_value()).await {
                tracing::warn!(user_id = user_id, error = %e, "lang toggle: preference write failed");
                bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed_retry")).await.ok();
                return Ok(());
            }
            let toast = i18n::t(next, "lang_switched");
            bot.answer_callback_query(q.id.clone()).text(toast).await.ok();
            render_channel_list(&bot, PageTarget::Edit(&q), &state, user_id, 0, next).await?;
        }
        Some("unsub") => {
            // c|unsub|<sub_id>[|<page>[|<ch_page>[|<chlist_page>]]]
            if let Some(sub_id_str) = parts.get(2) {
                if let Ok(sub_id) = sub_id_str.parse::<i64>() {
                    let page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                    let ch_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                    let chlist_page = parts.get(5).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                    unsub::handle_unsub_callback(&bot, &q, &state, sub_id, page, ch_page, chlist_page, lang).await?;
                }
            }
        }
        Some("unsubpg") => {
            // c|unsubpg|<chat_id>|<page>|<ch_page>[|<chlist_page>] — paginate the unsub panel
            if let Some(cid) = parts.get(2) {
                let page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let ch_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(5).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                unsub::render_unsub_panel(&bot, PageTarget::Edit(&q), cid, &state, page, ch_page, chlist_page, lang).await?;
            }
        }
        Some("unsuballask") => {
            // c|unsuballask|<chat_id>[|<page>[|<ch_page>[|<chlist_page>]]]
            // #36: the page parameter is passed through so the "Back" button on the
            // confirmation page can restore the unsub panel's original page.
            if let Some(cid) = parts.get(2) {
                let page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let ch_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(5).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                unsub::render_unsub_all_confirm(&bot, PageTarget::Edit(&q), cid, &state, page, ch_page, chlist_page, lang).await?;
            }
        }
        Some("unsuball") => {
            // c|unsuball|<chat_id>|<ch_page>|<page>[|<chlist_page>]
            // page is used to restore the confirmation page on failure (the confirmation page needs the unsub panel's page number).
            if let Some(cid) = parts.get(2) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(5).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                unsub::handle_unsub_all_callback(&bot, &q, &state, cid, ch_page, page, chlist_page, lang).await?;
            }
        }
        Some("cancelop") => {
            // Abort any in-flight multi-step flow for this user (e.g. rename
            // awaiting reply) and close the page silently. Already-applied
            // changes stay applied.
            if let Some(m) = q.message.as_ref() {
                let key = (m.chat().id.0, q.from.id.0 as i64);
                // If a pending op was active, retire its orphaned prompt
                // so it doesn't linger as a stale instruction.
                let removed_op = state.pending_ops.lock().await.remove(&key);
                bot.answer_callback_query(qid.clone()).await.ok();
                if let Some(op) = removed_op {
                    crate::push::retire_prompt(&bot, m.chat().id, op.prompt_msg_id(), i18n::t(lang, "cancelled")).await;
                }
                // The panel is not deleted; it is edited in place to "Closed." and its buttons are
                // cleared (avoid deleting messages whenever possible).
                // Fall back to deletion if the edit fails (message too old, etc.).
                if bot
                    .edit_message_text(m.chat().id, m.id(), i18n::t(lang, "closed"))
                    .reply_markup(InlineKeyboardMarkup::new(
                        Vec::<Vec<InlineKeyboardButton>>::new(),
                    ))
                    .await
                    .is_err()
                {
                    bot.delete_message(m.chat().id, m.id()).await.ok();
                }
            } else {
                bot.answer_callback_query(qid.clone()).await.ok();
            }
        }
        Some("cfg") => {
            // c|cfg|<chat_id> — open channel settings page
            if let Some(cid) = parts.get(2) {
                render_settings_page(&bot, PageTarget::Edit(&q), cid, &state, lang).await?;
            }
        }
        Some("cfgi") => {
            if let Some(cid) = parts.get(2) {
                render_settings_interval(&bot, PageTarget::Edit(&q), cid, &state, lang).await?;
            }
        }
        Some("cfgm") => {
            if let Some(cid) = parts.get(2) {
                render_settings_mode(&bot, PageTarget::Edit(&q), cid, &state, lang).await?;
            }
        }
        Some("cfgs") => {
            if let Some(cid) = parts.get(2) {
                render_settings_source(&bot, PageTarget::Edit(&q), cid, &state, lang).await?;
            }
        }
        Some("cfglp") => {
            // Toggle link preview (chat-level only)
            if let Some(cid) = parts.get(2) {
                // Strict baseline read: a transient DB error aborts the toggle
                // with a toast instead of flipping from a wrong default and
                // persisting a contradictory override.
                let cfg = match load_chat_config_checked(&state.db, cid).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(chat = %cid, error = %e, "cfglp: config read failed, toggle aborted");
                        bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed_retry")).await.ok();
                        return Ok(());
                    }
                };
                let next = if cfg.link_preview != 0 { "0" } else { "1" };
                match db::set_chat_config_field(&state.db, cid, "link_preview", next).await {
                    Ok(_) => {
                        render_settings_page(&bot, PageTarget::Edit(&q), cid, &state, lang).await?;
                    }
                    Err(e) => {
                        bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "set_failed").replace("{:#}", &format!("{:#}", e))).await.ok();
                    }
                }
            }
        }
        Some("resumeall") => {
            // c|resumeall|<chat_id>|<chlist_page>[|<ch_page>] — re-enable all paused sources under this channel in one click
            if let Some(cid) = parts.get(2) {
                let chlist_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let ch_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                match db::resume_all_subscriptions(&state.db, cid).await {
                    Ok(n) => {
                        info!(chat_id = %cid, count = n, "resume-all: subscriptions re-enabled");
                        bot.answer_callback_query(qid.clone())
                            .text(i18n::t(lang, "resumeall_ok").replacen("{}", &n.to_string(), 1))
                            .await
                            .ok();
                        // Keep the source-list page (ch_page) instead of resetting to 0, so the
                        // "enable all" action does not jump the user back to the first page.
                        render_channel_page(&bot, PageTarget::Edit(&q), cid, &state, ch_page, chlist_page, lang).await?;
                    }
                    Err(e) => {
                        bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed").replace("{:#}", &format!("{:#}", e))).await.ok();
                    }
                }
            }
        }
        Some("cfgiv") | Some("cfgmv") | Some("cfgsv") => {
            // c|cfgiv|<chat_id>|<minutes> / c|cfgmv|<chat_id>|<mode> / c|cfgsv|<chat_id>|<value>
            if let (Some(cid), Some(val)) = (parts.get(2), parts.get(3)) {
                let field = match parts[1] {
                    "cfgiv" => "interval_minutes",
                    "cfgmv" => "content_mode",
                    _ => "source_display",
                };
                // Range guard for the fetch interval: a forged/edge callback
                // with 0 or a negative value would make the due check
                // (now-last_fetch)*1440 >= interval always true, so the channel
                // is fetched on every 5-minute tick — easily tripping source
                // rate limits / IP bans. Clamp to a sane minimum (1 minute).
                if field == "interval_minutes" {
                    let ok = val.parse::<i64>().map(|n| n >= 1).unwrap_or(false);
                    if !ok {
                        bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "set_failed")).await.ok();
                        return Ok(());
                    }
                }
                match db::set_chat_config_field(&state.db, cid, field, val).await {
                    Ok(_) => {
                        info!(chat_id = %cid, field = field, value = %val, "chat config updated");
                        render_settings_page(&bot, PageTarget::Edit(&q), cid, &state, lang).await?;
                    }
                    Err(e) => {
                        bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "set_failed").replace("{:#}", &format!("{:#}", e))).await.ok();
                    }
                }
            }
        }
        Some("src") => {
            // c|src|<sub_id>[|<ch_page>[|<chlist_page>]] — feed (source) page.
            // ch_page is the source-list page on the channel page the user came
            // from; chlist_page is the channel-list page. Both are passed back
            // through the "back to channel" button so a full round trip keeps
            // the user's position.
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                render_feed_page_impl(&bot, PageTarget::Edit(&q), id, &state, ch_page, chlist_page, lang).await?;
            }
        }
        Some("srcpause") | Some("srcresume") => {
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let active = parts[1] == "srcresume";
                match db::set_subscription_active(&state.db, id, active).await {
                    Ok(_) => {
                        info!(sub_id = id, active = active, "subscription active toggled");
                        render_feed_page_impl(&bot, PageTarget::Edit(&q), id, &state, ch_page, chlist_page, lang).await?;
                    }
                    Err(e) => {
                        bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed").replace("{:#}", &format!("{:#}", e))).await.ok();
                    }
                }
            }
        }
        Some("srcrename") => {
            // Ask for a new name via plain text prompt; the reply is handled in commands.
            // NOTE: the callback data may carry a chlist_page (segment 5), but the
            // PendingOp variant in state.rs stores only ch_page; we cannot extend
            // it here (state.rs is off-limits to this change). The eventual
            // rename-reply refresh renders the feed page with chlist_page=0, so
            // "back to channel" after rename lands on page 0 of the channel list.
            // Feed-page state itself (ch_page) is preserved.
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                if let Some(sub) = fetch_sub_or_toast(&state, &bot, qid.clone(), id, lang).await {
                    let chat = q.message.as_ref().map(|m| m.chat().id);
                    // The feed page this button lives on; refreshed in place
                    // after a successful rename.
                    let panel_msg_id = q.message.as_ref().map(|m| m.id().0).unwrap_or(0);
                    if let Some(chat) = chat {
                        let user_id = q.from.id.0 as i64;
                        bot.answer_callback_query(qid.clone()).await.ok();
                        let prompt_text = i18n::t(lang, "rename_prompt")
                            .replacen("{}", sub.feed_title.as_deref().unwrap_or(&sub.feed_url), 1);
                        start_pending_op(&bot, &state, chat, user_id, prompt_text, false, lang, |prompt_msg_id| {
                            crate::state::PendingOp::RenameSub {
                                sub_id: id,
                                prompt_msg_id,
                                panel_msg_id,
                                ch_page,
                                created_at: std::time::Instant::now(),
                            }
                        }).await?;
                    }
                }
            }
        }
        // Second-level feed pages.
        Some("srcfip") => {
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                render_feed_interval(&bot, PageTarget::Edit(&q), id, &state, ch_page, chlist_page, lang).await?;
            }
        }
        Some("srcfmp") => {
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                render_feed_mode(&bot, PageTarget::Edit(&q), id, &state, ch_page, chlist_page, lang).await?;
            }
        }
        Some("srcfsp") => {
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                render_feed_source(&bot, PageTarget::Edit(&q), id, &state, ch_page, chlist_page, lang).await?;
            }
        }
        Some("srcflp") => {
            // Toggle feed-level link preview directly (mirrors channel cfglp).
            // Approach A: when the toggled value equals the channel default, clear the override (marker goes back to 🔗).
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                if let Some(sub) = fetch_sub_or_toast(&state, &bot, qid.clone(), id, lang).await {
                    // Read the current baseline strictly: a transient DB error
                    // must abort (with a toast, spinner cleared) rather than
                    // toggle from a wrong default and persist an override that
                    // contradicts the user's actual setting. A missing override
                    // row still yields the effective-config default normally.
                    let (cfg, chat_cfg) = match (
                        db::get_effective_config(&state.db, &sub.chat_id, &sub.feed_url).await,
                        load_chat_config_checked(&state.db, &sub.chat_id).await,
                    ) {
                        (Ok(cfg), Ok(chat_cfg)) => (cfg, chat_cfg),
                        (Err(e), _) | (_, Err(e)) => {
                            tracing::warn!(sub_id = id, error = %e, "srcflp: config read failed, toggle aborted");
                            bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed_retry")).await.ok();
                            return Ok(());
                        }
                    };
                    let next = if cfg.link_preview { "0" } else { "1" };
                    let channel_default = if chat_cfg.link_preview != 0 { "1" } else { "0" };
                    let value: Option<&str> = if next == channel_default { None } else { Some(next) };
                    match db::set_feed_config_field(&state.db, &sub.chat_id, &sub.feed_url, "link_preview", value).await {
                        Ok(_) => {
                            info!(sub_id = id, link_preview = next, "feed link preview toggled");
                            render_feed_page_impl(&bot, PageTarget::Edit(&q), id, &state, ch_page, chlist_page, lang).await?;
                        }
                        Err(e) => {
                            bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "set_failed").replace("{:#}", &format!("{:#}", e))).await.ok();
                        }
                    }
                }
            }
        }
        Some("srcfilter") => {
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                render_feed_filter_impl(&bot, PageTarget::Edit(&q), id, &state, ch_page, chlist_page, lang).await?;
            }
        }
        Some("srcftoggle") => {
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                if let Some(sub) = fetch_sub_or_toast(&state, &bot, qid.clone(), id, lang).await {
                    // Strict baseline read (same contract as cfglp/srcflp): a
                    // transient DB error must abort the toggle instead of
                    // flipping from the FilterConfig::default() and persisting
                    // a state that contradicts what the user actually set.
                    let fc = match db::get_feed_filter(&state.db, &sub.chat_id, &sub.feed_url).await {
                        Ok(fc) => fc,
                        Err(e) => {
                            tracing::warn!(sub_id = id, error = %e, "srcftoggle: filter read failed, toggle aborted");
                            bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed_retry")).await.ok();
                            return Ok(());
                        }
                    };
                    match db::set_feed_filter_enabled(&state.db, &sub.chat_id, &sub.feed_url, !fc.enabled).await {
                        Ok(_) => {
                            render_feed_filter_impl(&bot, PageTarget::Edit(&q), id, &state, ch_page, chlist_page, lang).await?;
                        }
                        Err(e) => {
                            bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "set_failed").replace("{:#}", &format!("{:#}", e))).await.ok();
                        }
                    }
                }
            }
        }
        Some("srcfmode") => {
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                if let Some(sub) = fetch_sub_or_toast(&state, &bot, qid.clone(), id, lang).await {
                    // Strict baseline read (see srcftoggle): abort on a DB
                    // error rather than persisting a mode flipped from a
                    // wrong default.
                    let fc = match db::get_feed_filter(&state.db, &sub.chat_id, &sub.feed_url).await {
                        Ok(fc) => fc,
                        Err(e) => {
                            tracing::warn!(sub_id = id, error = %e, "srcfmode: filter read failed, toggle aborted");
                            bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed_retry")).await.ok();
                            return Ok(());
                        }
                    };
                    let next = if fc.mode == "whitelist" { "blacklist" } else { "whitelist" };
                    match db::set_feed_filter_mode(&state.db, &sub.chat_id, &sub.feed_url, next).await {
                        Ok(_) => {
                            render_feed_filter_impl(&bot, PageTarget::Edit(&q), id, &state, ch_page, chlist_page, lang).await?;
                        }
                        Err(e) => {
                            bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "set_failed").replace("{:#}", &format!("{:#}", e))).await.ok();
                        }
                    }
                }
            }
        }
        Some("srcfkw") => {
            // Open keyword-input flow (plain text prompt; reply handled in commands)
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                if let Some(sub) = fetch_sub_or_toast(&state, &bot, qid.clone(), id, lang).await {
                    let chat = q.message.as_ref().map(|m| m.chat().id);
                    let panel_msg_id = q.message.as_ref().map(|m| m.id().0).unwrap_or(0);
                    if let Some(chat) = chat {
                        let user_id = q.from.id.0 as i64;
                        bot.answer_callback_query(qid.clone()).await.ok();
                        let fc = db::get_feed_filter(&state.db, &sub.chat_id, &sub.feed_url).await.unwrap_or_default();
                        let current = if fc.keywords.is_empty() {
                            i18n::t(lang, "none").to_string()
                        } else {
                            format!("<code>{}</code>", crate::push::escape_html(&fc.keywords))
                        };
                        let prompt_text = i18n::t(lang, "fkw_prompt").replacen("{}", &current, 1);
                        start_pending_op(&bot, &state, chat, user_id, prompt_text, true, lang, |prompt_msg_id| {
                            crate::state::PendingOp::SetFilterKeywords {
                                sub_id: id,
                                prompt_msg_id,
                                panel_msg_id,
                                ch_page,
                                created_at: std::time::Instant::now(),
                            }
                        }).await?;
                    }
                }
            }
        }
        Some("srcfi") | Some("srcfm") | Some("srcfs") => {
            // Feed-level overrides: c|srcfi|<sub_id>|<value>[|<ch_page>] etc.
            // A value of "inherit" clears the override (NULL → fall back to channel).
            // Approach A: when the selected value equals the channel default, the override is
            // cleared automatically (marker goes back to 🔗); an override record (marker ✏️) is
            // only written when the value truly differs from the channel.
            if let (Some(id), Some(val)) = (
                parts.get(2).and_then(|s| s.parse::<i64>().ok()),
                parts.get(3),
            ) {
                let ch_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(5).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                // Range guard mirroring the chat-level cfgiv path: a forged or
                // edge callback with a 0/negative interval makes the due check
                // always true, hammering the source every tick (rate-limit /
                // IP-ban risk). "inherit" clears the override and is allowed.
                if parts[1] == "srcfi" && *val != "inherit" {
                    let ok = val.parse::<i64>().map(|n| n >= 1).unwrap_or(false);
                    if !ok {
                        bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "set_failed")).await.ok();
                        return Ok(());
                    }
                }
                if let Some(sub) = fetch_sub_or_toast(&state, &bot, qid.clone(), id, lang).await {
                    let field = match parts[1] {
                        "srcfi" => "interval_minutes",
                        "srcfm" => "content_mode",
                        _ => "source_display",
                    };
                    // Compare with the channel default: clear the override when equal, only write an override when different.
                    // Strict baseline read (same contract as cfglp/srcflp): a
                    // transient DB error aborts with a toast instead of
                    // persisting an override computed from a wrong default.
                    let chat_cfg = match load_chat_config_checked(&state.db, &sub.chat_id).await {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!(sub_id = id, error = %e, "srcf*: config read failed, update aborted");
                            bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed_retry")).await.ok();
                            return Ok(());
                        }
                    };
                    let channel_default = match field {
                        "interval_minutes" => chat_cfg.interval_minutes.to_string(),
                        "content_mode" => chat_cfg.content_mode.clone(),
                        _ => chat_cfg.source_display.clone(),
                    };
                    let value: Option<&str> = if *val == "inherit" || *val == channel_default {
                        None
                    } else {
                        Some(*val)
                    };
                    match db::set_feed_config_field(&state.db, &sub.chat_id, &sub.feed_url, field, value).await {
                        Ok(_) => {
                            info!(sub_id = id, field = field, value = %val, "feed config updated");
                            // Return to the feed (source) page after a selection.
                            render_feed_page_impl(&bot, PageTarget::Edit(&q), id, &state, ch_page, chlist_page, lang).await?;
                        }
                        Err(e) => {
                            bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "set_failed").replace("{:#}", &format!("{:#}", e))).await.ok();
                        }
                    }
                }
            }
        }
        Some("ck") => {
            // c|ck|<platform> — pick a platform, then reply with the new cookie.
            // Admin-only: cookies are global (admin configures once, all users share).
            // Non-admins should never see this button (the panel is only visible to admins);
            // defensive re-check: callback data comes from the client and can be forged, so silently ignore.
            if !state.config.is_admin(user_id) {
                bot.answer_callback_query(qid.clone()).await.ok();
            } else if let Some(pid) = parts.get(2) {
                if let Some(p) = crate::platforms::find_platform(pid) {
                    if !p.needs_cookie {
                        // Cookie-less platform: just acknowledge.
                        bot.answer_callback_query(qid.clone())
                            .text(i18n::t(lang, "ck_no_need").replacen("{}", p.label, 1))
                            .await.ok();
                    } else {
                    let chat_obj = q.message.as_ref().map(|m| m.chat().clone());
                    // The cookie input flow is private-chat only: in a group chat the cookie
                    // plaintext in the user's reply would be visible, creating a leak window.
                    if !chat_obj.as_ref().map(|c| c.is_private()).unwrap_or(false) {
                        bot.answer_callback_query(qid.clone())
                            .text(i18n::t(lang, "ck_private_only"))
                            .await.ok();
                    } else {
                    let chat = chat_obj.map(|c| c.id);
                    // The /cookies panel this button lives on; refreshed in
                    // place after a successful update.
                    let panel_msg_id = q.message.as_ref().map(|m| m.id().0).unwrap_or(0);
                    if let Some(chat) = chat {
                        bot.answer_callback_query(qid.clone()).await.ok();
                        let prompt_text = i18n::t(lang, "ck_prompt").replacen("{}", p.label, 1);
                        let platform_id = p.id.to_string();
                        start_pending_op(&bot, &state, chat, user_id, prompt_text, false, lang, |prompt_msg_id| {
                            crate::state::PendingOp::SetCookie {
                                platform: platform_id,
                                prompt_msg_id,
                                panel_msg_id,
                                created_at: std::time::Instant::now(),
                            }
                        }).await?;
                    }
                    }
                    }
                } else {
                    tracing::warn!(platform = %pid, "ck: unknown platform id in callback");
                    bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "ck_unknown")).await.ok();
                }
            }
        }
        _ => {
            bot.answer_callback_query(qid.clone()).await.ok();
        }
        }
        Ok(())
    }
    .await;

    // Fallback answer: every exit path now reaches here — both silent Ok
    // branches (unknown platform, subscription not found, etc.) and Err
    // results from `render_*`/`start_pending_op` short-circuits — so the
    // client's loading indicator is always cleared. TG only honors the first
    // answer; repeated calls here are harmless (errors are swallowed by ok()).
    bot.answer_callback_query(qid).await.ok();

    if let Err(e) = &res {
        tracing::warn!(user_id = user_id, op = %op, error = %e, "callback dispatch failed (spinner cleared)");
    }
    res
}

/// Sub-level callback guard: look up the subscription by id; notify via toast on failure.
/// Returns Some(sub) on success; on Ok(None)/Err a toast answer has already been sent and the caller just skips.
async fn fetch_sub_or_toast(
    state: &AppState,
    bot: &Bot,
    qid: teloxide::types::CallbackQueryId,
    id: i64,
    lang: Lang,
) -> Option<db::SubRow> {
    match db::get_subscription_by_id(&state.db, id).await {
        Ok(Some(sub)) => Some(sub),
        Ok(None) => {
            // #37: the subscription has been deleted (stale button left by an old panel) → show a toast instead of staying silent.
            bot.answer_callback_query(qid).text(i18n::t(lang, "sub_not_found")).await.ok();
            None
        }
        Err(e) => {
            bot.answer_callback_query(qid).text(i18n::t(lang, "op_failed").replace("{:#}", &format!("{:#}", e))).await.ok();
            None
        }
    }
}

/// Send a prompt message and register a pending op; if a previous flow was
/// still pending for this (chat, user), retire its orphaned prompt.
/// `html`: whether the prompt text uses HTML parse mode.
/// `make_op`: builds the PendingOp variant from the prompt message id.
#[allow(clippy::too_many_arguments)]
async fn start_pending_op(
    bot: &Bot,
    state: &AppState,
    chat: ChatId,
    user_id: i64,
    prompt_text: String,
    html: bool,
    lang: Lang,
    make_op: impl FnOnce(i32) -> crate::state::PendingOp,
) -> ResponseResult<()> {
    let mut req = bot.send_message(chat, prompt_text);
    if html {
        req = req.parse_mode(ParseMode::Html);
    }
    // Hold the pending_ops lock across the send so "send prompt + register op"
    // is atomic. Otherwise a user who replies extremely fast (between the send
    // completing and the map insert) would have their reply treated as an
    // ordinary message, and the NEXT unrelated message would then be wrongly
    // consumed as the pending op (rename/cookie). A concurrent reply now either
    // runs before we acquire the lock (prompt not yet sent — correctly handled
    // as a normal message) or after the insert (correctly consumed).
    //
    // Cross-user impact assessment: teloxide 0.17's Dispatcher uses the default
    // distribution function `|upd| upd.chat().map(|c| c.id)`, and
    // `Update::chat()` for `CallbackQuery` returns `q.message.chat()`. Same
    // private chat → same worker queue → the user's callback and the user's
    // reply are always handled sequentially by a single spawned task. So the
    // race this lock protects against is already covered by same-chat
    // serialization; the global lock's cross-await hold only ever briefly
    // delays OTHER chats' handlers by one API roundtrip (send_message), never
    // creates a correctness hazard. Acceptable tradeoff; documented rather
    // than restructured (sharding the lock is out of scope: `pending_ops`
    // lives in state.rs, off-limits to this change).
    let mut ops = state.pending_ops.lock().await;
    let prompt = req.await?;
    let old = ops.insert((chat.0, user_id), make_op(prompt.id.0));
    drop(ops);
    if let Some(old) = old {
        crate::push::retire_prompt(bot, chat, old.prompt_msg_id(), i18n::t(lang, "new_op_started")).await;
    }
    Ok(())
}

/// Render the /cookies panel: title + one button per platform showing
/// cookie status (not configured / configured / expired). Global (admin-configured).
/// The header text is always the plain title — after a cookie update the
/// confirmation is carried by the retired prompt message, so the panel
/// itself only needs its button statuses refreshed.
pub async fn render_cookies_panel(
    bot: &Bot,
    target: PageTarget<'_>,
    state: &AppState,
    user_id: i64,
) -> ResponseResult<()> {
    let lang = resolve_lang(state, user_id, None).await;
    let text = String::from(i18n::t(lang, "ck_panel_title"));

    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();

    for p in crate::platforms::PLATFORMS {
        if !p.needs_cookie {
            // Cookie-less platforms (e.g. Telegram web preview): listed
            // read-only, no configuration flow.
            buttons.push(vec![InlineKeyboardButton::callback(
                i18n::t(lang, "ck_no_config").replacen("{}", p.label, 1),
                format!("c|ck|{}", p.id),
            )]);
            continue;
        }
        let current = state.get_cookie(p.id).await;
        // Status determination: not configured → expired (settings flag) → configured
        let status = if current.is_none() {
            i18n::t(lang, "ck_not_set")
        } else {
            let flag_key = crate::db::setting_key::cookie_expired_notified(p.id);
            let expired = crate::db::get_setting(&state.db, &flag_key)
                .await
                .ok()
                .flatten()
                .map(|v| v == "1")
                .unwrap_or(false);
            if expired { i18n::t(lang, "ck_expired") } else { i18n::t(lang, "ck_configured") }
        };
        buttons.push(vec![InlineKeyboardButton::callback(
            format!("{}（{}）", p.label, status),
            format!("c|ck|{}", p.id),
        )]);
    }

    buttons.push(vec![InlineKeyboardButton::callback(i18n::t(lang, "close"), "c|cancelop")]);

    let kb = InlineKeyboardMarkup::new(buttons);
    deliver(bot, &target, &text, Some(kb)).await?;
    Ok(())
}

/// Render the channel list page (paginated, PAGE_SIZE items per page).
pub async fn render_channel_list(
    bot: &Bot,
    target: PageTarget<'_>,
    state: &AppState,
    user_id: i64,
    page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    // Lang is resolved by the caller: the callback top-level and the command
    // path both run it through resolve_lang *with the Telegram client
    // language_code*, so the list page can no longer diverge from sibling
    // panels (previously this function re-resolved with None, which dropped
    // the client hint and forced English on never-configured users while
    // adjacent panels rendered Chinese).
    let chats = match db::list_chats(&state.db, user_id).await {
        Ok(c) => c,
        Err(e) => {
            deliver_error_page(bot, &target, lang, &format!("{:#}", e)).await?;
            return Ok(());
        }
    };

    if chats.is_empty() {
        deliver(bot, &target, i18n::t(lang, "chlist_empty"), Some(close_kb(lang))).await?;
        return Ok(());
    }

    let total_pages = chats.len().div_ceil(PAGE_SIZE);
    let page = page.min(total_pages.saturating_sub(1));
    let start = page * PAGE_SIZE;
    let end = (start + PAGE_SIZE).min(chats.len());

    // Buttons-only list: per-item text lines duplicated the buttons, so each
    // channel is now directly a clickable button.
    let text = if total_pages > 1 {
        i18n::t(lang, "chlist_multi")
            .replacen("{}", &(page + 1).to_string(), 1)
            .replacen("{}", &total_pages.to_string(), 1)
            .replacen("{}", &chats.len().to_string(), 1)
    } else {
        i18n::t(lang, "chlist_single").replacen("{}", &chats.len().to_string(), 1)
    };
    // Per-channel subscription counts for button labels. A DB failure must not
    // masquerade as "every channel has 0 subscriptions" — fall back to an
    // explicit "?" placeholder per chat in that case. Missing keys in a
    // successful result legitimately mean 0 (GROUP BY only returns chats with
    // at least one subscription).
    let sub_counts: Option<std::collections::HashMap<String, i64>> = match db::count_subscriptions_per_chat(&state.db).await {
        Ok(m) => Some(m),
        Err(e) => {
            tracing::warn!(error = %e, "count_subscriptions_per_chat failed; channel list will show ? for feed counts");
            None
        }
    };

    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    let mut row: Vec<InlineKeyboardButton> = Vec::new();

    for chat in &chats[start..end] {
        let title = chat_display_title(chat.title.as_deref(), &chat.chat_id);
        let cnt_label = match &sub_counts {
            Some(m) => m.get(&chat.chat_id).copied().unwrap_or(0).to_string(),
            None => "?".to_string(),
        };
        // The 4th segment passes through the channel-list page number so the channel page's
        // "back to channel list" button can restore the original page.
        row.push(InlineKeyboardButton::callback(
            format!("{}（{}）", crate::fetcher::truncate_text(&title, 32), cnt_label),
            format!("c|ch|{}|0|{}", chat.chat_id, page),
        ));
        if row.len() >= 2 {
            buttons.push(std::mem::take(&mut row));
        }
    }
    if !row.is_empty() {
        buttons.push(row);
    }
    if let Some(nav) = nav_row(page, total_pages, lang, |p| format!("c|chlist|{}", p)) {
        buttons.push(nav);
    }
    // Language switch button (shows current language, toggles on tap)
    buttons.push(vec![InlineKeyboardButton::callback(
        i18n::t(lang, "lang_btn"),
        "c|lang",
    )]);
    buttons.push(vec![InlineKeyboardButton::callback(i18n::t(lang, "close"), "c|cancelop")]);

    let kb = InlineKeyboardMarkup::new(buttons);
    deliver(bot, &target, &text, Some(kb)).await?;
    Ok(())
}

/// Render the channel page (subscription list + management buttons).
/// The source list is paginated, PAGE_SIZE items per page.
pub async fn render_channel_page(
    bot: &Bot,
    target: PageTarget<'_>,
    chat_id: &str,
    state: &AppState,
    page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    let chat = match db::get_chat(&state.db, chat_id).await {
        Ok(c) => c,
        Err(e) => {
            deliver_error_page(bot, &target, lang, &format!("{:#}", e)).await?;
            return Ok(());
        }
    };
    let title = chat_display_title(chat.as_ref().and_then(|c| c.title.as_deref()), chat_id);
    let subs = match db::get_subscriptions(&state.db, chat_id).await {
        Ok(s) => s,
        Err(e) => {
            deliver_error_page(bot, &target, lang, &format!("{:#}", e)).await?;
            return Ok(());
        }
    };

    let total_pages = subs.len().div_ceil(PAGE_SIZE).max(1);
    let page = page.min(total_pages.saturating_sub(1));
    let start = page * PAGE_SIZE;
    let end = (start + PAGE_SIZE).min(subs.len());

    let active_count = subs.iter().filter(|s| s.active == 1).count();
    let paused_count = subs.len() - active_count;

    let mut text = i18n::t(lang, "chpage_header")
        .replacen("{}", &crate::push::format_chat_title_html(&title, chat_id), 1)
        .replacen("{}", &crate::push::format_chat_id_html(chat_id), 1)
        .replacen("{}", &subs.len().to_string(), 1)
        .replacen("{}", &active_count.to_string(), 1)
        .replacen("{}", &paused_count.to_string(), 1);
    if subs.is_empty() {
        text.push_str(i18n::t(lang, "chpage_no_subs"));
    } else if total_pages > 1 {
        text.push_str(
            &i18n::t(lang, "chpage_multi")
                .replacen("{}", &(page + 1).to_string(), 1)
                .replacen("{}", &total_pages.to_string(), 1),
        );
    } else {
        text.push_str(i18n::t(lang, "chpage_single"));
    }

    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();

    // Each source is directly a button opening its feed page. Paused sources
    // get a ⏸ marker; error counts stay off the list (see feed page).
    // Sequence numbers stay global (not reset per page) for stable reference.
    // The trailing chlist_page keeps the channel-list position alive across the
    // whole feed/unsub round trip (fix: without it, "back" buttons defaulted
    // the channel list to page 0).
    for (offset, sub) in subs[start..end].iter().enumerate() {
        let name = sub.feed_title.as_deref().unwrap_or(&sub.feed_url);
        let flag = if sub.active == 0 { " ⏸" } else { "" };
        buttons.push(vec![InlineKeyboardButton::callback(
            format!("{}. {}{}", start + offset + 1, crate::fetcher::truncate_text(name, 36), flag),
            format!("c|src|{}|{}|{}", sub.id, page, chlist_page),
        )]);
    }

    if let Some(nav) = nav_row(page, total_pages, lang, |p| format!("c|ch|{}|{}|{}", chat_id, p, chlist_page)) {
        buttons.push(nav);
    }

    buttons.push(vec![
        InlineKeyboardButton::callback(i18n::t(lang, "chpage_settings"), format!("c|cfg|{}", chat_id)),
        InlineKeyboardButton::callback(i18n::t(lang, "chpage_unsub"), format!("c|unsubpg|{}|0|{}|{}", chat_id, page, chlist_page)),
    ]);
    // When paused sources exist, offer a one-click enable-all (reset error counts and resume fetching immediately)
    // Layout: c|resumeall|<chat_id>|<chlist_page>|<ch_page> — ch_page preserves the source-list page
    // across the re-render, chlist_page preserves the channel-list page for its own "back" button.
    if paused_count > 0 {
        buttons.push(vec![InlineKeyboardButton::callback(
            i18n::t(lang, "chpage_resume_all").replacen("{}", &paused_count.to_string(), 1),
            format!("c|resumeall|{}|{}|{}", chat_id, chlist_page, page),
        )]);
    }
    buttons.push(vec![
        InlineKeyboardButton::callback(i18n::t(lang, "chpage_back"), format!("c|chlist|{}", chlist_page)),
        InlineKeyboardButton::callback(i18n::t(lang, "close"), "c|cancelop"),
    ]);

    let kb = InlineKeyboardMarkup::new(buttons);

    deliver(bot, &target, &text, Some(kb)).await?;

    Ok(())
}

/// Render the channel settings page (rsstt-style option buttons).
/// Loads the subscription; when it doesn't exist or the query errors, delivers an error page to the target and returns None.
/// Boilerplate shared by render_feed_page / render_feed_filter / load_feed_ctx.
async fn load_sub_or_deliver(
    bot: &Bot,
    target: &PageTarget<'_>,
    db: &crate::db::Db,
    sub_id: i64,
    lang: Lang,
) -> ResponseResult<Option<db::SubRow>> {
    match db::get_subscription_by_id(db, sub_id).await {
        Ok(Some(s)) => Ok(Some(s)),
        Ok(None) => {
            deliver(bot, target, i18n::t(lang, "sub_not_found"), Some(close_kb(lang))).await?;
            Ok(None)
        }
        Err(e) => {
            deliver_error_page(bot, target, lang, &format!("{}", e)).await?;
            Ok(None)
        }
    }
}

/// Render the per-source (feed) page: status, effective config with override
/// markers, and management buttons (pause/resume, rename, feed-level overrides).
///
/// Public entry point kept for the command path (rename-reply refresh), which
/// has no channel-list page context (a PendingOp stores ch_page only). The
/// callback dispatch uses `render_feed_page_impl` to carry the real
/// `chlist_page` so "back to channel" restores the original channel-list page.
pub async fn render_feed_page(
    bot: &Bot,
    target: PageTarget<'_>,
    sub_id: i64,
    state: &AppState,
    ch_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    render_feed_page_impl(bot, target, sub_id, state, ch_page, 0, lang).await
}

/// Internal feed page renderer carrying `chlist_page` (the channel-list page the
/// user came from) through every button's callback data.
async fn render_feed_page_impl(
    bot: &Bot,
    target: PageTarget<'_>,
    sub_id: i64,
    state: &AppState,
    ch_page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    let Some(sub) = load_sub_or_deliver(bot, &target, &state.db, sub_id, lang).await? else {
        return Ok(());
    };

    let cfg = db::get_effective_config(&state.db, &sub.chat_id, &sub.feed_url).await.ok();

    let name = sub.feed_title.as_deref().unwrap_or(&sub.feed_url);
    let status = if sub.active == 1 { i18n::t(lang, "feed_active") } else { i18n::t(lang, "feed_paused") };

    let mut text = i18n::t(lang, "feed_header")
        .replacen("{}", &crate::push::escape_attr(&sub.feed_url), 1)
        .replacen("{}", &crate::push::escape_html(name), 1)
        .replacen("{}", status, 1);
    // Show original title when manually renamed
    if sub.title_overridden == 1 {
        if let Some(orig) = &sub.original_title {
            text.push_str(&i18n::t(lang, "feed_orig_name").replacen("{}", &crate::push::escape_html(orig), 1));
        }
    }

    if let Some(c) = &cfg {
        let iv_mark = if c.iv_overridden { "✏️" } else { "🔗" };
        let cm_mark = if c.cm_overridden { "✏️" } else { "🔗" };
        let sd_mark = if c.sd_overridden { "✏️" } else { "🔗" };
        let lp_mark = if c.lp_overridden { "✏️" } else { "🔗" };
        text.push_str(
            &i18n::t(lang, "feed_config")
                .replacen("{}", &fmt_interval(lang, c.interval_minutes), 1)
                .replacen("{}", iv_mark, 1)
                .replacen("{}", fmt_content_mode(lang, &c.content_mode), 1)
                .replacen("{}", cm_mark, 1)
                .replacen("{}", fmt_source_display(lang, &c.source_display), 1)
                .replacen("{}", sd_mark, 1)
                .replacen("{}", if c.link_preview { i18n::t(lang, "on") } else { i18n::t(lang, "off") }, 1)
                .replacen("{}", lp_mark, 1),
        );
    }

    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();

    // Pause / resume
    if sub.active == 1 {
        buttons.push(vec![InlineKeyboardButton::callback(
            i18n::t(lang, "feed_pause"),
            format!("c|srcpause|{}|{}|{}", sub_id, ch_page, chlist_page),
        )]);
    } else {
        buttons.push(vec![InlineKeyboardButton::callback(
            i18n::t(lang, "feed_resume"),
            format!("c|srcresume|{}|{}|{}", sub_id, ch_page, chlist_page),
        )]);
    }

    // Rename / reset name
    buttons.push(vec![
        InlineKeyboardButton::callback(i18n::t(lang, "feed_rename"), format!("c|srcrename|{}|{}|{}", sub_id, ch_page, chlist_page)),
    ]);


    // Feed-level overrides: each option opens a second-level page (mirrors the
    // channel settings layout). Labels show the effective value; override state
    // is shown in the summary above.
    if let Some(c) = &cfg {
        buttons.push(vec![InlineKeyboardButton::callback(
            i18n::t(lang, "feed_interval").replacen("{}", &fmt_interval(lang, c.interval_minutes), 1),
            format!("c|srcfip|{}|{}|{}", sub_id, ch_page, chlist_page),
        )]);
        buttons.push(vec![InlineKeyboardButton::callback(
            i18n::t(lang, "feed_mode").replacen("{}", fmt_content_mode(lang, &c.content_mode), 1),
            format!("c|srcfmp|{}|{}|{}", sub_id, ch_page, chlist_page),
        )]);
        buttons.push(vec![InlineKeyboardButton::callback(
            i18n::t(lang, "feed_source").replacen("{}", fmt_source_display(lang, &c.source_display), 1),
            format!("c|srcfsp|{}|{}|{}", sub_id, ch_page, chlist_page),
        )]);
        buttons.push(vec![InlineKeyboardButton::callback(
            i18n::t(lang, "feed_link_preview").replacen("{}", if c.link_preview { i18n::t(lang, "on") } else { i18n::t(lang, "off") }, 1),
            format!("c|srcflp|{}|{}|{}", sub_id, ch_page, chlist_page),
        )]);
        // Note: under Approach A, picking a value identical to the channel automatically clears
        // the override, so no global reset button is needed; each sub-page still keeps the
        // "↩️ 🔗 follow channel" button.
    }

    // Content filter button (shows status when active)
    {
        let fc = db::get_feed_filter(&state.db, &sub.chat_id, &sub.feed_url).await.unwrap_or_default();
        let label = if fc.is_active() {
            let mode_label = if fc.mode == "whitelist" { i18n::t(lang, "whitelist") } else { i18n::t(lang, "blacklist") };
            let kw_count = fc.keyword_list().len();
            i18n::t(lang, "feed_filter_active")
                .replacen("{}", mode_label, 1)
                .replacen("{}", &kw_count.to_string(), 1)
        } else {
            i18n::t(lang, "feed_filter").to_string()
        };
        buttons.push(vec![InlineKeyboardButton::callback(
            label,
            format!("c|srcfilter|{}|{}|{}", sub_id, ch_page, chlist_page),
        )]);
    }

    buttons.push(vec![
        InlineKeyboardButton::callback(i18n::t(lang, "feed_back"), format!("c|ch|{}|{}|{}", sub.chat_id, ch_page, chlist_page)),
        InlineKeyboardButton::callback(i18n::t(lang, "close"), "c|cancelop"),
    ]);

    let kb = InlineKeyboardMarkup::new(buttons);
    deliver(bot, &target, &text, Some(kb)).await?;
    Ok(())
}

/// Handle my_chat_member updates: auto-register or purge on removal.
pub async fn handle_my_chat_member(
    update: ChatMemberUpdated,
    state: AppState,
) -> ResponseResult<()> {
    let chat = update.chat.clone();
    let chat_id = chat.id.0.to_string();
    // Telegram also emits my_chat_member for private chats (user blocks /
    // unblocks the bot). Those must not be registered as manageable "channels"
    // (they would pollute the /set list and allow a nonsensical self-subscribe),
    // nor purged as if a channel. Only groups/supergroups/channels are in scope.
    if !(chat.is_channel() || chat.is_supergroup() || chat.is_group()) {
        return Ok(());
    }
    let chat_type = crate::db::chat_type_str(chat.is_channel(), chat.is_group(), chat.is_supergroup());
    // Record the inviter (the user who performed the add/remove action) as the channel owner.
    let inviter_id = update.from.id.0.to_string();

    match update.new_chat_member.status() {
        ChatMemberStatus::Member
        | ChatMemberStatus::Administrator
        | ChatMemberStatus::Owner => {
            let title = chat.title().map(|t| t.to_string());
            let username = chat.username().map(|u| u.to_string());
            if let Err(e) = db::upsert_chat(&state.db, &chat_id, chat_type, title.as_deref(), username.as_deref(), Some(&inviter_id)).await {
                error!(chat_id = %chat_id, error = %e, "failed to upsert chat");
            } else {
                info!(chat_id = %chat_id, added_by = %inviter_id, "chat registered");
            }
        }
        ChatMemberStatus::Left | ChatMemberStatus::Banned => {
            // Bot removed: purge all data for this chat
            if let Err(e) = db::purge_chat(&state.db, &chat_id).await {
                error!(chat_id = %chat_id, error = %e, "failed to purge chat");
            } else {
                info!(chat_id = %chat_id, "bot removed — chat data purged");
            }
        }
        _ => {}
    }

    Ok(())
}
