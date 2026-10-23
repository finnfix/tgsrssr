//! Unsubscribe flow: single unsub, unsub panel, unsub-all confirmation,
//! OPML backup generation. Split out of callbacks/mod.rs (batch-3 refactor);
//! behavior unchanged. Uses the parent module's private helpers (`deliver`,
//! `nav_row`, `PageTarget`, `PAGE_SIZE`) via `super::`.

use teloxide::prelude::*;
use teloxide::types::{CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, ParseMode};
use tracing::{error, info};

use super::{deliver, deliver_error_page, fetch_sub_or_toast, nav_row, paginate, PageTarget};
use crate::push::no_preview;
use crate::db;
use crate::i18n::{self, Lang};
use crate::state::AppState;

/// Handle single unsub callback: delete subscription, notify, then refresh the
/// panel in place (same page, clamped) so the removed button disappears.
// Page-threading params (page/ch_page/chlist_page) pushed the arity past the
// clippy default; bundling them into a struct was considered and rejected as
// more disruptive than the established allow pattern (start_pending_op).
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_unsub_callback(
    bot: &Bot,
    q: &CallbackQuery,
    state: &AppState,
    sub_id: i64,
    page: usize,
    ch_page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    let qid = q.id.clone();
    let Some(sub) = fetch_sub_or_toast(state, bot, qid.clone(), sub_id, lang).await else {
        // fetch_sub_or_toast already answered the query with the failure or
        // "not found" toast; nothing else to do.
        return Ok(());
    };

    let name = sub.feed_title.as_deref().unwrap_or(&sub.feed_url).to_string();
    let feed_url = sub.feed_url.clone();
    let chat_id = sub.chat_id.clone();

    if let Err(e) = db::delete_subscription(&state.db, sub_id).await {
        bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed").replacen("{:#}", &format!("{:#}", e), 1)).await.ok();
        return Ok(());
    }

    // feed_configs overrides are dropped inside delete_subscription's
    // transaction, so a re-subscribe starts from channel defaults.

    info!(sub_id = sub_id, feed = %feed_url, "subscription removed");

    // Immediately send a notice (source name hyperlinked to the feed URL);
    // the /unsub list page stays open so the user can continue unsubscribing
    // (closed later via the Cancel button, silently).
    let chat = q.message.as_ref().map(|m| m.chat().id);
    bot.answer_callback_query(qid.clone()).await.ok();
    if let Some(chat) = chat {
        let msg = i18n::t(lang, "unsub_ok")
            .replacen("{}", &crate::push::escape_attr(&feed_url), 1)
            .replacen("{}", &crate::push::escape_html(&name), 1);
        bot.send_message(chat, msg)
            .parse_mode(ParseMode::Html)
            .link_preview_options(no_preview())
            .await.ok();
    }

    // Refresh the panel in place so the deleted source's button disappears.
    render_unsub_panel(bot, PageTarget::Edit(q), &chat_id, state, page, ch_page, chlist_page, lang).await?;

    Ok(())
}

/// Render the /unsub panel (paginated, PAGE_SIZE items per page). Each source
/// is directly an "unsubscribe" button; paused sources get a ⏸ marker.
/// `chlist_page` is the channel-list page threaded through the round trip so
/// the "back to channel page" button restores the user's original list page.
#[allow(clippy::too_many_arguments)]
pub(super) async fn render_unsub_panel(
    bot: &Bot,
    target: PageTarget<'_>,
    chat_id: &str,
    state: &AppState,
    page: usize,
    ch_page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    let subs = match db::get_subscriptions(&state.db, chat_id).await {
        Ok(s) => s,
        Err(e) => {
            deliver_error_page(bot, &target, lang, &format!("{:#}", e)).await?;
            return Ok(());
        }
    };

    if subs.is_empty() {
        // #15: the empty state provides a back-to-channel-page button; no dead page is left behind.
        let kb = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
            i18n::t(lang, "unsub_back"),
            format!("c|ch|{}|{}|{}", chat_id, ch_page, chlist_page),
        )]]);
        deliver(bot, &target, i18n::t(lang, "unsub_panel_empty"), Some(kb)).await?;
        return Ok(());
    }

    let (total_pages, page, start, end) = paginate(subs.len(), page);

    let text = if total_pages > 1 {
        i18n::t(lang, "unsub_panel_multi")
            .replacen("{}", &(page + 1).to_string(), 1)
            .replacen("{}", &total_pages.to_string(), 1)
            .replacen("{}", &subs.len().to_string(), 1)
    } else {
        i18n::t(lang, "unsub_panel_single")
            .replacen("{}", &subs.len().to_string(), 1)
    };

    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    for (offset, sub) in subs[start..end].iter().enumerate() {
        let name = sub.feed_title.as_deref().unwrap_or(&sub.feed_url);
        let flag = if sub.active == 0 { " ⏸" } else { "" };
        buttons.push(vec![InlineKeyboardButton::callback(
            format!("{}. {}{}", start + offset + 1, crate::fetcher::truncate_text(name, 30), flag),
            format!("c|unsub|{}|{}|{}|{}", sub.id, page, ch_page, chlist_page),
        )]);
    }
    if let Some(nav) = nav_row(page, total_pages, lang, |p| format!("c|unsubpg|{}|{}|{}|{}", chat_id, p, ch_page, chlist_page)) {
        buttons.push(nav);
    }
    // The "unsubscribe all" entry point appears only in this panel (there is no /unsub_all command);
    // tapping it enters the confirmation panel.
    // #36: pass through the current page number so the confirmation page's "Back" button can restore the original page.
    buttons.push(vec![InlineKeyboardButton::callback(
        i18n::t(lang, "unsub_all_btn"),
        format!("c|unsuballask|{}|{}|{}|{}", chat_id, page, ch_page, chlist_page),
    )]);
    buttons.push(vec![InlineKeyboardButton::callback(
        i18n::t(lang, "unsub_back"),
        format!("c|ch|{}|{}|{}", chat_id, ch_page, chlist_page),
    )]);

    let kb = InlineKeyboardMarkup::new(buttons);
    deliver(bot, &target, &text, Some(kb)).await?;
    Ok(())
}

