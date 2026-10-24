use feed_rs::model::Entry;
use teloxide::prelude::*;
use teloxide::types::{
    ChatId, InputFile, InputMedia, InputMediaDocument, InputMediaPhoto, InputMediaVideo, Message,
    ParseMode,
};

use crate::db::{self, Db};
use crate::fetcher;
use crate::i18n::Lang;
use crate::htmltext::{
    merge_adjacent_blockquotes, split_html, strip_html_tags, truncate_html_keep_links,
};
use crate::media::{MediaItem, MediaKind};
use crate::sendpolicy::{
    flood_backoff, flood_retry_secs, is_retryable_error, pace, SEND_BACKOFF_SECS,
};

// Re-export pure HTML helpers for external callers (commands, callbacks,
// scheduler, platforms). `pub use` also brings them into push.rs scope.
pub use crate::htmltext::{
    escape_attr, escape_html, format_chat_id_html, format_chat_title_html,
    normalize_blockquote_fold, take_utf16_prefix, utf16_len, visible_len,
};

// Hot-path regexes are compiled statically once (hit on every message/chunk).
// RE_TAG and the anchor/entity regexes live in crate::htmltext along with the
// HTML helpers (imported via the `use` at the top).
// Rate-limiting/backoff policy (pacer, flood_backoff, is_retryable_error,
// SEND_BACKOFF_SECS) moved to crate::sendpolicy (imported via the `use` at
// the top).
// The Weibo poll data marker has been migrated to EntryMeta.poll (group-C
// sidecar); RE_VOTE_DIV has been removed.

/// Leave room for suffixes. Telegram's hard limit is 4096 visible chars;
/// the footer (source line) is appended to the last chunk, and the split
/// budget reserves the footer's visible length + 2 separator chars up front
/// (see send_entry_msg), so staying within this limit keeps the combined
/// text under 4096.
const SAFE_LIMIT: usize = 4096;
/// Telegram caption limit for media messages.
const CAPTION_LIMIT: usize = 1024;

/// Shared constructor that disables link preview (used by command responses,
/// notifications, and text messages).
/// Commands/notifications often contain source links and message links; the
/// preview cards are noise.
pub fn no_preview() -> teloxide::types::LinkPreviewOptions {
    teloxide::types::LinkPreviewOptions {
        is_disabled: true,
        url: None,
        prefer_small_media: false,
        prefer_large_media: false,
        show_above_text: false,
    }
}

/// Retire a pending-op prompt by editing it into a plain status line.
/// Editing (instead of deleting) keeps the reply target alive so the
/// client's local reply-draft — and the quote anchor of the user's reply —
/// never dangles on a deleted message.
///
/// Prompts are sent as plain messages without any reply markup (ForceReply
/// was removed in v1.3.2: its markup could not be reliably cleared via
/// editMessageText and left clients stuck in a permanent reply state), so a
/// single text edit suffices. If the edit fails (e.g. message too old) the
/// message is left in place — never deleted.
///
/// Defined in push.rs (batch A3: breaks the callbacks→commands reverse
/// dependency edge); commands keeps a re-export to preserve existing call
/// paths.
pub async fn retire_prompt(bot: &Bot, chat: teloxide::types::ChatId, prompt_msg_id: i32, status: &str) {
    let mid = teloxide::types::MessageId(prompt_msg_id);
    if bot.edit_message_text(chat, mid, status).await.is_err() {
        // Truly uneditable (e.g. too old). Leave it in place — deleting would
        // remove the very message the user's reply quotes.
        tracing::warn!(
            chat_id = chat.0,
            message_id = prompt_msg_id,
            "retire_prompt: edit failed, leaving message in place (no delete)"
        );
    }
}

/// Outcome of sending one entry.
/// - `media_failed`: true when media could not be sent (fell back to text,
///   or a document send failed) — the caller reports this privately to
///   the chat owner, never to the channel.
/// - `entry_link`: the source entry URL (e.g. the Weibo post).
/// - `delivered_tg_link`: t.me link of the message actually delivered (the
///   main media message / text fallback), used in owner notices and for
///   linking download warnings back to the channel.
/// - `delivered_message_id`: numeric message id of the delivered message,
///   used to register the TG-message → RSS-entry mapping for /find.
#[derive(Debug, Clone, Default)]
pub struct SendOutcome {
    pub media_failed: bool,
    pub entry_link: Option<String>,
    pub delivered_tg_link: Option<String>,
    pub delivered_message_id: Option<i64>,
    /// The entry's main message is the plain-text fallback (including the
    /// text-only path and "all media failed -> send full text"). Used by the
    /// oversized-video notice so it does not claim a text message when the
    /// entry was actually delivered with other media.
    pub text_only: bool,
}

/// Extract Weibo poll data from EntryMeta (group-C sidecar explicit protocol,
/// replacing the former in-body data-vote marker parsing). Returns
/// (question, options, is_multi_select).
/// Telegram limits: question ≤300 chars, options 2-10 items of ≤100 chars each.
pub fn extract_poll(meta: Option<&crate::entrymeta::EntryMeta>) -> Option<(String, Vec<String>, bool)> {
    let poll = meta.and_then(|m| m.poll.as_ref())?;
    truncate_poll(&poll.question, &poll.options, poll.is_multi_select)
}

/// Truncate to Telegram poll limits: question ≤300, options ≤10 items of
/// ≤100 chars each.
/// Counted in UTF-16 code units (Telegram's measure; emoji count as 2 units),
/// consistent with message splitting (#3 fix).
fn truncate_poll(question: &str, options: &[String], is_multi: bool) -> Option<(String, Vec<String>, bool)> {
    // Guard: an empty question is never turned into a poll (weibo.rs already
    // blocks this at construction time; this defends against outbox
    // deserialization or future data bypassing the construction-side check).
    if question.trim().is_empty() {
        return None;
    }
    let question = question.to_string();
    let question = if utf16_len(&question) > 300 {
        format!("{}...", take_utf16_prefix(&question, 297))
    } else {
        question
    };
    let options: Vec<String> = options
        .iter()
        .take(10)
        .map(|o| {
            if utf16_len(o) > 100 {
                format!("{}...", take_utf16_prefix(o, 97))
            } else {
                o.clone()
            }
        })
        .collect();
    if options.len() < 2 {
        return None;
    }
    Some((question, options, is_multi))
}

/// Send a Telegram poll (sendPoll). On failure only a warn log is emitted;
/// the entry push is not affected.
/// The poll replies to the main message (reply_to) so it stays associated
/// within the channel.
/// Always anonymous: Telegram forbids non-anonymous polls in channels
/// (Bad Request: non-anonymous polls can't be sent to channel chats).
/// After a successful send the poll message is pinned automatically
/// (following the channel notification options, allowed to replace an older
/// pin).
/// Paced before sending; on 429 it backs off and retries via flood_backoff
/// (#4 fix).
pub async fn send_poll(
    bot: &Bot,
    chat_id: i64,
    question: &str,
    options: &[String],
    is_multi: bool,
    reply_to: Option<teloxide::types::MessageId>,
) -> anyhow::Result<Message> {
    use teloxide::requests::Requester;
    use teloxide::types::InputPollOption;
    let poll_options: Vec<InputPollOption> = options
        .iter()
        .map(|o| InputPollOption { text: o.clone(), formatting: None })
        .collect();
    pace(chat_id).await;
    let mut flood_tries: u32 = 0;
    let mut transient_tries: usize = 0;
    let msg = loop {
        let mut req = bot
            .send_poll(ChatId(chat_id), question, poll_options.clone())
            .is_anonymous(true)
            .allows_multiple_answers(is_multi);
        if let Some(mid) = reply_to {
            req = req.reply_parameters(teloxide::types::ReplyParameters::new(mid));
        }
        match req.await {
            Ok(m) => break m,
            Err(e) => {
                let es = e.to_string();
                if flood_backoff(&es, &mut flood_tries, chat_id).await {
                    continue;
                }
                // v1.10.2: retry on network/TG transient errors (consistent
                // with the other send sites)
                if is_retryable_error(&es) && transient_tries < SEND_BACKOFF_SECS.len() {
                    let backoff = SEND_BACKOFF_SECS[transient_tries];
                    tracing::warn!(chat_id = chat_id, backoff = backoff, error = %es, "poll send transient error, backing off");
                    tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
                    transient_tries += 1;
                    continue;
                }
                return Err(e.into());
            }
        }
    };
    // Pin the poll message: on failure only warn (e.g. missing permission);
    // the poll itself is not affected.
    if let Err(e) = bot.pin_chat_message(ChatId(chat_id), msg.id).await {
        tracing::warn!(chat_id = chat_id, error = %e, "pin poll message failed (missing permission?)");
    }
    Ok(msg)
}

/// Unified follow-up poll helper for Weibo polls (extracted in v1.9.7):
/// after the body is delivered, try to extract and send the poll.
/// Returns whether the poll send failed (so the caller can set media_failed
/// and notify the owner).
/// All send paths (plain text / media / media-failure fallback / outbox
/// retry) go through this single function, preventing another missed-send
/// caused by copy-paste drift (the root cause fixed in v1.9.6 was exactly a
/// missing call on the plain-text path).
pub async fn try_send_poll(
    bot: &Bot,
    chat_id: i64,
    reply_to: Option<teloxide::types::MessageId>,
    meta: Option<&crate::entrymeta::EntryMeta>,
) -> bool {
    if let Some((question, options, is_multi)) = extract_poll(meta) {
        if let Err(e) = send_poll(bot, chat_id, &question, &options, is_multi, reply_to).await {
            tracing::warn!(chat_id = chat_id, error = %e, "poll send failed");
            return true;
        }
    }
    false
}

/// Unified helper for the plain-text send path (H1 fix: the scheduler main
/// push and the outbox retry previously duplicated
/// format_entry_parts→send_entry_msg→try_send_poll→SendOutcome verbatim,
/// and the poll follow-up logic had been fixed twice because of it
/// (v1.9.3/v1.9.6); converged into a single function to prevent further
/// drift).
/// Returns Ok(SendOutcome) or Err (the caller decides between outbox and
/// fallback).
pub async fn send_text_only(
    bot: &Bot,
    chat_id: i64,
    entry: &Entry,
    feed_title: &str,
    author: Option<&str>,
    opts: RenderOpts,
    meta: Option<&crate::entrymeta::EntryMeta>,
) -> anyhow::Result<SendOutcome> {
    let Some((body, footer)) = format_entry_parts(entry, feed_title, author, opts, meta) else {
        return Ok(SendOutcome::default());
    };
    let m = send_entry_msg(bot, chat_id, &body, &footer, opts.link_preview).await?;
    let poll_failed = try_send_poll(bot, chat_id, Some(m.id), meta).await;
    Ok(SendOutcome {
        media_failed: poll_failed,
        entry_link: fetcher::entry_link(entry),
        delivered_tg_link: tg_message_link(&m),
        delivered_message_id: Some(m.id.0 as i64),
        text_only: true,
    })
}