/// Render the "unsubscribe all" confirmation panel. Entry point is the button
/// on the /unsub panel (c|unsuballask); there is no /unsub_all command.
/// #36: the page parameter lets the "Back" button restore the unsub panel's original page number.
#[allow(clippy::too_many_arguments)]
pub(super) async fn render_unsub_all_confirm(
    bot: &Bot,
    target: PageTarget<'_>,
    chat_id: &str,
    state: &AppState,
    page: usize,
    ch_page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    let subs = match db::get_subscriptions(&state.db, chat_id).await {
        Ok(s) => s,
        Err(e) => {
            deliver_error_page(bot, &target, lang, &format!("{}", e)).await?;
            return Ok(());
        }
    };
    if subs.is_empty() {
        let kb = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
            i18n::t(lang, "unsub_back"),
            format!("c|ch|{}|{}|{}", chat_id, ch_page, chlist_page),
        )]]);
        deliver(bot, &target, i18n::t(lang, "unsub_panel_empty"), Some(kb)).await?;
        return Ok(());
    }

    let chat = db::get_chat(&state.db, chat_id).await.ok().flatten();
    let title = super::chat_display_title(chat.as_ref().and_then(|c| c.title.as_deref()), chat_id);

    let text = i18n::t(lang, "unsuball_confirm")
        .replacen("{}", &crate::push::format_chat_title_html(&title, chat_id), 1)
        .replacen("{}", &crate::push::format_chat_id_html(chat_id), 1)
        .replacen("{}", &subs.len().to_string(), 1);

    let kb = InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback(i18n::t(lang, "unsuball_confirm_btn"), format!("c|unsuball|{}|{}|{}|{}", chat_id, ch_page, page, chlist_page)),
        InlineKeyboardButton::callback(i18n::t(lang, "back"), format!("c|unsubpg|{}|{}|{}|{}", chat_id, page, ch_page, chlist_page)),
    ]]);

    deliver(bot, &target, &text, Some(kb)).await?;
    Ok(())
}