/// Content mode for rendering.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ContentMode {
    /// Title (with hyperlink)
    TitleLink,
    /// Title (with hyperlink) + summary (first 50 chars of body + "......")
    TitleSummary,
    /// Title (with hyperlink) + content (truncated when too long)
    TitleContent,
    /// Title only (no link)
    TitleOnly,
    /// Content only (no title; fits title-less sources such as Weibo /
    /// Telegram channels)
    ContentOnly,
    /// Media only (silently skipped when there is no media)
    MediaOnly,
}

impl ContentMode {
    pub fn from_str(s: &str) -> Self {
        // Accept the legacy camel-case spellings used by older schema
        // defaults ('TitleSummary') in addition to the canonical snake_case.
        match s.to_ascii_lowercase().as_str() {
            "title_summary" | "titlesummary" => Self::TitleSummary,
            "title_content" | "titlecontent" => Self::TitleContent,
            "title_only" | "titleonly" => Self::TitleOnly,
            "content_only" | "contentonly" => Self::ContentOnly,
            "media_only" | "mediaonly" => Self::MediaOnly,
            _ => Self::TitleLink,
        }
    }

    /// Whether this mode skips media fetching entirely (title+link /
    /// title-only: push text only, never fetch media).
    /// The scheduler (timed pushes) and /test (commands) share this check to
    /// avoid rule drift between the two sites.
    pub fn skip_media(&self) -> bool {
        matches!(self, ContentMode::TitleLink | ContentMode::TitleOnly)
    }

    /// Whether this mode keeps only the first media item (title_summary).
    /// At fetch time `prepare_entry_media` stops after the first successfully
    /// downloaded image/video (audio/documents and GIFs are never fetched);
    /// the slice in `send_entry_with_media` stays as a compatibility guard
    /// for outbox rows serialized by older versions (they can still carry a
    /// full media set). Shared by scheduler and /test to avoid rule drift.
    pub fn single_media(&self) -> bool {
        matches!(self, ContentMode::TitleSummary)
    }
}

/// Source footer display.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SourceDisplay {
    /// "via <source title>" (the source title carries the entry hyperlink)
    Source,
    /// "author: <author>" (no link)
    Author,
    /// Not shown
    None,
}

impl SourceDisplay {
    pub fn from_str(s: &str) -> Self {
        // Accept legacy camel-case spellings from old schema defaults, same
        // tolerance as ContentMode::from_str.
        match s.to_ascii_lowercase().as_str() {
            "author" => Self::Author,
            "none" => Self::None,
            _ => Self::Source,
        }
    }
}

/// Rendering options for one push.
#[derive(Debug, Clone, Copy)]
pub struct RenderOpts {
    pub mode: ContentMode,
    pub source: SourceDisplay,
    pub link_preview: bool,
    /// Owner language for user-visible fallbacks (missing title, etc.).
    pub lang: Lang,
}

/// Source footer line, or empty string.
fn source_line(entry: &Entry, feed_title: &str, author: Option<&str>, sd: SourceDisplay) -> String {
    match sd {
        SourceDisplay::Source => {
            if feed_title.is_empty() {
                return String::new();
            }
            match fetcher::entry_link(entry) {
                Some(url) => format!(
                    "<i>via <a href=\"{}\">{}</a></i>",
                    escape_attr(&url),
                    escape_html(feed_title)
                ),
                None => format!("<i>via {}</i>", escape_html(feed_title)),
            }
        }
        SourceDisplay::Author => match author {
            Some(a) if !a.is_empty() => format!("<i>author: {}</i>", escape_html(a)),
            _ => String::new(),
        },
        SourceDisplay::None => String::new(),
    }
}

/// Max visible characters kept in a `Title+summary` excerpt.
const SUMMARY_MAX_CHARS: usize = 140;

/// Build a short summary from the entry body: the first `max_chars` visible
/// characters as Telegram-HTML with hyperlinks preserved, with "......"
/// appended when truncated. Returns empty string when the body has no
/// visible text.
fn entry_summary(entry: &Entry, max_chars: usize, meta: Option<&crate::entrymeta::EntryMeta>) -> String {
    let html = fetcher::entry_body_html(entry, 100_000, meta);
    if html.is_empty() {
        return String::new();
    }
    // Truncate on the Telegram-HTML the body pipeline already produced:
    // media hrefs were dropped upstream by `html_to_telegram_html` (per
    // `is_media_href`), so every surviving anchor is a real content link
    // (e.g. the WeChat article behind "本文来自微信公众号：账号名") and stays
    // clickable in the summary. The cut counts display characters only —
    // href text never eats into the budget.
    let (cut, truncated) = truncate_html_keep_links(&html, max_chars);
    // Whitespace at the edges comes from flattened block tags (the old
    // plain-text path trimmed the same way); an all-whitespace cut means the
    // body had no visible content.
    let cut = cut.trim();
    if cut.is_empty() {
        return String::new();
    }
    if truncated {
        format!("{}......", cut)
    } else {
        cut.to_string()
    }
}

/// Format an entry into (body_html, footer_html) separately.
/// The footer (source line) is returned separately so that when the message
/// is split into multiple parts, the footer can be placed on the LAST message.
/// Returns None for MediaOnly.
pub fn format_entry_parts(
    entry: &Entry,
    feed_title: &str,
    author: Option<&str>,
    opts: RenderOpts,
    meta: Option<&crate::entrymeta::EntryMeta>,
) -> Option<(String, String)> {
    render_parts(entry, feed_title, author, opts, meta)
        .map(|r| (r.parts.join("\n\n"), r.footer))
}

/// Shared rendering for the plain-text and caption paths. These were two
/// near-identical implementations that had already drifted once (TitleContent
/// silently lost the body on the media path), so the rules live here only.
struct RenderedParts {
    parts: Vec<String>,
    footer: String,
}

fn render_parts(
    entry: &Entry,
    feed_title: &str,
    author: Option<&str>,
    opts: RenderOpts,
    meta: Option<&crate::entrymeta::EntryMeta>,
) -> Option<RenderedParts> {
    let title = fetcher::entry_title(entry, opts.lang);
    let link = fetcher::entry_link(entry);
    let head = |linked: bool| -> String {
        match (&link, linked) {
            (Some(url), true) => format!(
                "<b><a href=\"{}\">{}</a></b>",
                escape_attr(url),
                escape_html(&title)
            ),
            _ => format!("<b>{}</b>", escape_html(&title)),
        }
    };

    let mut parts: Vec<String> = Vec::new();
    match opts.mode {
        ContentMode::MediaOnly => return None,
        ContentMode::TitleLink => parts.push(head(true)),
        ContentMode::TitleSummary => {
            parts.push(head(true));
            let summary = entry_summary(entry, SUMMARY_MAX_CHARS, meta);
            if !summary.is_empty() {
                parts.push(summary);
            }
        }
        ContentMode::TitleContent => {
            parts.push(head(true));
            // Body is already Telegram-HTML (media stripped, links as <a>).
            // A large limit: splitting into multiple messages happens at send
            // time, so we never want to fall back to plain text here.
            let body = fetcher::entry_body_html(entry, 100_000, meta);
            if !body.is_empty() {
                parts.push(body);
            }
        }
        ContentMode::TitleOnly => parts.push(head(false)),
        ContentMode::ContentOnly => {
            let body = fetcher::entry_body_html(entry, 100_000, meta);
            if body.is_empty() {
                // No-content fallback: degrade to a bold title to avoid an
                // empty message (both text and caption paths).
                parts.push(head(false));
            } else {
                parts.push(body);
            }
        }
    }
    Some(RenderedParts {
        parts,
        footer: source_line(entry, feed_title, author, opts.source),
    })
}

/// Build a media caption. All modes are platform-agnostic — Weibo gets no
/// special-casing here (Weibo sources are typically switched to ContentOnly
/// by the operator, which renders the post body directly).
///
/// - MediaOnly: only the source line.
/// - ContentOnly: show the body text directly (no synthesized title).
/// - Title modes: bold title (linked where applicable) + optional summary.
///
/// No truncation here — `send_entry_with_media` splits into a ≤1024 caption
/// plus follow-up text messages via `split_html`.
fn format_caption(
    entry: &Entry,
    feed_title: &str,
    author: Option<&str>,
    opts: RenderOpts,
    meta: Option<&crate::entrymeta::EntryMeta>,
) -> String {
    let Some(rendered) = render_parts(entry, feed_title, author, opts, meta) else {
        return source_line(entry, feed_title, author, opts.source);
    };
    let mut parts = rendered.parts;
    if !rendered.footer.is_empty() {
        parts.push(rendered.footer);
    }
    parts.join("\n\n")
}

/// Build the InputFile for a local file path.
/// - Local Bot API + translatable path → `InputFile::file` (server reads its
///   own mount: zero copy, no RAM, 2GB cap).
/// - Otherwise → memory upload (official API or untranslatable path).
async fn path_input(
    path: &std::path::Path,
    transport: &crate::media::MediaTransport,
) -> anyhow::Result<InputFile> {
    if let Some(bp) = transport.bot_path(path) {
        Ok(InputFile::file(std::path::PathBuf::from(bp)))
    } else {
        // Reading a media file can be tens of MB; use async IO instead of
        // blocking a tokio worker while the bytes are loaded.
        Ok(InputFile::memory(tokio::fs::read(path).await?))
    }
}

/// Cache-aware input construction: first look up the file_id cache by
/// "content hash:send mode"; on a hit use `InputFile::Id` directly
/// (Telegram's server already has the file, no re-upload needed).
/// On a miss fall back to local file / memory upload.
/// The cache key carries a send-mode suffix (photo/gif/video/doc): the same
/// content sent via different modes yields non-interchangeable file_ids, so
/// they are cached separately to avoid cross-use.
async fn resolve_input(
    db: &Db,
    item: &MediaItem,
    transport: &crate::media::MediaTransport,
    mode: &str,
    use_cache: bool,
) -> anyhow::Result<InputFile> {
    if use_cache {
        let key = format!("{}:{}", item.content_hash, mode);
        if let Ok(Some(fid)) = db::get_cached_file_id(db, &key).await {
            return Ok(InputFile::file_id(teloxide::types::FileId(fid)));
        }
    }
    path_input(&item.path, transport).await
}

/// Extract the media file_id from a sent message by send mode (used to write
/// back to the cache).
/// In teloxide 0.17 the media types expose the file_id via `.file.id`
/// (FileId newtype).
fn message_file_id_for(msg: &Message, mode: &str) -> Option<String> {
    match mode {
        "video" => msg.video().map(|v| v.file.id.0.clone()),
        "gif" => msg.animation().map(|a| a.file.id.0.clone()),
        "doc" => msg.document().map(|d| d.file.id.0.clone()),
        _ => msg.photo().and_then(|s| s.last()).map(|p| p.file.id.0.clone()),
    }
}

/// Send mode of an item inside a media group (consistent with the
/// build_rich_ops / media-group construction logic).
fn group_item_mode(m: &MediaItem) -> &'static str {
    match m.kind {
        MediaKind::Video => "video",
        _ if m.is_gif => "gif",
        _ => "photo",
    }
}

/// Write the file_id back to the cache after a successful send (failures are
/// silently ignored; the push is not affected).
async fn cache_file_id(db: &Db, item: &MediaItem, mode: &str, file_id: Option<String>) {
    if let Some(fid) = file_id {
        let key = format!("{}:{}", item.content_hash, mode);
        db::put_cached_file_id(db, &key, &fid).await.ok();
    }
}

/// Invalidate all cache entries involved by an op (called when Telegram
/// rejects a file_id; afterwards it automatically falls back to re-upload).
async fn invalidate_op_cache(db: &Db, op: &RichOp<'_>) {
    let items: Vec<(&MediaItem, &str)> = match op {
        RichOp::Photo(p) => vec![(p, "photo")],
        RichOp::Gif(g) => vec![(g, "gif")],
        RichOp::Video(v) => vec![(v, "video")],
        RichOp::Group(items) => items.iter().map(|m| (*m, group_item_mode(m))).collect(),
    };
    for (item, mode) in items {
        let key = format!("{}:{}", item.content_hash, mode);
        db::invalidate_cached_file_id(db, &key).await.ok();
    }
}

/// Determine whether an error is "file_id invalid/unusable" — in that case
/// the cache should be invalidated and the file re-uploaded, rather than
/// degrading to plain text.
fn is_file_id_error(es: &str) -> bool {
    let s = es.to_lowercase();
    s.contains("file_id") || s.contains("file identifier") || s.contains("wrong file")
}

/// Telegram rejects thumbnails outside 320x320 / 200KB. A thumbnail-related
/// 400 must drop the thumbnail and retry, not fail the whole document.
fn is_thumbnail_error(es: &str) -> bool {
    let s = es.to_lowercase();
    s.contains("thumbnail") || s.contains("photo_invalid_dimensions") || s.contains("wrong file dimensions")
}

/// Entry context for one push: collapses the many parameters of
/// send_entry_with_media into a single struct that callers construct once.
/// The fields are destructured into same-named local variables inside the
/// function body; behavior is unchanged.
pub struct PushCtx<'a> {
    pub entry: &'a Entry,
    pub feed_title: &'a str,
    pub author: Option<&'a str>,
    pub opts: RenderOpts,
    pub caption_above_media: bool,
    pub aspect_threshold: f64,
    pub media: &'a [MediaItem],
    /// Structured entry metadata (group-C sidecar); None for RSS feeds /
    /// legacy outbox behavior.
    pub meta: Option<&'a crate::entrymeta::EntryMeta>,
}