/// Handle unsub_all callback: generate OPML backup, send it, then delete all.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_unsub_all_callback(
    bot: &Bot,
    q: &CallbackQuery,
    state: &AppState,
    chat_id: &str,
    ch_page: usize,
    page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    let qid = q.id.clone();

    let subs = match db::get_subscriptions(&state.db, chat_id).await {
        Ok(s) => s,
        Err(e) => {
            bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed").replacen("{:#}", &format!("{:#}", e), 1)).await.ok();
            return Ok(());
        }
    };

    if subs.is_empty() {
        bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "unsub_no_subs")).await.ok();
        return Ok(());
    }

    // Idempotency protection: when the confirm button is double-tapped quickly, two callbacks
    // run concurrently and would send the OPML backup twice.
    // On entry, edit the confirmation message to "Processing…" — Telegram rejects an edit whose
    // text is unchanged, so the second callback's edit inevitably fails (message is not modified),
    // which acts as the preemption.
    // Placed after the subscription query: ensures early-exit paths like "no subscriptions" don't
    // leave the message stuck at "Processing…".
    if let Some(m) = q.message.as_ref() {
        match bot
            .edit_message_text(m.chat().id, m.id(), i18n::t(lang, "unsuball_processing"))
            .await
        {
            Ok(_) => {}
            Err(e) => {
                // Only a genuine "message is not modified" means another
                // callback already preempted this one (double-tap). A transient
                // network / Bot API error must NOT be mistaken for a duplicate —
                // otherwise the unsuball silently aborts (neither unsubscribing
                // nor restoring the page). On transient errors, propagate so the
                // dispatcher can retry rather than reporting "already processing".
                let is_not_modified = matches!(
                    e,
                    teloxide::RequestError::Api(teloxide::ApiError::MessageNotModified)
                ) || e.to_string().contains("not modified");
                if is_not_modified {
                    bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "unsuball_dup")).await.ok();
                    return Ok(());
                }
                return Err(e);
            }
        }
    }

    // Generate OPML
    let opml = generate_opml(&subs);
    let opml_bytes = opml.as_bytes().to_vec();

    let chat = db::get_chat(&state.db, chat_id).await.ok().flatten();
    let title = super::chat_display_title(chat.as_ref().and_then(|c| c.title.as_deref()), chat_id);
    let safe_name: String = title.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
    let filename = format!("rss_backup_{}.opml", safe_name);

    // Send backup file first — only proceed if successful
    let target_chat = match q.message.as_ref().map(|m| m.chat().id) {
        Some(t) => t,
        None => {
            bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "unsuball_no_target")).await.ok();
            return Ok(());
        }
    };

    let doc = teloxide::types::InputFile::memory(opml_bytes).file_name(filename.clone());
    match bot.send_document(target_chat, doc)
        .caption(i18n::t(lang, "unsuball_caption")
            .replacen("{}", &title, 1)
            .replacen("{}", &subs.len().to_string(), 1))
        .await
    {
        Ok(_) => {}
        Err(e) => {
            error!(chat_id = chat_id, error = %e, "failed to send OPML backup");
            bot.answer_callback_query(qid.clone())
                .text(i18n::t(lang, "unsuball_backup_failed"))
                .await?;
            // Restore the confirmation page (the idempotency lock has already changed the message
            // to "Processing…"; without restoring it, retries would be locked out forever).
            render_unsub_all_confirm(bot, PageTarget::Edit(q), chat_id, state, page, ch_page, chlist_page, lang).await?;
            return Ok(());
        }
    }

    // Now delete all subscriptions
    let count = match db::delete_all_subscriptions(&state.db, chat_id).await {
        Ok(c) => c,
        Err(e) => {
            bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed").replacen("{:#}", &format!("{:#}", e), 1)).await.ok();
            // Restore the confirmation page (same as above).
            render_unsub_all_confirm(bot, PageTarget::Edit(q), chat_id, state, page, ch_page, chlist_page, lang).await?;
            return Ok(());
        }
    };

    // feed_configs overrides are dropped inside delete_all_subscriptions'
    // transaction, so re-subscribes start from channel defaults.

    info!(chat_id = chat_id, count = count, "all subscriptions removed with backup");

    // Operation complete: edit the confirmation message in place to show the
    // result (avoid deleting messages). Falls back to sending a new message
    // when the original can no longer be edited.
    match &q.message {
        Some(m) => {
            let chat = m.chat().id;
            let mid = m.id();
            bot.answer_callback_query(qid.clone()).await.ok();
            let text = i18n::t(lang, "unsuball_ok")
                .replacen("{}", &crate::push::format_chat_title_html(&title, chat_id), 1)
                .replacen("{}", &crate::push::format_chat_id_html(chat_id), 1)
                .replacen("{}", &count.to_string(), 1);
            let kb = InlineKeyboardMarkup::new(vec![vec![
                InlineKeyboardButton::callback(i18n::t(lang, "unsuball_view"), format!("c|ch|{}|{}|{}", chat_id, ch_page, chlist_page)),
            ]]);
            if bot
                .edit_message_text(chat, mid, &text)
                .parse_mode(ParseMode::Html)
                .link_preview_options(no_preview())
                .reply_markup(kb.clone())
                .await
                .is_err()
            {
                bot.send_message(chat, text)
                    .parse_mode(ParseMode::Html)
                    .link_preview_options(no_preview())
                    .reply_markup(kb)
                    .await
                    .ok();
            }
        }
        None => {
            bot.answer_callback_query(qid.clone())
                .text(i18n::t(lang, "unsuball_toast").replacen("{}", &count.to_string(), 1))
                .await?;
        }
    }

    Ok(())
}

/// Generate OPML XML from subscriptions.
fn generate_opml(subs: &[db::SubRow]) -> String {
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <opml version=\"2.0\">\n<head><title>RSS Subscriptions Backup</title></head>\n<body>\n",
    );
    for sub in subs {
        let title = sub.feed_title.as_deref().unwrap_or(&sub.feed_url);
        // OPML attribute escaping is exactly equivalent to push::escape_attr, so reuse it uniformly.
        xml.push_str(&format!(
            "  <outline type=\"rss\" text=\"{}\" title=\"{}\" xmlUrl=\"{}\"/>\n",
            crate::push::escape_attr(title),
            crate::push::escape_attr(title),
            crate::push::escape_attr(&sub.feed_url)
        ));
    }
    xml.push_str("</body>\n</opml>\n");
    xml
}