/// Send an entry with prepared media. Strategy:
/// - images ≤10MB (normal aspect) → sendPhoto / sendMediaGroup
/// - images >10MB (photo hard limit) → sendDocument
/// - long images (h/w > threshold) → sendDocument (avoid photo-viewer cropping)
/// - videos → sendVideo (single) or mixed into media group when with images
/// - caption position follows the channel setting for all platforms
/// - transport: local Bot API uses file:// zero-copy; official API uploads bytes
/// - any send failure → fall back to text-only so the entry is never lost, and
///   set `media_failed` (reported privately to the chat owner, not the channel).
pub async fn send_entry_with_media(
    bot: &Bot,
    db: &Db,
    chat_id: i64,
    ctx: PushCtx<'_>,
    transport: &crate::media::MediaTransport,
) -> anyhow::Result<SendOutcome> {
    let PushCtx {
        entry,
        feed_title,
        author,
        opts,
        caption_above_media,
        aspect_threshold,
        media,
        meta,
    } = ctx;
    // Caption rendering and placement are uniform across platforms; no
    // Weibo-specific handling.
    let full_caption = format_caption(entry, feed_title, author, opts, meta);
    let caption_above = caption_above_media;

    // Title+summary keeps at most one media item. The fetch layer already
    // enforces this for new downloads (prepare_entry_media single_media:
    // stops after the first image/video, never fetches audio or GIFs), so
    // this slice is a compatibility guard for outbox rows serialized by
    // older versions: when images are present only the first one is sent
    // (no full album group); otherwise the existing video/document behavior
    // is kept.
    // (Title+link / title-only modes already skip media downloads at the
    // scheduler layer and never reach here.)
    let media: &[MediaItem] = if opts.mode == ContentMode::TitleSummary {
        match media.iter().position(|m| m.kind == MediaKind::Image) {
            Some(i) => &media[i..i + 1],
            None => media,
        }
    } else {
        media
    };

    if media.is_empty() {
        // No media: plain text message path (split into multiple if needed,
        // footer placed on the last message). The first message link is
        // returned so download warnings can link back to the delivered entry.
        return send_text_only(bot, chat_id, entry, feed_title, author, opts, meta).await;
    }

    // Caption is limited to 1024 chars. If the full text exceeds it, use the
    // first chunk as the media caption and send the rest as follow-up messages.
    // The overflow is sent as plain text messages (limit 4096), so re-split
    // the remainder at SAFE_LIMIT instead of CAPTION_LIMIT.
    //
    // Splitting at 1024 may break one large blockquote into multiple
    // fragments (each ≤1024); before re-splitting the overflow at 4096,
    // adjacent fragments are merged back into a single blockquote to avoid
    // multiple collapsed blocks in one message. If the merged result still
    // exceeds 4096, split_html splits it again (each part wrapped on its own).
    let caption_chunks = split_html(&full_caption, CAPTION_LIMIT);
    let caption = caption_chunks.first().cloned().unwrap_or_default();
    let overflow_chunks: Vec<String> = if caption_chunks.len() > 1 {
        let rest = merge_adjacent_blockquotes(&caption_chunks[1..].join("\n\n"));
        split_html(&rest, SAFE_LIMIT)
    } else {
        Vec::new()
    };

    // Classify. Converting long/oversized (>10MB) images to documents is a
    // normal downgrade: only a debug log is recorded, and admins are not
    // notified (only an actual send failure triggers a notification).
    let mut photos: Vec<&MediaItem> = Vec::new();
    let mut gifs: Vec<&MediaItem> = Vec::new();
    let mut docs: Vec<&MediaItem> = Vec::new();
    let mut videos: Vec<&MediaItem> = Vec::new();
    // Inline images from the retweeted original post (Weibo [view image]
    // links); sent separately and quoting the main message.
    let mut rt_photos: Vec<&MediaItem> = Vec::new();
    for m in media {
        // Retweet inline images: go through the same oversized/long-image/GIF
        // classification; anything over the limits is merged into docs.
        if m.is_retweet && m.kind == MediaKind::Image {
            let format_doc = !m.is_gif && !crate::media::photo_format_compatible(&m.path);
            if crate::media::image_goes_to_doc(m.size, m.width, m.height, aspect_threshold) || format_doc {
                docs.push(m);
            } else if m.is_gif {
                gifs.push(m);
            } else {
                rt_photos.push(m);
            }
            continue;
        }
        match m.kind {
            MediaKind::Image => {
                let format_doc = !m.is_gif && !crate::media::photo_format_compatible(&m.path);
                if crate::media::image_goes_to_doc(m.size, m.width, m.height, aspect_threshold) || format_doc {
                    docs.push(m);
                    tracing::debug!(
                        size = m.size,
                        w = m.width,
                        h = m.height,
                        format_doc = format_doc,
                        "image -> document (oversized/long/w+h/format)"
                    );
                } else if m.is_gif {
                    // GIFs go through sendAnimation; the photo API does not
                    // play animated images
                    gifs.push(m);
                } else {
                    photos.push(m);
                }
            }
            MediaKind::Video => videos.push(m),
            MediaKind::Other => docs.push(m),
        }
    }

    // Op-list send: each op == one Telegram API call, so a retried send can
    // resume after the last successful op without duplicating messages.
    // Rich media (images/GIFs/videos) are sent first as the entry's main
    // message; the converted-to-document messages then follow as "quote
    // replies" to the main message, making it easy to jump between them.
    let rich_ops = build_rich_ops(&photos, &gifs, &videos);
    // Network errors (proxy blips / upstream hiccups) get bounded exponential
    // backoff, resuming from the last successful op each time so partial
    // progress is never re-sent. Non-network errors (bad request, parse) fail
    // fast to the text fallback.
    // The entry's main message (the first rich-media message); document
    // messages quote it for easy navigation.
    let mut anchor: Option<Message> = None;
    // True once the caption carrier (first rich op or the full-text fallback)
    // has been delivered; the post-send sanity check below must not treat a
    // partial media failure as "caption undelivered" and trigger an outbox
    // replay that duplicates it.
    let mut caption_delivered = false;

    if !rich_ops.is_empty() {
        let rich = send_rich_ops_with_retry(bot, db, chat_id, &caption, caption_above, &rich_ops, transport, None, "main").await;
        match rich {
            Ok((_, first_msg)) => {
                anchor = first_msg;
                caption_delivered = true;
            }
            Err((sent, delivered, e)) => {
                tracing::warn!(
                    chat_id = chat_id,
                    entry_id = %fetcher::entry_id(entry),
                    failed_op = sent,
                    total_ops = rich_ops.len(),
                    error = %e,
                    "media send failed, falling back to text"
                );
                // Fall back to text so the entry is never lost, and capture the
                // delivered message's link for the owner notice. `delivered`
                // is the first op's message when sent > 0; a later retry may
                // have resumed after it, but the helper keeps the original
                // first message across attempts.
                let mut delivered_tg_link = delivered.as_ref().and_then(|m| tg_message_link(m));
                let mut fallback_msg_id: Option<teloxide::types::MessageId> =
                    delivered.as_ref().map(|m| m.id);
                let mut text_delivered = false;
                if sent > 0 {
                    // The first op (which carries the caption) was delivered
                    // before the failure. Re-sending the full title+body would
                    // duplicate the caption in the channel, so only the
                    // caption overflow is sent as follow-up messages.
                    text_delivered = true;
                    for chunk in &overflow_chunks {
                        if let Err(te) = send_chunk_with_retry(bot, chat_id, chunk, false, fallback_msg_id).await {
                            tracing::warn!(
                                chat_id = chat_id,
                                entry_id = %fetcher::entry_id(entry),
                                error = %te,
                                "overflow text failed after partial media delivery"
                            );
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
                    }
                } else if let Some((body, footer)) = format_entry_parts(entry, feed_title, author, opts, meta) {
                    match send_entry_msg(bot, chat_id, &body, &footer, opts.link_preview).await {
                        Ok(m) => {
                            delivered_tg_link = tg_message_link(&m);
                            fallback_msg_id = Some(m.id);
                            text_delivered = true;
                        }
                        Err(te) => {
                            // Text fallback also failed: the entry was not
                            // delivered at all. Return Err so the scheduler
                            // goes through outbox persistent retry — never
                            // silently mark it sent and lose the entry.
                            tracing::warn!(chat_id = chat_id, entry_id = %fetcher::entry_id(entry), error = %te, "text fallback also failed, propagating for outbox");
                            return Err(anyhow::anyhow!(
                                "media send failed ({}) and text fallback also failed: {}",
                                e, te
                            ));
                        }
                    }
                }
                // Media-only mode (no text to fall back to): the entry was
                // not delivered at all; likewise goes through outbox retry.
                if !text_delivered {
                    return Err(anyhow::anyhow!(
                        "media send failed ({}) and no text fallback available",
                        e
                    ));
                }
                // After the main media failed and fell back to plain text,
                // still try to send the converted-to-document long/large
                // images, quoting the fallback body message for easy
                // navigation. The text is already delivered, so a document
                // failure does not affect entry completeness.
                for (di, d) in docs.iter().enumerate() {
                    if let Err(de) = send_doc_msg(bot, db, chat_id, d, "", fallback_msg_id, transport).await {
                        tracing::warn!(
                            chat_id = chat_id,
                            entry_id = %fetcher::entry_id(entry),
                            failed_doc = di,
                            skipped_docs = docs.len() - di - 1,
                            error = %de,
                            "document send failed after text fallback"
                        );
                        break;
                    }
                }
                // Retweet inline images get the same follow-up send, quoting
                // the fallback body message.
                if !rt_photos.is_empty() {
                    let rt_ops = build_rich_ops(&rt_photos, &[], &[]);
                    if let Err((_, _, re)) = send_rich_ops(bot, db, chat_id, "", false, &rt_ops, transport, 0, fallback_msg_id).await {
                        tracing::warn!(chat_id = chat_id, entry_id = %fetcher::entry_id(entry), error = %re, "retweet images send failed after text fallback");
                    }
                }
                // Weibo poll: after the media failure falls back to plain
                // text, the poll is likewise sent as a follow-up (v1.9.7
                // fix: this path previously missed the send, so when the
                // media send of a poll post with media failed, the poll was
                // permanently lost).
                try_send_poll(bot, chat_id, fallback_msg_id, meta).await;
                return Ok(SendOutcome {
                    media_failed: true,
                    entry_link: fetcher::entry_link(entry),
                    delivered_tg_link,
                    delivered_message_id: fallback_msg_id.map(|m| m.0 as i64),
                    // sent > 0: the first media op (caption) was delivered;
                    // sent == 0: the full text fallback carries the entry.
                    text_only: sent == 0,
                });
            }
        }
    }

    // Converted-to-document messages: multiple documents are sent as a group
    // via sendMediaGroup (InputMediaDocument), appearing visually as one
    // unit, which naturally solves the "scattered into N independent,
    // unrelated messages" problem (previously, with one-by-one sendDocument
    // and no rich-media main message, anchor was None and the document
    // messages had no quote at all). On group failure it falls back to
    // one-by-one sending as a safety net so nothing is lost.
    // When the entry consists only of documents (no rich media), the caption
    // is placed on the first document of the first group.
    // A document send failure does not fall back to plain text (the body was
    // already delivered with the main message); it only sets media_failed
    // and relies on the admin notification as the safety net.
    let mut media_failed = false;
    let mut first_doc_msg: Option<Message> = None;
    if !docs.is_empty() {
        let reply_to = anchor.as_ref().map(|m| m.id);
        let cap = if anchor.is_none() { caption.as_str() } else { "" };
        match send_doc_group(bot, db, chat_id, &docs, cap, reply_to, transport).await {
            Ok(msgs) => {
                first_doc_msg = msgs.into_iter().next();
            }
            Err((delivered, e)) => {
                tracing::warn!(chat_id = chat_id, entry_id = %fetcher::entry_id(entry), error = %e, delivered = delivered.len(), "doc group send failed, falling back to one-by-one");
                // Fall back to one-by-one sending: skip the part already
                // delivered as a group (in multi-group scenarios, when group
                // 2 fails, group 1 is already delivered and must not be
                // re-sent). When there is no rich-media main message (anchor
                // is None), subsequent documents quote the first document,
                // avoiding a recurrence of the "scattered, unrelated" defect.
                let mut chain_id: Option<teloxide::types::MessageId> = reply_to;
                // First message of the already-delivered part: used as
                // first_doc_msg and as the quote anchor for what follows
                if let Some(first) = delivered.first() {
                    first_doc_msg = Some(first.clone());
                    if anchor.is_none() && chain_id.is_none() {
                        chain_id = Some(first.id);
                    }
                }
                for (i, d) in docs.iter().enumerate().skip(delivered.len()) {
                    let cap = if i == 0 && anchor.is_none() { caption.as_str() } else { "" };
                    match send_doc_msg(bot, db, chat_id, d, cap, chain_id, transport).await {
                        Ok(m) => {
                            if first_doc_msg.is_none() {
                                first_doc_msg = Some(m.clone());
                            }
                            // Without an anchor, the first document becomes
                            // the quote target for subsequent documents
                            if anchor.is_none() && chain_id.is_none() {
                                chain_id = Some(m.id);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(chat_id = chat_id, entry_id = %fetcher::entry_id(entry), error = %e, "document send failed");
                            media_failed = true;
                            break;
                        }
                    }
                }
            }
        }
    }

    // Inline images from the retweeted original post (decoded from Weibo
    // [view image] links): merged into a single media message, quoting the
    // main message (anchor) for easy navigation.
    // Special case: when the entry's only media are all retweet images (the
    // main post has no images) → rich_ops is empty and docs is empty, so the
    // caption (body text) has no other carrier; in that case attach it to
    // the first rt op to prevent losing the body text.
    if !rt_photos.is_empty() {
        let rt_is_sole_carrier = anchor.is_none() && first_doc_msg.is_none();
        let rt_caption = if rt_is_sole_carrier { caption.as_str() } else { "" };
        let rt_reply_to = anchor.as_ref().or(first_doc_msg.as_ref()).map(|m| m.id);
        let rt_ops = build_rich_ops(&rt_photos, &[], &[]);
        // Network backoff retry consistent with the main rich_ops (resume
        // from the breakpoint).
        let rt_res = send_rich_ops_with_retry(bot, db, chat_id, rt_caption, caption_above, &rt_ops, transport, rt_reply_to, "rt_images").await;
        match rt_res {
            Ok((_, first_rt_msg)) => {
                // When retweet images become the only delivered carrier,
                // record the anchor so subsequent overflow can quote it.
                if rt_is_sole_carrier {
                    if let Some(m) = first_rt_msg {
                        anchor = Some(m);
                    }
                }
            }
            Err((sent, delivered, e)) => {
                tracing::warn!(
                    chat_id = chat_id,
                    entry_id = %fetcher::entry_id(entry),
                    failed_op = sent,
                    total_ops = rt_ops.len(),
                    error = %e,
                    "retweet inline images send failed"
                );
                media_failed = true;
                // A non-zero `sent` means op 0 completed. When these rt
                // images are the sole caption carrier, the caption was
                // delivered even though a later group failed; marking it
                // undelivered here would enqueue the entry for outbox retry
                // and re-send the already delivered group every backoff
                // until the 72h abandon. Keep the delivered message so
                // overflow/poll can quote it and /find records a usable id.
                if rt_is_sole_carrier && sent > 0 {
                    caption_delivered = true;
                    if anchor.is_none() {
                        anchor = delivered.map(|m| *m);
                    }
                }
            }
        }
    }

    // Caption overflow follows the entry's messages. For document-only
    // entries the first document carries the caption, and the overflow text
    // follows right after it. Overflow chunks uniformly quote the first
    // message (the anchor rich-media main message or the first document),
    // flat structure, consistent with plain-text chunking.
    // Note: if rich media/documents are already delivered (anchor or
    // first_doc_msg is set), an overflow text failure does not return Err —
    // otherwise an upstream retry would re-send the media as a whole and
    // create duplicates. Instead, set media_failed and continue returning Ok.
    let overflow_reply_to: Option<teloxide::types::MessageId> = anchor
        .as_ref()
        .or(first_doc_msg.as_ref())
        .map(|m| m.id);
    // `caption_delivered` also covers a partial rt/rich send whose message
    // object is unavailable: an overflow failure must not trigger a whole
    // entry replay and duplicate the caption.
    let already_delivered = caption_delivered || anchor.is_some() || first_doc_msg.is_some();
    for chunk in &overflow_chunks {
        if let Err(e) = send_chunk_with_retry(bot, chat_id, chunk, false, overflow_reply_to).await {
            if already_delivered {
                tracing::warn!(chat_id = chat_id, entry_id = %fetcher::entry_id(entry), error = %e, "overflow text failed after delivery, not retrying");
                media_failed = true;
                break;
            }
            return Err(e);
        }
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    }

    // Weibo poll: after the body is delivered, convert the poll data into a
    // native Telegram poll and send it.
    // A poll failure does not affect entry completeness (the body is already
    // delivered); it only sets media_failed to notify the owner.
    {
        let poll_reply_to = anchor
            .as_ref()
            .or(first_doc_msg.as_ref())
            .map(|m| m.id);
        if try_send_poll(bot, chat_id, poll_reply_to, meta).await {
            media_failed = true;
        }
    }

    // P1-1 fix (v1.10.21): the first caption chunk is attached to the first
    // carrier (document/retweet images). If all carriers fail (both anchor
    // and first_doc_msg are None), the first 1024 characters of the body
    // were never delivered — even if the overflow text succeeded, the entry
    // is still incomplete.
    // The previous guard additionally required !overflow_delivered, which
    // caused "all carriers failed + overflow succeeded" to wrongly return
    // Ok, the entry to be marked sent, and the first caption chunk to be
    // permanently lost.
    // Fix: as long as all caption carriers fail, return Err for outbox retry
    // regardless of whether the overflow was delivered. The cost is that the
    // overflow text may be duplicated on retry, but that is better than
    // losing the body text.
    if !caption_delivered
        && anchor.is_none()
        && first_doc_msg.is_none()
        && (!docs.is_empty() || !rt_photos.is_empty())
    {
        return Err(anyhow::anyhow!(
            "all caption carriers (documents/retweet images) failed to send; caption undelivered"
        ));
    }

    let delivered_tg_link = anchor
        .as_ref()
        .and_then(tg_message_link)
        .or_else(|| first_doc_msg.as_ref().and_then(tg_message_link));
    let delivered_message_id = anchor
        .as_ref()
        .or(first_doc_msg.as_ref())
        .map(|m| m.id.0 as i64);

    Ok(SendOutcome {
        media_failed,
        entry_link: fetcher::entry_link(entry),
        delivered_tg_link,
        delivered_message_id,
        text_only: false,
    })
}

/// One media send operation (granularity = one Telegram API call).
/// Converted-to-document messages do not go through here — they are sent
/// separately after the rich-media main message and quote it.
enum RichOp<'a> {
    Photo(&'a MediaItem),
    Gif(&'a MediaItem),
    Video(&'a MediaItem),
    Group(Vec<&'a MediaItem>),
}

/// Order rich media (videos/GIFs/images) as "photo/video album first → GIFs
/// last", splitting the album pool into groups of ≤10 each.
/// Telegram sendMediaGroup allows at most 10 media per call; anything beyond
/// that is split into follow-up albums, which quote the first album's
/// message when sent for easy navigation.
/// Photos and videos are merged into one pool (videos leading, images after)
/// because Telegram's sendMediaGroup accepts them in the same album — the
/// shared type constraint only applies to documents and audios (which must be
/// grouped with same-type peers), so those still go through `send_doc_group`
/// as follow-up messages that quote the album.
/// GIFs must be separated into independent ops (sendAnimation) and always
/// follow the album: Telegram's sendMediaGroup does not accept the animation
/// type — mixing one into an image/video album gets the whole album rejected
/// (Bad Request: type "animation" can't be used in sendMediaGroup).
fn build_rich_ops<'a>(
    photos: &[&'a MediaItem],
    gifs: &[&'a MediaItem],
    videos: &[&'a MediaItem],
) -> Vec<RichOp<'a>> {
    let mut ops: Vec<RichOp<'a>> = Vec::new();

    // Videos + images share one album pool (videos first, preserving the
    // historical "video leads" ordering for multi-media entries).
    let mut pool: Vec<&'a MediaItem> = Vec::with_capacity(videos.len() + photos.len());
    pool.extend_from_slice(videos);
    pool.extend_from_slice(photos);

    // chunks(10) never yields an empty slice, so len is always ≥1.
    // A single item cannot form an album (sendMediaGroup requires 2–10), so
    // dispatch it to the dedicated single-media op by kind.
    for chunk in pool.chunks(10) {
        if chunk.len() == 1 {
            if chunk[0].kind == MediaKind::Video {
                ops.push(RichOp::Video(chunk[0]));
            } else {
                ops.push(RichOp::Photo(chunk[0]));
            }
        } else {
            ops.push(RichOp::Group(chunk.to_vec()));
        }
    }

    // GIFs are sent one by one independently (sendAnimation), never in an
    // album, and always after the album (they cannot share the album anyway).
    for g in gifs {
        ops.push(RichOp::Gif(g));
    }

    ops
}

/// Send converted-to-document media as a group (sendMediaGroup +
/// InputMediaDocument).
/// Groups of ≤10 documents each; the first group carries the caption (if
/// any), and follow-up groups quote the first message of the first group.
/// Uses the file_id cache (mode="doc"); a rejected file_id is automatically
/// invalidated and re-uploaded with one retry.
/// Returns all sent messages (in order); the caller takes the first one as
/// the anchor/link.
async fn send_doc_group(
    bot: &Bot,
    db: &Db,
    chat_id: i64,
    docs: &[&MediaItem],
    caption: &str,
    reply_to: Option<teloxide::types::MessageId>,
    transport: &crate::media::MediaTransport,
) -> Result<Vec<Message>, (Vec<Message>, anyhow::Error)> {
    let chat = ChatId(chat_id);
    let caption = normalize_blockquote_fold(caption);
    let caption = caption.as_str();

    // sendMediaGroup requires 2-10 media items; a single document goes
    // through sendDocument (consistent with build_rich_ops sending a single
    // photo via send_photo).
    if docs.len() == 1 {
        let m = send_doc_msg(bot, db, chat_id, docs[0], caption, reply_to, transport)
            .await
            .map_err(|e| (Vec::new(), e))?;
        return Ok(vec![m]);
    }

    let mut all_msgs: Vec<Message> = Vec::new();
    let mut first_msg_id: Option<teloxide::types::MessageId> = None;

    for (gi, chunk) in docs.chunks(10).enumerate() {
        pace(chat_id).await;
        // The caption goes only on the first document of the first group;
        // follow-up groups carry no caption
        let group_caption = if gi == 0 { caption } else { "" };
        // Follow-up groups quote the first message of the first group
        let reply_msg_id = if gi > 0 { first_msg_id } else { reply_to };
        let rp = reply_msg_id.map(teloxide::types::ReplyParameters::new);

        // sendMediaGroup requires 2-10 items; a trailing single document
        // (11/21/31 docs) must go through sendDocument instead of a group.
        if chunk.len() == 1 {
            let m = send_doc_msg(bot, db, chat_id, chunk[0], group_caption, reply_msg_id, transport)
                .await
                .map_err(|e| (all_msgs.clone(), e))?;
            if first_msg_id.is_none() {
                first_msg_id = Some(m.id);
            }
            all_msgs.push(m);
            continue;
        }

        let mut use_cache = true;
        let mut use_thumbs = true;
        let mut flood_tries = 0u32;
        let mut transient_tries = 0usize;
        let msgs = loop {
            let mut group: Vec<InputMedia> = Vec::new();
            for (j, d) in chunk.iter().enumerate() {
                let inp = resolve_input(db, d, transport, "doc", use_cache)
                    .await
                    .map_err(|e| (all_msgs.clone(), e))?;
                let mut im = InputMediaDocument::new(inp);
                if j == 0 && !group_caption.is_empty() {
                    im = im.caption(group_caption.to_string()).parse_mode(ParseMode::Html);
                }
                // Converted-to-document long/large images carry a thumbnail
                if use_thumbs {
                    if let Some(tp) = &d.thumb_path {
                        if let Ok(tin) = path_input(tp, transport).await {
                            im.thumbnail = Some(tin);
                        }
                    }
                }
                group.push(InputMedia::Document(im));
            }
            let mut req = bot.send_media_group(chat, group);
            if let Some(ref params) = rp {
                req = req.reply_parameters(params.clone());
            }
            match req.await {
                Ok(msgs) => break msgs,
                Err(e) if use_cache && is_file_id_error(&e.to_string()) => {
                    for d in chunk {
                        let key = format!("{}:doc", d.content_hash);
                        db::invalidate_cached_file_id(db, &key).await.ok();
                    }
                    pace(chat_id).await;
                    use_cache = false;
                }
                Err(e) => {
                    let es = e.to_string();
                    if use_thumbs && is_thumbnail_error(&es) {
                        tracing::warn!(chat_id = chat_id, error = %es, "doc group thumbnail rejected, retrying without thumbnails");
                        use_thumbs = false;
                        continue;
                    }
                    if flood_backoff(&es, &mut flood_tries, chat_id).await {
                        continue;
                    }
                    // v1.10.2: retry on network/TG transient errors
                    // (consistent with send_rich_ops_with_retry)
                    if is_retryable_error(&es) && transient_tries < SEND_BACKOFF_SECS.len() {
                        let backoff = SEND_BACKOFF_SECS[transient_tries];
                        tracing::warn!(chat_id = chat_id, backoff = backoff, error = %es, "doc group send transient error, backing off");
                        tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
                        transient_tries += 1;
                        continue;
                    }
                    // Return the already-delivered messages so the caller's
                    // one-by-one fallback skips the delivered part, avoiding
                    // group 1 being re-sent when group 2 fails in a
                    // multi-group scenario.
                    return Err((all_msgs, e.into()));
                }
            }
        };
        // Write cache: returned messages correspond to the chunk in order
        for (m, d) in msgs.iter().zip(chunk.iter()) {
            cache_file_id(db, d, "doc", message_file_id_for(m, "doc")).await;
        }
        if first_msg_id.is_none() {
            first_msg_id = msgs.first().map(|m| m.id);
        }
        all_msgs.extend(msgs);
    }
    Ok(all_msgs)
}

/// Send a single converted-to-document message (sendDocument). The document
/// message quotes the entry's main message (reply_to) so it is easy to jump
/// between the document and the body text; when the entry consists only of
/// documents, the caption is placed on the first document. Also uses the
/// file_id cache (mode="doc").
async fn send_doc_msg(
    bot: &Bot,
    db: &Db,
    chat_id: i64,
    d: &MediaItem,
    caption: &str,
    reply_to: Option<teloxide::types::MessageId>,
    transport: &crate::media::MediaTransport,
) -> anyhow::Result<Message> {
    pace(chat_id).await;
    // Unified blockquote folding rules
    let caption = normalize_blockquote_fold(caption);
    let caption = caption.as_str();
    // file_id cache retry: the first round uses the cached file_id
    // (use_cache=true); if Telegram rejects it (file_id expired), invalidate
    // that cache entry and re-upload once from the local file.
    // resolve_input(use_cache=false) is equivalent to calling path_input
    // directly, matching the behavior of the original retry path.
    let mut use_cache = true;
    let mut attach_thumb = d.thumb_path.is_some();
    let mut flood_tries = 0u32;
    let mut transient_tries = 0usize;
    let m = loop {
        let inp = resolve_input(db, d, transport, "doc", use_cache).await?;
        let mut req = bot.send_document(ChatId(chat_id), inp);
        // Converted-to-document long/large images carry a thumbnail so the
        // message list shows a preview; a thumbnail-related 400 drops it and
        // retries so the document itself is still delivered.
        if attach_thumb {
            if let Some(tp) = &d.thumb_path {
                if let Ok(tin) = path_input(tp, transport).await {
                    req = req.thumbnail(tin);
                }
            }
        }
        if !caption.is_empty() {
            // Note: Telegram's show_caption_above_media only supports
            // photo/video/animation/paid media; a document message's caption
            // is always fixed below the file.
            req = req.caption(caption).parse_mode(ParseMode::Html);
        }
        if let Some(rid) = reply_to {
            req = req.reply_parameters(teloxide::types::ReplyParameters::new(rid));
        }
        match req.await {
            Ok(m) => break m,
            Err(e) if use_cache && is_file_id_error(&e.to_string()) => {
                let key = format!("{}:doc", d.content_hash);
                db::invalidate_cached_file_id(db, &key).await.ok();
                pace(chat_id).await;
                use_cache = false;
            }
            Err(e) => {
                let es = e.to_string();
                if attach_thumb && is_thumbnail_error(&es) {
                    tracing::warn!(chat_id = chat_id, error = %es, "document thumbnail rejected, retrying without it");
                    attach_thumb = false;
                    continue;
                }
                if flood_backoff(&es, &mut flood_tries, chat_id).await {
                    continue;
                }
                // v1.10.2: retry on network/TG transient errors
                // (consistent with send_rich_ops_with_retry)
                if is_retryable_error(&es) && transient_tries < SEND_BACKOFF_SECS.len() {
                    let backoff = SEND_BACKOFF_SECS[transient_tries];
                    tracing::warn!(chat_id = chat_id, backoff = backoff, error = %es, "doc send transient error, backing off");
                    tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
                    transient_tries += 1;
                    continue;
                }
                return Err(e.into());
            }
        }
    };
    cache_file_id(db, d, "doc", message_file_id_for(&m, "doc")).await;
    Ok(m)
}

/// Failure of a rich-op send: (failed op index, first delivered message,
/// error). The message is boxed because teloxide's `Message` is large and
/// clippy rejects a ~2KB Err variant; it is needed to keep the caption
/// carrier's link/id after a partial send across retries.
type RichSendFailure = (usize, Option<Box<Message>>, anyhow::Error);

/// Execute ops starting at `resume`. Returns total op count on success, or
/// (ops completed, error) on failure so the caller can resume.
/// On success returns the first sent message (for document messages to quote
/// and for generating message links).
/// Each op internally uses the file_id cache (a hit avoids the upload; a
/// rejected file_id is automatically invalidated and re-uploaded).
/// With multiple groups: the caption goes only on the first group, and
/// follow-up groups quote the first group's message.
// The 8 parameters all share the same push context; packing them into yet
// another struct would be over-engineering.
#[allow(clippy::too_many_arguments)]
async fn send_rich_ops(
    bot: &Bot,
    db: &Db,
    chat_id: i64,
    caption: &str,
    caption_above: bool,
    ops: &[RichOp<'_>],
    transport: &crate::media::MediaTransport,
    resume: usize,
    // When set, the FIRST op replies to this message (used by retweet
    // inline images to quote the main entry message).
    reply_first_to: Option<teloxide::types::MessageId>,
) -> Result<(usize, Option<Message>), RichSendFailure> {
    let mut first_msg: Option<Message> = None;
    for (i, op) in ops.iter().enumerate().skip(resume) {
        // Queue centrally per channel to keep parallel sources from hitting
        // flood control
        pace(chat_id).await;
        // Caption goes only on the first group; follow-up groups quote the
        // first group's message
        let op_caption = if i == 0 { caption } else { "" };
        let reply_to = if i > 0 {
            first_msg.as_ref().map(|m| m.id)
        } else {
            reply_first_to
        };
        match send_op(bot, db, chat_id, op_caption, caption_above, op, transport, reply_to).await {
            Err(e) => return Err((i, first_msg.map(Box::new), anyhow::anyhow!("{:#}", e))),
            Ok(msgs) => {
                if first_msg.is_none() {
                    first_msg = msgs.into_iter().next();
                }
            }
        }
    }
    Ok((ops.len(), first_msg))
}

/// send_rich_ops + bounded backoff with resume on network errors (M3 fix:
/// the main rich_ops and rt_photos sites previously duplicated the same
/// loop verbatim). Only connection-class errors that are "definitely not
/// delivered" are retried, resuming after the last successful op; already
/// delivered parts are never re-sent.
#[allow(clippy::too_many_arguments)]
async fn send_rich_ops_with_retry(
    bot: &Bot,
    db: &Db,
    chat_id: i64,
    caption: &str,
    caption_above: bool,
    ops: &[RichOp<'_>],
    transport: &crate::media::MediaTransport,
    reply_first_to: Option<teloxide::types::MessageId>,
    ctx: &str,
) -> Result<(usize, Option<Message>), RichSendFailure> {
    let mut first_delivered: Option<Box<Message>> = None;
    let mut res = send_rich_ops(bot, db, chat_id, caption, caption_above, ops, transport, 0, reply_first_to).await;
    if let Err((_, first, _)) = &res {
        first_delivered = first.clone();
    }
    for backoff in SEND_BACKOFF_SECS {
        let retry_from = match &res {
            Err((sent, _, e)) if *sent < ops.len() && is_retryable_error(&e.to_string()) => Some(*sent),
            _ => None,
        };
        let Some(sent) = retry_from else { break };
        tracing::warn!(chat_id = chat_id, ctx = ctx, resume = sent, backoff = backoff, "rich send network error, backing off before retry");
        tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
        res = send_rich_ops(bot, db, chat_id, caption, caption_above, ops, transport, sent, reply_first_to).await;
        if first_delivered.is_none() {
            if let Err((_, first, _)) = &res {
                first_delivered = first.clone();
            }
        }
    }
    match res {
        // Keep the message from the first attempt: a resumed successful call
        // only returns the first message of the resumed slice, not the true
        // caption/first-op message delivered before the failure.
        Ok((total, first)) => Ok((total, first_delivered.map(|m| *m).or(first))),
        Err((sent, first, e)) => Err((sent, first_delivered.or(first), e)),
    }
}

/// Send a single op (granularity = one Telegram API call), with built-in
/// file_id cache:
/// on a cache hit use `InputFile::Id` to avoid re-uploading; if Telegram
/// rejects that file_id (bot changed / file cleaned up), invalidate the
/// cache entry and retry once with a fresh upload.
/// Returns all messages produced by the op (multiple for media groups) so
/// the caller can take the first one as the anchor.
/// When `reply_to` is set, the message quotes the specified message
/// (follow-up groups quote the first group).
///
/// D1 convergence: the file_id cache retry + flood backoff skeleton was
/// extracted into send_with_cache_retry; each media-type branch only
/// provides a closure that "builds and sends the request given use_cache".
#[allow(clippy::too_many_arguments)]
async fn send_op(
    bot: &Bot,
    db: &Db,
    chat_id: i64,
    caption: &str,
    caption_above: bool,
    op: &RichOp<'_>,
    transport: &crate::media::MediaTransport,
    reply_to: Option<teloxide::types::MessageId>,
) -> anyhow::Result<Vec<Message>> {
    let chat = ChatId(chat_id);
    // Unified blockquote folding rules (caption is shared across all media
    // types)
    let caption_owned = normalize_blockquote_fold(caption);
    let caption = caption_owned.as_str();
    let reply_params = reply_to.map(teloxide::types::ReplyParameters::new);
    // async move closures require all captures to be Copy/references;
    // reply_params is lifted to a reference.
    let rp = &reply_params;

    match op {
        RichOp::Photo(p) => {
            let m = send_with_cache_retry(db, chat_id, op, |cache_enabled| { let use_cache = cache_enabled; async move {
                let inp = resolve_input(db, p, transport, "photo", use_cache).await?;
                let mut req = bot
                    .send_photo(chat, inp)
                    .caption(caption)
                    .parse_mode(ParseMode::Html)
                    .show_caption_above_media(caption_above);
                if let Some(rp) = rp {
                    req = req.reply_parameters(rp.clone());
                }
                req.await.map_err(|e| anyhow::anyhow!(e))
            }}).await?;
            cache_file_id(db, p, "photo", message_file_id_for(&m, "photo")).await;
            Ok(vec![m])
        }
        RichOp::Gif(g) => {
            let m = send_with_cache_retry(db, chat_id, op, |cache_enabled| { let use_cache = cache_enabled; async move {
                let inp = resolve_input(db, g, transport, "gif", use_cache).await?;
                let mut req = bot
                    .send_animation(chat, inp)
                    .caption(caption)
                    .parse_mode(ParseMode::Html)
                    .show_caption_above_media(caption_above);
                if let Some(rp) = rp {
                    req = req.reply_parameters(rp.clone());
                }
                req.await.map_err(|e| anyhow::anyhow!(e))
            }}).await?;
            cache_file_id(db, g, "gif", message_file_id_for(&m, "gif")).await;
            Ok(vec![m])
        }
        RichOp::Video(v) => {
            let m = send_with_cache_retry(db, chat_id, op, |cache_enabled| { let use_cache = cache_enabled; async move {
                let inp = resolve_input(db, v, transport, "video", use_cache).await?;
                let mut req = bot
                    .send_video(chat, inp)
                    .caption(caption)
                    .parse_mode(ParseMode::Html)
                    .show_caption_above_media(caption_above)
                    .supports_streaming(true);
                if let Some(d) = v.duration_secs {
                    req = req.duration(d);
                }
                if let Some(tp) = &v.thumb_path {
                    if let Ok(tin) = path_input(tp, transport).await {
                        req = req.thumbnail(tin);
                    }
                }
                if let Some(rp) = rp {
                    req = req.reply_parameters(rp.clone());
                }
                req.await.map_err(|e| anyhow::anyhow!(e))
            }}).await?;
            cache_file_id(db, v, "video", message_file_id_for(&m, "video")).await;
            Ok(vec![m])
        }
        RichOp::Group(items) => {
            let msgs = send_with_cache_retry(db, chat_id, op, |cache_enabled| { let use_cache = cache_enabled; async move {
                let group = build_group_inputs(db, items, transport, caption, caption_above, use_cache).await?;
                let mut req = bot.send_media_group(chat, group);
                if let Some(rp) = rp {
                    req = req.reply_parameters(rp.clone());
                }
                req.await.map_err(|e| anyhow::anyhow!(e))
            }}).await?;
            // Write cache: returned messages correspond to items in order
            for (m, item) in msgs.iter().zip(items.iter()) {
                let mode = group_item_mode(item);
                cache_file_id(db, item, mode, message_file_id_for(m, mode)).await;
            }
            Ok(msgs)
        }
    }
}

/// Generic skeleton for file_id cache retry + flood backoff (D1 convergence).
/// `attempt(use_cache)` builds and sends the request; when a file_id is
/// rejected the cache is invalidated and it retries once with caching off;
/// on flood 429 it backs off as Telegram requires, then retries.
async fn send_with_cache_retry<T, F, Fut>(
    db: &Db,
    chat_id: i64,
    op: &RichOp<'_>,
    mut attempt: F,
) -> anyhow::Result<T>
where
    F: FnMut(bool) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let mut use_cache = true;
    let mut flood_tries = 0u32;
    loop {
        match attempt(use_cache).await {
            Ok(v) => return Ok(v),
            Err(e) if use_cache && is_file_id_error(&e.to_string()) => {
                invalidate_op_cache(db, op).await;
                pace(chat_id).await;
                use_cache = false;
            }
            Err(e) => {
                let es = e.to_string();
                if flood_backoff(&es, &mut flood_tries, chat_id).await {
                    continue;
                }
                return Err(e);
            }
        }
    }
}

/// Build the InputMedia list for a media group. When `use_cache=true` the
/// cached file_id is preferred.
async fn build_group_inputs(
    db: &Db,
    items: &[&MediaItem],
    transport: &crate::media::MediaTransport,
    caption: &str,
    caption_above: bool,
    use_cache: bool,
) -> anyhow::Result<Vec<InputMedia>> {
    let mut group: Vec<InputMedia> = Vec::new();
    for (j, m) in items.iter().enumerate() {
        let mode = group_item_mode(m);
        let inp = resolve_input(db, m, transport, mode, use_cache).await?;
        match m.kind {
            MediaKind::Video => {
                let mut im = InputMediaVideo::new(inp);
                im.supports_streaming = Some(true);
                if j == 0 {
                    im = im.caption(caption.to_string()).parse_mode(ParseMode::Html);
                }
                // Telegram requires show_caption_above_media to be
                // the same for ALL items in a media group.
                im.show_caption_above_media = caption_above;
                // Same as send_video: explicitly attach thumbnail/duration
                // so the preview is shown
                if let Some(d) = m.duration_secs {
                    im.duration = Some(d.min(65535) as u16);
                }
                if let Some(tp) = &m.thumb_path {
                    if let Ok(tin) = path_input(tp, transport).await {
                        im.thumbnail = Some(tin);
                    }
                }
                group.push(InputMedia::Video(im));
            }
            // Note: GIFs are already separated into independent ops
            // (sendAnimation) in build_rich_ops and never enter a media
            // group — Telegram's sendMediaGroup does not accept the
            // animation type. Here everything is treated uniformly as a
            // photo as a defensive fallback.
            _ => {
                let mut im = InputMediaPhoto::new(inp);
                if j == 0 {
                    im = im.caption(caption.to_string()).parse_mode(ParseMode::Html);
                }
                im.show_caption_above_media = caption_above;
                group.push(InputMedia::Photo(im));
            }
        }
    }
    Ok(group)
}

/// Send an entry and return the FIRST sent message (used to build the
/// Telegram message link for owner failure notices).
/// When split into multiple messages, each subsequent message replies to the
/// previous one for easy navigation. The footer is placed on the last message.
///
/// Resume-aware retry: if a chunk fails, only that chunk is retried (bounded
/// backoff) — earlier chunks are already delivered and are never re-sent.
/// This mirrors the media path's op-level resume and prevents the scheduler's
/// coarse whole-entry retry from duplicating already-sent chunks.
pub async fn send_entry_msg(
    bot: &Bot,
    chat_id: i64,
    html_text: &str,
    footer: &str,
    link_preview: bool,
) -> anyhow::Result<Message> {
    // The last chunk gets the footer (source line) appended, so reserve the
    // footer's visible length in the split budget up front, to avoid the
    // combined text exceeding Telegram's 4096 limit and failing to send.
    // +2 accounts for the "\n\n" separator (visible chars) used when
    // joining, preventing an extreme edge case from going over 4096.
    // No longer capped at 500: with very long source titles an insufficient
    // reserve would make last-chunk+footer deterministically exceed 4096.
    // If the footer itself exceeds SAFE_LIMIT it is truncated (cannot happen
    // in practice; purely defensive).
    let footer: String = if visible_len(footer) > SAFE_LIMIT - 2 {
        // HTML-aware: a raw UTF-16 cut can split an <a>/<i> tag or entity.
        let (cut, _) = truncate_html_keep_links(footer, SAFE_LIMIT - 3);
        format!("{cut}…")
    } else {
        footer.to_string()
    };
    let footer_reserve = visible_len(&footer) + 2;
    let chunks = split_html(html_text, SAFE_LIMIT.saturating_sub(footer_reserve).max(1));
    // Defensive: split_html does not contractually guarantee a non-empty Vec
    // (a pathological input whose visible_len>limit but whose every \n\n
    // segment is empty could yield none). Guard against the `len()-1` underflow
    // and the `expect("at least one chunk")` panic below by sending a single
    // message instead — never silently drop the entry.
    let chunks = if chunks.is_empty() {
        vec![html_text.to_string()]
    } else {
        chunks
    };
    let mut first_msg: Option<Message> = None;
    let last_idx = chunks.len() - 1;
    for (i, chunk) in chunks.iter().enumerate() {
        // Append footer to the last chunk only.
        let text = if i == last_idx && !footer.is_empty() {
            format!("{}\n\n{}", chunk, footer)
        } else {
            chunk.clone()
        };
        // Only the first chunk gets link preview (avoid duplicate previews).
        let lp = if i == 0 { link_preview } else { false };
        // Chunks uniformly quote the first message (flat structure, not
        // chained).
        let reply_to = first_msg.as_ref().map(|m| m.id);
        // Chunk-level bounded backoff retry: on failure only the current
        // chunk is re-sent.
        let m = send_chunk_with_retry(bot, chat_id, &text, lp, reply_to).await?;
        if first_msg.is_none() {
            first_msg = Some(m.clone());
        }
        // Small delay between chunks to respect rate limits.
        if i < last_idx {
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        }
    }
    Ok(first_msg.expect("at least one chunk"))
}

/// Send a single HTML message (must already fit within Telegram's limit).
/// Handles flood-control retry and HTML parse fallback.
/// If `reply_to` is set, the message replies to that message id.
async fn send_single_html(
    bot: &Bot,
    chat_id: i64,
    html_text: &str,
    link_preview: bool,
    reply_to: Option<teloxide::types::MessageId>,
) -> anyhow::Result<Message> {
    // Flood backoff capped at 2 retries (consistent with the media path's
    // flood_backoff).
    const MAX_RETRIES: u32 = 2;
    // Unified blockquote folding rules (fold when >200 chars).
    let html_text = normalize_blockquote_fold(html_text);
    let lpo = if link_preview { None } else { Some(no_preview()) };

    for attempt in 0..=MAX_RETRIES {
        // Queue centrally per channel (including flood retries) so parallel
        // sources don't hit rate limits
        pace(chat_id).await;
        let mut req = bot
            .send_message(ChatId(chat_id), html_text.as_str())
            .parse_mode(ParseMode::Html);
        if let Some(o) = &lpo {
            req = req.link_preview_options(o.clone());
        }
        if let Some(rid) = reply_to {
            req = req.reply_parameters(teloxide::types::ReplyParameters::new(rid));
        }
        let result = req.await;

        match result {
            Ok(m) => return Ok(m),
            Err(e) => {
                let err_str = e.to_string();
                // Flood control: wait the requested time and retry.
                if let Some(secs) = flood_retry_secs(&err_str) {
                    if attempt < MAX_RETRIES {
                        tracing::warn!(chat_id = chat_id, secs = secs, attempt = attempt, "flood control, backing off");
                        tokio::time::sleep(std::time::Duration::from_secs(secs + 1)).await;
                        continue;
                    }
                    return Err(e.into());
                }
                // If Telegram can't parse HTML entities, fallback to plain text
                if err_str.contains("parse entities") || err_str.contains("Can't parse") {
                    // Stripping all formatting is a user-visible degradation;
                    // leave a trace so we can track down which HTML triggered
                    // it.
                    tracing::warn!(
                        chat_id = chat_id,
                        error = %err_str,
                        html_head = %html_text.chars().take(100).collect::<String>(),
                        "HTML parse failed, falling back to plain text"
                    );
                    let plain = strip_html_tags(&html_text);
                    let mut req2 = bot.send_message(ChatId(chat_id), &plain);
                    if let Some(o) = &lpo {
                        req2 = req2.link_preview_options(o.clone());
                    }
                    if let Some(rid) = reply_to {
                        req2 = req2.reply_parameters(teloxide::types::ReplyParameters::new(rid));
                    }
                    let m = req2.await?;
                    return Ok(m);
                }
                return Err(e.into());
            }
        }
    }
    unreachable!()
}

/// Send one already-split HTML chunk with bounded network/transient-TG
/// backoff. Flood-control retry lives inside send_single_html; this layer
/// covers the connection-class retry shared by the text-chunk and
/// caption-overflow paths (the latter previously had no retry at all).
async fn send_chunk_with_retry(
    bot: &Bot,
    chat_id: i64,
    text: &str,
    link_preview: bool,
    reply_to: Option<teloxide::types::MessageId>,
) -> anyhow::Result<Message> {
    let mut send_res = send_single_html(bot, chat_id, text, link_preview, reply_to).await;
    for backoff in SEND_BACKOFF_SECS {
        if !matches!(&send_res, Err(e) if is_retryable_error(&e.to_string())) {
            break;
        }
        tracing::warn!(chat_id = chat_id, backoff = backoff, "text chunk send failed, retrying");
        tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
        send_res = send_single_html(bot, chat_id, text, link_preview, reply_to).await;
    }
    send_res
}

/// Build a t.me link for a message in a channel/supergroup (best effort).
pub fn tg_message_link(msg: &Message) -> Option<String> {
    let cid = msg.chat.id.0;
    // Channels / supergroups have a public t.me/c/<id>/<msg_id> link using the
    // id without the -100 prefix. Negative IDs without the -100 prefix
    // (regular group chats etc.) have no public link.
    let bare = if cid < 0 {
        let s = cid.to_string();
        let x = s.strip_prefix("-100")?;
        x.to_string()
    } else {
        return None;
    };
    Some(format!("https://t.me/c/{}/{}", bare, msg.id.0))
}

// Escape HTML special characters / attribute escaping / chat-id & title
// formatting / plain-text stripping now live in `crate::htmltext`
// (re-exported above for external callers).

#[cfg(test)]
mod tests {
    use super::*;

    // ===== extract_poll =====

    /// Build an EntryMeta containing a poll (simulating what weibo.rs
    /// produces).
    fn meta_with_poll(question: &str, options: Vec<&str>, is_multi: bool) -> crate::entrymeta::EntryMeta {
        crate::entrymeta::EntryMeta {
            prerendered_html: false,
            poll: Some(crate::entrymeta::PollData {
                question: question.to_string(),
                options: options.into_iter().map(|s| s.to_string()).collect(),
                is_multi_select: is_multi,
            }),
            retweet_images: Vec::new(),
        }
    }

    #[test]
    fn extract_poll_basic() {
        let meta = meta_with_poll("你搬过家吗？", vec!["A：没搬过", "B：搬过"], false);
        let (q, opts, multi) = extract_poll(Some(&meta)).unwrap();
        assert_eq!(q, "你搬过家吗？");
        assert_eq!(opts, vec!["A：没搬过", "B：搬过"]);
        assert!(!multi);
    }

    #[test]
    fn extract_poll_multi_select() {
        let meta = meta_with_poll("多选", vec!["甲", "乙", "丙"], true);
        let (_, opts, multi) = extract_poll(Some(&meta)).unwrap();
        assert!(multi);
        assert_eq!(opts.len(), 3);
    }

    #[test]
    fn extract_poll_truncates_long_question_and_options() {
        // question >300 chars is truncated, option >100 chars is truncated
        let long_q = "问".repeat(310);
        let long_opt = "选".repeat(110);
        let meta = meta_with_poll(&long_q, vec![&long_opt, "短"], false);
        let (q, opts, _) = extract_poll(Some(&meta)).unwrap();
        assert!(q.chars().count() <= 300, "question too long: {}", q.chars().count());
        assert!(q.ends_with("..."));
        assert!(opts[0].chars().count() <= 100, "option too long: {}", opts[0].chars().count());
        assert!(opts[0].ends_with("..."));
    }

    #[test]
    fn extract_poll_caps_at_10_options() {
        // 12 options → only the first 10 are kept (Telegram's cap)
        let opts: Vec<&str> = (0..12).map(|_| "选项").collect();
        let meta = meta_with_poll("多选", opts, false);
        let (_, options, _) = extract_poll(Some(&meta)).unwrap();
        assert_eq!(options.len(), 10);
    }

    #[test]
    fn extract_poll_rejects_single_option() {
        // Only 1 valid option → does not meet Telegram's minimum of 2 items
        let meta = meta_with_poll("问题", vec!["唯一"], false);
        assert!(extract_poll(Some(&meta)).is_none());
    }

    #[test]
    fn extract_poll_no_meta() {
        // No meta (RSS feed) → no poll
        assert!(extract_poll(None).is_none());
    }

    #[test]
    fn extract_poll_meta_without_poll() {
        // meta exists but has no poll field
        let meta = crate::entrymeta::EntryMeta::default();
        assert!(extract_poll(Some(&meta)).is_none());
    }

    // ===== entry_summary hyperlink handling =====

    fn rss_entry_with_body(body: &str) -> Entry {
        crate::platforms::make_entry(
            "test-id".to_string(),
            "测试标题".to_string(),
            String::new(),
            body.to_string(),
            "text/html",
            "https://example.com/post".to_string(),
            None,
        )
    }

    #[test]
    fn entry_summary_keeps_wechat_account_link_clickable() {
        // huxiu (RSSHub) real shape: the WeChat account name sits inside an
        // <a> right after "本文来自微信公众号：". The summary must keep both
        // the anchor text and the hyperlink (the account name alone was the
        // previous fix; the href was dropped along with it).
        let body = "<p>本文来自微信公众号：<a href=\"https://mp.weixin.qq.com/s?x=1&amp;y=2\"> 懒熊体育 </a>，作者：黄怡 苏洋</p><p>正文开始，第二段内容。</p>";
        let summary = entry_summary(&rss_entry_with_body(body), 140, None);
        assert!(
            summary.contains("<a href=\"https://mp.weixin.qq.com/s?x=1&amp;y=2\">懒熊体育</a>"),
            "hyperlink lost: {summary}"
        );
        assert!(summary.contains("本文来自微信公众号："), "lead lost: {summary}");
        // The account name stays in its sentence position (anchor text must
        // not drift away from the surrounding words).
        assert!(
            summary.contains("微信公众号：<a href=\"https://mp.weixin.qq.com/s?x=1&amp;y=2\">懒熊体育</a>，作者"),
            "anchor misplaced: {summary}"
        );
        assert!(summary.contains("正文开始"), "body lost: {summary}");
    }

    #[test]
    fn entry_summary_closes_anchor_when_cut_inside_link() {
        // Truncation inside the anchor text must re-close <a> so the summary
        // stays valid Telegram HTML, with the ellipsis outside the link.
        let long_label = "长".repeat(60);
        let body = format!(
            "<p>前文。<a href=\"https://mp.weixin.qq.com/s/abc\">{long_label}</a>后续内容。</p>"
        );
        let summary = entry_summary(&rss_entry_with_body(&body), 20, None);
        assert!(summary.ends_with("</a>......"), "unbalanced or misplaced suffix: {summary}");
        assert_eq!(summary.matches("<a ").count(), 1, "unbalanced anchor: {summary}");
        assert_eq!(summary.matches("</a>").count(), 1, "unbalanced anchor: {summary}");
        assert!(summary.starts_with("前文。"), "lead lost: {summary}");
    }

    #[test]
    fn entry_summary_drops_anchor_without_visible_text_in_budget() {
        // An <a> whose text starts past the cut point must not appear as an
        // empty hyperlink.
        let body = "<p>正文内容比较长按 here</p><p><a href=\"https://mp.weixin.qq.com/s/x\">后面的链接</a></p>";
        let summary = entry_summary(&rss_entry_with_body(body), 6, None);
        assert!(!summary.contains("<a"), "empty/dangling anchor emitted: {summary}");
        assert!(!summary.contains("</a>"), "orphan close emitted: {summary}");
        assert!(summary.ends_with("......"), "truncation marker missing: {summary}");
    }

    #[test]
    fn entry_summary_never_splits_entity_reference() {
        // Text entities survive the body pipeline as references. The cut
        // counts each reference as one display character and must emit it
        // whole: a split "&am" makes Telegram reject the message with a
        // parse-entities error.
        let body = "<p>成本&lt;100&amp;更多&gt;一些文字继续更多内容截断部分。</p>";
        // Budget ends exactly after the "&amp;" reference.
        let summary = entry_summary(&rss_entry_with_body(body), 7, None);
        assert_eq!(summary, "成本&lt;100&amp;......", "entity cut wrong: {summary}");
        // Budget ends just before it: nothing of the reference may appear.
        let summary = entry_summary(&rss_entry_with_body(body), 6, None);
        assert_eq!(summary, "成本&lt;100......", "entity cut wrong: {summary}");
    }

    #[test]
    fn entry_summary_still_drops_media_only_anchors() {
        // Media links (href is a media resource) were removed upstream by
        // html_to_telegram_html before the summary cut, so the summary
        // keeps no video page URL or its card text.
        let body = "<p>正文。</p><a href=\"https://video.weibo.com/show?fid=1034:1\">微博视频</a>";
        let summary = entry_summary(&rss_entry_with_body(body), 140, None);
        assert!(summary.contains("正文。"), "body lost: {summary}");
        assert!(!summary.contains("video.weibo.com"), "media href leaked: {summary}");
        assert!(!summary.contains("<a"), "media anchor survived: {summary}");
    }

    // ===== build_rich_ops (album pool: videos + images in one album) =====

    /// Minimal MediaItem factory for op-assembly tests: only `kind`/`is_gif`
    /// drive build_rich_ops, and `content_hash` labels each item so tests can
    /// assert the resulting order.
    fn mk_item(hash: &str, kind: MediaKind, is_gif: bool) -> MediaItem {
        MediaItem {
            path: std::path::PathBuf::from(format!("{hash}.bin")),
            kind,
            width: 0,
            height: 0,
            size: 0,
            is_gif,
            thumb_path: None,
            duration_secs: None,
            content_hash: hash.to_string(),
            is_retweet: false,
        }
    }

    /// Flatten the ops into an ordered `(label, group?)` list for assertions:
    /// each op becomes one entry, and album-group members are joined by "+"
    /// so their internal order is checked too.
    fn op_labels(ops: &[RichOp<'_>]) -> Vec<String> {
        ops.iter()
            .map(|op| match op {
                RichOp::Photo(p) => format!("photo:{}", p.content_hash),
                RichOp::Gif(g) => format!("gif:{}", g.content_hash),
                RichOp::Video(v) => format!("video:{}", v.content_hash),
                RichOp::Group(items) => {
                    let parts: Vec<String> = items
                        .iter()
                        .map(|m| m.content_hash.to_string())
                        .collect();
                    format!("group[{}]", parts.join("+"))
                }
            })
            .collect()
    }

    #[test]
    fn rich_ops_merge_one_video_one_image_into_single_album() {
        // The core of this change: a video and an image must land in the SAME
        // first album (video leading), not "video msg + image quoting it".
        let v = mk_item("v1", MediaKind::Video, false);
        let p = mk_item("p1", MediaKind::Image, false);
        let ops = build_rich_ops(&[&p], &[], &[&v]);
        assert_eq!(op_labels(&ops), vec!["group[v1+p1]"]);
    }

    #[test]
    fn rich_ops_video_leads_image_within_merged_album() {
        // Ordering contract: all videos precede all images inside the pool.
        let v = mk_item("v1", MediaKind::Video, false);
        let p1 = mk_item("p1", MediaKind::Image, false);
        let p2 = mk_item("p2", MediaKind::Image, false);
        let ops = build_rich_ops(&[&p1, &p2], &[], &[&v]);
        assert_eq!(op_labels(&ops), vec!["group[v1+p1+p2]"]);
    }

    #[test]
    fn rich_ops_single_item_falls_back_to_direct_send() {
        // sendMediaGroup requires 2–10 items, so a lone media goes through
        // its dedicated single op, dispatched by kind.
        let v = mk_item("v1", MediaKind::Video, false);
        assert_eq!(op_labels(&build_rich_ops(&[], &[], &[&v])), vec!["video:v1"]);
        let p = mk_item("p1", MediaKind::Image, false);
        assert_eq!(op_labels(&build_rich_ops(&[&p], &[], &[])), vec!["photo:p1"]);
    }

    #[test]
    fn rich_ops_gifs_are_standalone_and_follow_album() {
        // GIFs cannot enter an album and must be emitted after it.
        let v = mk_item("v1", MediaKind::Video, false);
        let p = mk_item("p1", MediaKind::Image, false);
        let g1 = mk_item("g1", MediaKind::Image, true);
        let g2 = mk_item("g2", MediaKind::Image, true);
        let ops = build_rich_ops(&[&p], &[&g1, &g2], &[&v]);
        assert_eq!(
            op_labels(&ops),
            vec!["group[v1+p1]", "gif:g1", "gif:g2"]
        );
    }

    #[test]
    fn rich_ops_overflow_splits_into_follow_up_albums() {
        // 12 media > the 10-per-album cap: first album fills to 10, the
        // remainder becomes a follow-up album (which the sender replies to
        // the first). Videos lead, then images fill the same pool.
        let videos: Vec<MediaItem> =
            (0..3).map(|i| mk_item(&format!("v{i}"), MediaKind::Video, false)).collect();
        let photos: Vec<MediaItem> =
            (0..9).map(|i| mk_item(&format!("p{i}"), MediaKind::Image, false)).collect();
        let vrefs: Vec<&MediaItem> = videos.iter().collect();
        let prefs: Vec<&MediaItem> = photos.iter().collect();
        let ops = build_rich_ops(&prefs, &[], &vrefs);
        assert_eq!(ops.len(), 2);
        assert_eq!(
            op_labels(&ops),
            vec![
                "group[v0+v1+v2+p0+p1+p2+p3+p4+p5+p6]",
                "group[p7+p8]",
            ]
        );
    }

    #[test]
    fn rich_ops_title_summary_slice_still_yields_single_photo() {
        // Compatibility guard: TitleSummary keeps only the first image, so
        // the pool is a lone image and must not become an album.
        let v = mk_item("v1", MediaKind::Video, false);
        let p = mk_item("p1", MediaKind::Image, false);
        let media = [v.clone(), p.clone()];
        let sliced: &[MediaItem] = match media.iter().position(|m| m.kind == MediaKind::Image) {
            Some(i) => &media[i..i + 1],
            None => &media,
        };
        let prefs: Vec<&MediaItem> = sliced
            .iter()
            .filter(|m| m.kind == MediaKind::Image && !m.is_gif)
            .collect();
        let vrefs: Vec<&MediaItem> = sliced.iter().filter(|m| m.kind == MediaKind::Video).collect();
        let ops = build_rich_ops(&prefs, &[], &vrefs);
        assert_eq!(op_labels(&ops), vec!["photo:p1"]);
    }

    #[test]
    fn thumbnail_errors_are_detected() {
        assert!(is_thumbnail_error("Bad Request: PHOTO_INVALID_DIMENSIONS"));
        assert!(is_thumbnail_error("Bad Request: wrong file dimensions"));
        assert!(is_thumbnail_error("Bad Request: THUMBNAIL_DIMENSIONS_INVALID"));
        assert!(!is_thumbnail_error("Bad Request: message is too long"));
    }

    #[test]
    fn content_mode_accepts_legacy_camel_case_defaults() {
        assert_eq!(ContentMode::from_str("TitleSummary"), ContentMode::TitleSummary);
        assert_eq!(ContentMode::from_str("TitleContent"), ContentMode::TitleContent);
        assert_eq!(ContentMode::from_str("content_only"), ContentMode::ContentOnly);
        assert_eq!(ContentMode::from_str("MediaOnly"), ContentMode::MediaOnly);
        assert_eq!(ContentMode::from_str("unknown"), ContentMode::TitleLink);
    }
}
