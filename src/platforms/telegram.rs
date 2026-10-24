//! Telegram public-channel native fetcher: scrapes the `t.me/s/<channel>`
//! web preview (server-rendered HTML, no JS, no login required). Mirrors
//! RSSHub's telegram/channel route.
//!
//! Subscription URL format: `https://t.me/<channel>` (telegram.me and the
//! `/s/<channel>` form are also accepted). Only PUBLIC channels have a web
//! preview; private channels cannot be fetched.
//!
//! Media: photos appear as `background-image:url(...)` on the photo wrap;
//! videos as `<video src=...>`. Both are rewritten to `<img>`/`<video>` tags
//! in the entry body so the existing media pipeline picks them up.

use std::sync::LazyLock;

use anyhow::Context;
use chrono::{DateTime, Utc};
use feed_rs::model::Entry;

use crate::i18n::{self, Lang};
use crate::state::AppState;

// ---------------------------------------------------------------------------
// URL detection
// ---------------------------------------------------------------------------

/// True when the feed URL is a Telegram channel that should be fetched
/// natively via the web preview.
pub fn is_telegram_url(url: &str) -> bool {
    parse_channel_name(url).is_some()
}

/// Parse the channel username from a Telegram channel URL:
/// `https://t.me/<name>`, `https://telegram.me/<name>`, and the
/// `https://t.me/s/<name>` preview form. Reserved paths and invite links
/// (`t.me/+xxx`) are rejected.
pub fn parse_channel_name(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?.to_lowercase();
    if !matches!(host.as_str(), "t.me" | "telegram.me") {
        return None;
    }
    let segs: Vec<&str> = u.path_segments()?.filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return None;
    }
    // The /s/<name> preview form.
    let name = if segs[0] == "s" {
        *segs.get(1)?
    } else {
        segs[0]
    };
    // Reserved t.me paths: preview prefix, invite/share/proxy/gift/theme/
    // sticker/chatlist routes, login pages, etc. Without a slug here, a link
    // like `t.me/joinchat/ABCDEF` parses "joinchat" as a channel username
    // (8 chars, all valid) and 404s forever after subscribing, accumulating
    // fetch errors.
    const RESERVED: &[&str] = &[
        "s",
        "i",
        "c",
        "joinchat",
        "share",
        "contact",
        "login",
        "setlanguage",
        "confirmphone",
        "addstickers",
        "addemoji",
        "addtheme",
        "addlist",
        "proxy",
        "socks",
        "boost",
        "gift",
        "gifts",
        "username",
    ];
    if RESERVED.contains(&name) {
        return None;
    }
    // Invite links (+abc) are private channels — no web preview.
    if name.starts_with('+') {
        return None;
    }
    // Channel usernames: 5-32 chars, alphanumeric + underscore.
    if name.len() < 5 || name.len() > 32 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(name.to_string())
}

// ---------------------------------------------------------------------------
// Static regexes
// ---------------------------------------------------------------------------

/// Channel display name from the info header.
static RE_CHAN_TITLE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?is)tgme_channel_info_header_title[^>]*>.*?<span[^>]*>(.*?)</span>"#).unwrap()
});
/// Opening tag carrying the data-post attribute (used to locate block boundaries).
static RE_DATA_POST: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"data-post="([^"]+)""#).unwrap()
});
/// Message text container opening tag (main text only, not reply text).
/// Captures nothing: the inner HTML is extracted by a balanced-div position
/// scan (see `extract_main_text_html`), because the body can contain nested
/// `</div>` (blockquote / code blocks) that a non-greedy `(.*?)</div>` regex
/// would truncate at, silently cutting the message (LESSONS: non-greedy
/// truncation).
/// The reply text div is class `tgme_widget_message_text js-message_reply_text`
/// and cannot match: the pattern requires the literal `js-message_text`,
/// absent from `js-message_reply_text` (no `js-message_` immediately precedes
/// the reply's `_text` suffix).
static RE_MSG_TEXT_OPEN: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?is)<div[^>]*tgme_widget_message_text js-message_text[^>]*>"#).unwrap()
});
/// Reply block: `<a class="tgme_widget_message_reply" href="...">...</a>`.
/// Captures: [1]=href. Inner content is extracted by position-based scanning
/// (see extract_reply_blockquote) to handle nested `<a>` hyperlinks in the
/// reply text — a non-greedy `(.*?)</a>` regex would truncate at the first
/// inner `</a>`.
static RE_REPLY_BLOCK: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?is)<a[^>]*tgme_widget_message_reply[^>]*href="([^"]*)"[^>]*>"#).unwrap()
});
/// Reply author name.
static RE_REPLY_AUTHOR: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?is)tgme_widget_message_author_name[^>]*>(.*?)</span>"#).unwrap()
});
/// Reply text container opening tag. Captures nothing: inner HTML is
/// extracted by the balanced-div scan (`extract_balanced_div_inner`) since
/// quoted content may itself contain nested `</div>`.
static RE_REPLY_TEXT_OPEN: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?is)<div[^>]*js-message_reply_text[^>]*>"#).unwrap()
});
/// Photo: background-image url on the photo wrap.
/// Anchored inside the `tgme_widget_message_photo_wrap` tag: the page also
/// uses background-image for video posters (video_thumb), reply message
/// thumbnails (reply_thumb) and link preview cards; without the restriction
/// those would be mistakenly extracted as body images.
static RE_PHOTO_BG: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?is)<a\b[^>]*tgme_widget_message_photo_wrap[^>]*background-image:url\('([^']+)'\)"#).unwrap()
});
/// Video: `<video src=...>`.
static RE_VIDEO_SRC: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?is)<video\b[^>]*?\bsrc\s*=\s*["']([^"']+)["']"#).unwrap()
});
/// Non-text message containers. The web preview has no `js-message_text`
/// div and no photo/video tag for these, so without a fallback the message
/// would be dropped silently (see message_to_entry).
static RE_MSG_POLL: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)tgme_widget_message_poll").unwrap());
static RE_MSG_DOCUMENT: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)tgme_widget_message_document").unwrap());
static RE_MSG_AUDIO: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)tgme_widget_message_(?:audio|voice)").unwrap());
static RE_MSG_STICKER: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)tgme_widget_message_sticker").unwrap());
/// Document file name shown in the preview (optional).
static RE_DOC_TITLE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?is)tgme_widget_message_document_title[^>]*>(.*?)</span>"#).unwrap()
});
/// Timestamp of the message link. Used only relative to an anchor position
/// (see `extract_published`) — never as "first datetime in the block", since
/// the reply block precedes the main text and embeds the quoted message's own
/// `<time datetime>`.
static RE_DATETIME: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"datetime="([^"]+)""#).unwrap()
});
/// Main message date container: `<div class="tgme_widget_message_date">`
/// wrapping the message's own `<time datetime=...>`. Any tag name with this
/// class qualifies; matched generically so a markup tweak (div→span) still
/// anchors correctly.
static RE_DATE_CONTAINER: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?is)<[a-z]+[^>]*tgme_widget_message_date[^>]*>"#).unwrap()
});
/// Anchor / div open-close tags for the balanced position scanners below.
/// `\b` after the tag name avoids matching `<divider>`-style names.
static RE_ANCHOR_OPEN: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)<a\b").unwrap());
static RE_ANCHOR_CLOSE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)</a\s*>").unwrap());
static RE_DIV_OPEN: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)<div\b").unwrap());
static RE_DIV_CLOSE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)</div\s*>").unwrap());

// ---------------------------------------------------------------------------
// Main fetch entry point
// ---------------------------------------------------------------------------

/// Registry fetch entry: parse the channel name from the full feed URL, then
/// delegate to [`fetch_telegram_channel`]. Called via `PlatformDef.fetch`
/// (URL passed owned; public web preview needs no cookie, so no cookie
/// state is consulted).
pub async fn fetch_by_url(
    state: &AppState,
    url: String,
    _known_sent: Option<&std::collections::HashSet<String>>,
    lang: Lang,
) -> anyhow::Result<crate::fetcher::FetchResult> {
    let channel = parse_channel_name(&url)
        .ok_or_else(|| anyhow::anyhow!("Invalid Telegram channel URL: {}", url))?;
    fetch_telegram_channel(state, &channel, lang).await
}

/// Fetch a Telegram public channel's web preview and convert to FetchResult.
pub async fn fetch_telegram_channel(
    state: &AppState,
    channel: &str,
    lang: Lang,
) -> anyhow::Result<crate::fetcher::FetchResult> {
    let url = format!("https://t.me/s/{}", channel);
    let client = state.client_for_url(&url);
    let resp = client
        .get(&url)
        .header("Accept", "text/html,application/xhtml+xml")
        .send()
        .await
        .with_context(|| format!("Telegram channel request failed: {}", url))?;

    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("Telegram channel does not exist or is private (no public web preview): @{}", channel);
    }
    if !status.is_success() {
        anyhow::bail!("Telegram channel preview HTTP {}", status);
    }
    // Response body size cap (20MB): shares read_body_capped with the RSS
    // fetch path (M8 fix: the two former call sites repeated the same
    // bytes_stream loop verbatim). Streamed read; aborts as soon as the
    // limit is exceeded.
    let ctx = format!("Telegram channel @{}", channel);
    let body_buf = crate::fetcher::read_body_capped(resp, &ctx).await?;
    let html = String::from_utf8_lossy(&body_buf).into_owned();

    // Channel title extraction: a missing title is not fatal (we fall back to
    // the channel slug) but IS a structural signal — the info header has been
    // a stable part of the t.me/s/<channel> layout for years, so its absence
    // usually means the preview page was redesigned. Log it so operators can
    // correlate "titles all became @slug" reports with a specific fetch round.
    let feed_title = match extract_channel_title(&html) {
        Some(t) => t,
        None => {
            tracing::warn!(
                channel = %channel,
                body_len = body_buf.len(),
                "telegram channel title not found in preview page (falling back to channel slug; page structure may have changed)"
            );
            channel.to_string()
        }
    };

    // Locate message blocks by finding all data-post positions and slicing
    // between consecutive ones (Rust regex has no lookahead).
    let blocks = split_message_blocks(&html);

    let mut entries: Vec<Entry> = Vec::new();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (post, block) in &blocks {
        if let Some(e) = message_to_entry(post, block, &feed_title, lang) {
            // Dedup by entry id within the batch, matching the Weibo/X
            // convention (fix for #8)
            if !e.id.is_empty() && seen_ids.insert(e.id.clone()) {
                entries.push(e);
            }
        }
    }

    // Per-round parse summary (aligned with weibo.rs / x.rs debug counters):
    // lets an operator eyeball "blocks found but zero entries" vs "zero
    // blocks at all" without needing to reproduce the fetch locally. Debug
    // level — the warn below covers the anomalous case.
    tracing::debug!(
        channel = %channel,
        blocks = blocks.len(),
        entries = entries.len(),
        "telegram preview parsed"
    );

    // Newest first.
    crate::platforms::sort_entries_newest_first(&mut entries);

    // The page contains message blocks yet not a single entry could be
    // parsed: most likely the t.me preview page structure was redesigned.
    // Must not silently return empty — the scheduler would treat it as
    // success, reset the error count, and the feed would silently stop
    // updating with zero logs.
    if entries.is_empty() && !blocks.is_empty() {
        tracing::warn!(
            channel = %channel,
            posts = blocks.len(),
            "telegram preview has message blocks but zero entries extracted (page structure may have changed)"
        );
    }

    Ok(crate::fetcher::FetchResult::platform(Some(feed_title), entries))
}

// ---------------------------------------------------------------------------
// HTML parsing helpers
// ---------------------------------------------------------------------------

/// Split a t.me/s/<channel> preview page into per-message blocks.
///
/// Locates every `data-post="channel/msgid"` occurrence (Rust regex has no
/// lookahead, so boundaries are derived from consecutive positions) and
/// slices between them. Only actual message posts (`channel/msgid` format,
/// i.e. containing '/') are counted. Returns (post_id, block_html) pairs in
/// page order.
fn split_message_blocks(html: &str) -> Vec<(String, &str)> {
    let posts: Vec<(usize, String)> = RE_DATA_POST
        .captures_iter(html)
        .filter_map(|c| {
            let m = c.get(0)?;
            let post = c[1].to_string();
            // Only count actual message posts (channel/msgid format).
            if post.contains('/') { Some((m.start(), post)) } else { None }
        })
        .collect();
    posts
        .iter()
        .enumerate()
        .map(|(i, (start, post))| {
            let end = posts.get(i + 1).map(|(s, _)| *s).unwrap_or(html.len());
            (post.clone(), &html[*start..end])
        })
        .collect()
}

/// Extract the channel display name from the info header.
fn extract_channel_title(html: &str) -> Option<String> {
    let caps = RE_CHAN_TITLE.captures(html)?;
    let t = crate::fetcher::html_to_plain(&caps[1]);
    if t.is_empty() { None } else { Some(t) }
}

/// Convert one message block into a feed-rs Entry.
/// `data_post` has the form `<channel>/<msg_id>`.
fn message_to_entry(data_post: &str, block: &str, feed_title: &str, lang: Lang) -> Option<Entry> {
    // data_post must be in channel/msgid form.
    if !data_post.contains('/') {
        return None;
    }

    let published = extract_published(block);

    // Main text: keep raw HTML so downstream html_to_telegram_html
    // preserves hyperlinks (<a> tags) correctly.
    let main_html = extract_main_text_html(block, data_post);

    // Reply / quoted content → <blockquote>.
    let reply_html = extract_reply_blockquote(block);

    // Compose body: reply quote at top (TG-style) + main text + media tags.
    let link = format!("https://t.me/{}", data_post);
    let mut body = String::new();
    if !reply_html.is_empty() {
        body.push_str(&reply_html);
        if !main_html.is_empty() {
            body.push('\n');
        }
    }
    body.push_str(&main_html);
    append_media_tags(&mut body, block);

    // Native polls/documents/audio/stickers have no text container and no
    // photo/video tag, so the body would be empty and the message silently
    // dropped. Keep them as a link-only entry; genuine service messages
    // (joins, pins, ...) are still skipped.
    if body.trim().is_empty() {
        let marker: Option<String> = if RE_MSG_POLL.is_match(block) {
            Some("🗳️".to_string())
        } else if RE_MSG_DOCUMENT.is_match(block) {
            let name = RE_DOC_TITLE
                .captures(block)
                .map(|c| crate::fetcher::html_to_plain(&c[1]))
                .unwrap_or_default();
            Some(if name.is_empty() {
                "📎".to_string()
            } else {
                format!("📎 {}", crate::push::escape_html(&name))
            })
        } else if RE_MSG_AUDIO.is_match(block) {
            Some("🎧".to_string())
        } else if RE_MSG_STICKER.is_match(block) {
            Some("🖼️".to_string())
        } else {
            None
        };
        match marker {
            Some(m) => {
                body = format!("<a href=\"{}\">{}</a>", crate::push::escape_attr(&link), m);
            }
            None => {
                tracing::debug!(post = %data_post, "telegram message skipped (service or unsupported)");
                return None;
            }
        }
    }

    // Title: plain-text first 50 visible chars (strip tags for display).
    let plain = crate::fetcher::html_to_plain(&main_html);
    let title = crate::platforms::make_title(&plain, i18n::t(lang, "telegram_media_title"));



    Some(crate::platforms::make_entry(data_post.to_string(), title, feed_title.to_string(), body, "text/html", link, published))
}

/// Extract a reply/quoted block and render it as a `<blockquote>`.
/// Returns empty string when the message has no reply block.
/// Author name is wrapped in a hyperlink pointing to the original message.
fn extract_reply_blockquote(block: &str) -> String {
    let reply_caps = match RE_REPLY_BLOCK.captures(block) {
        Some(c) => c,
        None => return String::new(),
    };
    let href = &reply_caps[1];
    // Inner content: position-based scan counting <a>/</a> nesting depth,
    // so hyperlinks inside the reply text don't truncate the block.
    let open_end = reply_caps.get(0).map(|m| m.end()).unwrap_or(0);
    let inner = extract_balanced_anchor_inner(block, open_end);

    // Author name (the channel/user being replied to). Use html_to_plain so
    // entities in the name (`&amp;`) are decoded rather than shown literally.
    let author = RE_REPLY_AUTHOR
        .captures(&inner)
        .map(|c| {
            crate::fetcher::html_to_plain(&c[1]).trim().to_string()
        })
        .unwrap_or_default();

    // Reply text (keep raw HTML for link preservation): balanced-div scan so
    // quoted content containing blockquotes/code blocks (inner `</div>`) is
    // not truncated at the first close tag.
    let text = RE_REPLY_TEXT_OPEN
        .find(&inner)
        .map(|m| extract_balanced_div_inner(&inner, m.end()).0)
        .unwrap_or_default();

    if text.trim().is_empty() {
        return String::new();
    }

    // Format: "<a href>author</a>:\ntext" inside a blockquote (TG-style reply at top).
    // The author is metadata decoded by html_to_plain, not body markup: escape
    // it before splicing into the anchor/text (a display name containing
    // `<`/`&` would otherwise corrupt the reply block).
    let quoted = if author.is_empty() {
        text
    } else if href.is_empty() {
        format!("{}:\n{}", crate::push::escape_html(&author), text)
    } else {
        // `href` comes from an HTML attribute and is already entity-encoded
        // (`&amp;`); the downstream anchor extraction unescapes it once, so
        // escaping it again here would turn `&` into a literal `&amp;`.
        format!(
            "<a href=\"{}\">{}</a>:\n{}",
            href,
            crate::push::escape_html(&author),
            text
        )
    };
    format!("<blockquote>{}</blockquote>", quoted)
}

/// Parse the message's publish time from its **own** date anchor.
///
/// The reply block (`tgme_widget_message_reply`) sits before the main text
/// and embeds the quoted message's own `<time datetime>` — inside a nested
/// `tgme_widget_message_date` span too — so grabbing the first
/// `datetime="..."` or the first date container in the block makes replies
/// publish at the quoted message's timestamp (breaks ordering and
/// fresh-entry detection).
/// Strategy: compute the reply block's byte range via the balanced-anchor
/// scan, then take the first `tgme_widget_message_date` container **outside**
/// that range and read its datetime (bounded by the next `data-post`, i.e.
/// the next message, so a stray match cannot leak). If no date container
/// exists outside the reply (page redesigned), fall back to the first
/// datetime positioned outside the reply range. Datetimes inside the reply
/// block are never a source.
fn extract_published(block: &str) -> Option<DateTime<Utc>> {
    // Reply block byte range [opening tag start, matching `</a>` start);
    // every datetime inside belongs to the quoted message. Unbalanced reply
    // anchor (truncated markup): exclude through end of block — a missing
    // timestamp is safer than the quoted message's wrong one.
    let reply_range = RE_REPLY_BLOCK.find(block).map(|reply| {
        let end = balanced_close_start(block, reply.end(), &RE_ANCHOR_OPEN, &RE_ANCHOR_CLOSE, 1)
            .unwrap_or(block.len());
        (reply.start(), end)
    });
    let in_reply = |pos: usize| matches!(&reply_range, Some((s, e)) if pos >= *s && pos < *e);

    // Preferred: date container outside the reply block, first datetime
    // within it.
    for open in RE_DATE_CONTAINER.find_iter(block) {
        if in_reply(open.start()) {
            continue;
        }
        let s = &block[open.end()..];
        let s = match RE_DATA_POST.find(s) {
            // Do not leak past this message's wrap into the next block.
            Some(next) => &s[..next.start()],
            None => s,
        };
        let dt = RE_DATETIME
            .captures(s)
            .and_then(|c| DateTime::parse_from_rfc3339(&c[1]).ok())
            .map(|d| d.with_timezone(&Utc));
        if dt.is_some() {
            return dt;
        }
    }
    // Fallback: first datetime positioned outside the reply range.
    for caps in RE_DATETIME.captures_iter(block) {
        if caps.get(0).map(|m| in_reply(m.start())).unwrap_or(true) {
            continue;
        }
        if let Ok(dt) = DateTime::parse_from_rfc3339(&caps[1]) {
            return Some(dt.with_timezone(&Utc));
        }
    }
    None
}

/// Extract the main message text (raw HTML) via a balanced-div scan, with a
/// warning when the container cannot be closed (page redesign guard, so a
/// truncated body is observable instead of silent).
fn extract_main_text_html(block: &str, data_post: &str) -> String {
    let Some(open) = RE_MSG_TEXT_OPEN.find(block) else {
        // No main-text container at all: media-only message; the skip-empty
        // service-message check downstream handles this silently by design.
        return String::new();
    };
    let (inner, balanced) = extract_balanced_div_inner(block, open.end());
    if !balanced {
        tracing::warn!(
            post = %data_post,
            "telegram message text div has no balanced </div>; body may be truncated (page structure may have changed)"
        );
    }
    inner
}

/// Given `html` and the byte offset just after an opening `<a ...>` tag,
/// scan forward counting anchor nesting depth and return the inner HTML up to
/// (but excluding) the matching `</a>`. Handles nested `<a>` hyperlinks.
/// Falls back to the rest of the string if no balanced close is found.
fn extract_balanced_anchor_inner(html: &str, start: usize) -> String {
    // We are already inside the reply <a>, hence depth 1.
    match balanced_close_start(html, start, &RE_ANCHOR_OPEN, &RE_ANCHOR_CLOSE, 1) {
        Some(pos) => html[start..pos].to_string(),
        None => html[start..].to_string(),
    }
}

/// Given `html` and the byte offset just after an opening `<div ...>` tag,
/// scan forward counting **div** nesting depth (blockquotes / code blocks
/// inside the message body produce nested `</div>` tags that a non-greedy
/// `(.*?)</div>` regex would truncate at — LESSONS: non-greedy truncation).
/// Returns the inner HTML up to (but excluding) the matching `</div>` and
/// `true`; on an unbalanced region returns the remainder and `false` so
/// callers can log a redesign warning.
fn extract_balanced_div_inner(html: &str, start: usize) -> (String, bool) {
    // We are already inside the text <div>, hence depth 1.
    match balanced_close_start(html, start, &RE_DIV_OPEN, &RE_DIV_CLOSE, 1) {
        Some(pos) => (html[start..pos].to_string(), true),
        None => (html[start..].to_string(), false),
    }
}

/// Shared skeleton for the balanced-tag scanners above: collect open/close
/// tag positions at or after `start`, walk them in order from `initial_depth`,
/// and return the offset where depth returns to 0 (the matching close tag).
/// `None` when the region never closes.
fn balanced_close_start(
    html: &str,
    start: usize,
    re_open: &regex::Regex,
    re_close: &regex::Regex,
    initial_depth: i32,
) -> Option<usize> {
    let mut events: Vec<(usize, bool)> = Vec::new(); // (pos, is_open)
    for m in re_open.find_iter(html) {
        if m.start() >= start {
            events.push((m.start(), true));
        }
    }
    for m in re_close.find_iter(html) {
        if m.start() >= start {
            events.push((m.start(), false));
        }
    }
    events.sort_by_key(|e| e.0);
    let mut depth = initial_depth;
    for (pos, is_open) in events {
        if is_open {
            depth += 1;
        } else {
            depth -= 1;
            if depth == 0 {
                return Some(pos);
            }
        }
    }
    None
}

/// Append `<img>`/`<video>` tags for the message's media to the body.
fn append_media_tags(body: &mut String, block: &str) {
    for caps in RE_PHOTO_BG.captures_iter(block) {
        body.push_str(&format!("<img src=\"{}\">", crate::push::escape_attr(&caps[1])));
    }
    for caps in RE_VIDEO_SRC.captures_iter(block) {
        body.push_str(&format!("<video src=\"{}\"></video>", crate::push::escape_attr(&caps[1])));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_channel_name_valid() {
        assert_eq!(parse_channel_name("https://t.me/telegram"), Some("telegram".into()));
        assert_eq!(parse_channel_name("https://telegram.me/telegram"), Some("telegram".into()));
        assert_eq!(parse_channel_name("https://t.me/s/telegram"), Some("telegram".into()));
        assert_eq!(parse_channel_name("https://t.me/some_channel_1"), Some("some_channel_1".into()));
    }

    #[test]
    fn parse_channel_name_invalid() {
        // Too short (usernames are >= 5 chars).
        assert_eq!(parse_channel_name("https://t.me/abcd"), None);
        // Invite link (private channel).
        assert_eq!(parse_channel_name("https://t.me/+abcde"), None);
        // Reserved path.
        assert_eq!(parse_channel_name("https://t.me/share/url?url=x"), None);
        // Reserved invite path must not parse as a channel: "joinchat" is an
        // otherwise-legal 8-char username and would 404 forever.
        assert_eq!(parse_channel_name("https://t.me/joinchat/ABCDEF"), None);
        assert_eq!(parse_channel_name("https://t.me/c/1234567890/42"), None);
        // Bare domain.
        assert_eq!(parse_channel_name("https://t.me/"), None);
        // Other host.
        assert_eq!(parse_channel_name("https://example.com/telegram"), None);
    }

    #[test]
    fn extract_title_works() {
        let html = r#"<div class="tgme_channel_info_header_title"><span dir="auto">Telegram News</span></div>"#;
        assert_eq!(extract_channel_title(html), Some("Telegram News".into()));
        assert_eq!(extract_channel_title("<div>nothing</div>"), None);
    }

    #[test]
    fn strip_tags_for_title_works() {
        let html = r#"Hello <b>world</b> <a href="https://example.com">link</a>"#;
        assert_eq!(crate::fetcher::html_to_plain(html), "Hello world link");
        assert_eq!(crate::fetcher::html_to_plain(""), "");
    }

    #[test]
    fn extract_reply_blockquote_works() {
        let block = r#"<div class="tgme_widget_message_bubble"><a class="tgme_widget_message_reply user-color-default" href="https://t.me/some_channel/1"><div class="tgme_widget_message_author accent_color"><span class="tgme_widget_message_author_name" dir="auto">News Channel</span></div><div class="tgme_widget_message_text js-message_reply_text" dir="auto">quoted original content</div></a><div class="tgme_widget_message_text js-message_text">main text</div></div>"#;
        let q = extract_reply_blockquote(block);
        assert!(q.starts_with("<blockquote>"));
        assert!(q.ends_with("</blockquote>"));
        // Author wrapped in hyperlink to original message
        assert!(q.contains(r#"<a href="https://t.me/some_channel/1">News Channel</a>:"#));
        assert!(q.contains("quoted original content"));
    }

    #[test]
    fn extract_reply_blockquote_none() {
        let block = r#"<div class="tgme_widget_message_text js-message_text">message without a reply</div>"#;
        assert_eq!(extract_reply_blockquote(block), "");
    }

    #[test]
    fn extract_reply_blockquote_nested_hyperlink() {
        // When the reply text contains hyperlinks, the block must not be
        // truncated at the first inner </a> (fix for #2).
        let block = r#"<a class="tgme_widget_message_reply" href="https://t.me/src/1"><span class="tgme_widget_message_author_name">Author</span><div class="tgme_widget_message_text js-message_reply_text">before<a href="https://example.com">link text</a>after</div></a>"#;
        let q = extract_reply_blockquote(block);
        assert!(q.contains("before"), "out: {}", q);
        assert!(q.contains("after"), "out: {}", q);
        assert!(q.contains(r#"<a href="https://example.com">link text</a>"#), "out: {}", q);
    }

    #[test]
    fn extract_reply_blockquote_nested_div() {
        // Quoted content containing an inner </div> (e.g. a blockquote or
        // code block) must not truncate the reply text at the first close.
        let block = r#"<a class="tgme_widget_message_reply" href="https://t.me/src/1"><span class="tgme_widget_message_author_name">Author</span><div class="tgme_widget_message_text js-message_reply_text">head<div class="quoted-inner">middle</div>tail</div></a>"#;
        let q = extract_reply_blockquote(block);
        assert!(q.contains("head"), "out: {}", q);
        assert!(q.contains("middle"), "out: {}", q);
        assert!(q.contains("tail"), "out: {}", q);
    }

    #[test]
    fn main_text_nested_div_not_truncated() {
        // The main text div may contain nested block divs; the balanced scan
        // must return the full inner HTML up to the matching close.
        let block = r#"<div class="tgme_widget_message" data-post="chan/9"><div class="tgme_widget_message_text js-message_text" dir="auto">A<div class="quote">B</div>C</div><div class="after">ignored</div></div>"#;
        let html = extract_main_text_html(block, "chan/9");
        assert_eq!(html, "A<div class=\"quote\">B</div>C");
    }

    #[test]
    fn published_prefers_date_container_outside_reply() {
        // The reply block appears before the main text and embeds the quoted
        // message's own <time> (inside a nested tgme_widget_message_date
        // wrapper). published must come from the message's own date
        // container, never from the quoted timestamp.
        let block = r#"<div class="tgme_widget_message" data-post="chan/200"><a class="tgme_widget_message_reply" href="https://t.me/chan/100"><div class="tgme_widget_message_date"><time datetime="2020-01-01T00:00:00+00:00">old</time></div><span class="tgme_widget_message_author_name">Author</span><div class="tgme_widget_message_text js-message_reply_text">quoted</div></a><div class="tgme_widget_message_text js-message_text">new main</div><div class="tgme_widget_message_date"><time datetime="2026-10-17T08:30:00+00:00">now</time></div></div>"#;
        let dt = extract_published(block).expect("a publish time");
        let expected = DateTime::parse_from_rfc3339("2026-10-17T08:30:00+00:00")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(dt, expected);
    }

    #[test]
    fn published_falls_back_after_reply_anchor() {
        // Redesigned layout without a recognizable date container outside
        // the reply: fall back to the first datetime after the reply block's
        // matching </a>, still skipping the quoted message's timestamp.
        let block = r#"<div class="tgme_widget_message" data-post="chan/200"><a class="tgme_widget_message_reply" href="https://t.me/chan/100"><time datetime="2020-01-01T00:00:00+00:00">old</time><span class="tgme_widget_message_author_name">Author</span><div class="tgme_widget_message_text js-message_reply_text">quoted</div></a><div class="tgme_widget_message_text js-message_text">new main</div><time datetime="2026-10-17T08:30:00+00:00">now</time></div>"#;
        let dt = extract_published(block).expect("a publish time");
        let expected = DateTime::parse_from_rfc3339("2026-10-17T08:30:00+00:00")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(dt, expected);
    }

    #[test]
    fn published_without_reply_unchanged() {
        // A reply-less message block keeps the pre-existing behavior: its own
        // (only) datetime is used, container present or not.
        let block = r#"<div class="tgme_widget_message" data-post="chan/200"><div class="tgme_widget_message_date"><time datetime="2026-10-17T08:30:00+00:00">now</time></div><div class="tgme_widget_message_text js-message_text">plain</div></div>"#;
        let dt = extract_published(block).expect("a publish time");
        let expected = DateTime::parse_from_rfc3339("2026-10-17T08:30:00+00:00")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(dt, expected);
    }

    #[test]
    fn append_media_tags_works() {
        let block = r#"<a class="tgme_widget_message_photo_wrap" style="background-image:url('https://cdn1.telesco.pe/file/abc.jpg')"></a><video src="https://cdn1.telesco.pe/file/x.mp4?token=t"></video>"#;
        let mut body = String::from("text");
        append_media_tags(&mut body, block);
        assert!(body.contains("<img src=\"https://cdn1.telesco.pe/file/abc.jpg\">"));
        assert!(body.contains("<video src=\"https://cdn1.telesco.pe/file/x.mp4?token=t\"></video>"));
    }

    #[test]
    fn append_media_tags_ignores_non_photo_thumbs() {
        // Video posters (video_thumb) and reply thumbnails (reply_thumb) must
        // not be treated as body images.
        let block = r#"<a class="tgme_widget_message_video_thumb" style="background-image:url('https://cdn1.telesco.pe/file/poster.jpg')"></a><div class="tgme_widget_message_reply_thumb" style="background-image:url('https://cdn1.telesco.pe/file/reply.jpg')"></div>"#;
        let mut body = String::from("text");
        append_media_tags(&mut body, block);
        assert!(!body.contains("poster.jpg"));
        assert!(!body.contains("reply.jpg"));
    }

    #[test]
    fn message_block_split_works() {
        let html = r#"<div class="tgme_widget_message_wrap"><div class="tgme_widget_message" data-post="telegram/441"><div class="tgme_widget_message_text">first</div></div></div><div class="tgme_widget_message_wrap"><div class="tgme_widget_message" data-post="telegram/442"><div class="tgme_widget_message_text">second</div></div></div>"#;
        let blocks = split_message_blocks(html);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0, "telegram/441");
        assert_eq!(blocks[1].0, "telegram/442");
        // Verify slicing yields distinct blocks.
        assert!(blocks[0].1.contains("first"));
        assert!(!blocks[0].1.contains("second"));
        assert!(blocks[1].1.contains("second"));
    }

    #[test]
    fn message_block_split_skips_non_post_data_post() {
        // data-post without '/' (not a channel/msgid message post) is skipped.
        let html = r#"<div data-post="notapost">x</div><div data-post="telegram/1">real</div>"#;
        let blocks = split_message_blocks(html);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].0, "telegram/1");
    }

    #[test]
    fn non_text_messages_keep_a_link_entry() {
        // Polls/documents/audio/stickers have no text container; they must
        // become a link-only entry instead of being dropped silently.
        let poll = r#"<div class="tgme_widget_message_poll"><div class="tgme_widget_message_poll_question">Q?</div></div>"#;
        let entry = message_to_entry("chan/1", poll, "Chan", Lang::Zh).expect("poll entry");
        assert_eq!(entry.title.as_ref().unwrap().content, "(媒体消息)");
        let body = entry.content.unwrap().body.unwrap();
        assert!(body.contains("https://t.me/chan/1"));
        assert!(body.contains('🗳'));

        let en = message_to_entry("chan/1", poll, "Chan", Lang::En).expect("poll entry");
        assert_eq!(en.title.as_ref().unwrap().content, "(Media message)");

        let doc = r#"<div class="tgme_widget_message_document_wrap"><span class="tgme_widget_message_document_title">report.pdf</span></div>"#;
        let entry = message_to_entry("chan/2", doc, "Chan", Lang::Zh).expect("document entry");
        let body = entry.content.unwrap().body.unwrap();
        assert!(body.contains("https://t.me/chan/2"));
        assert!(body.contains("report.pdf"));

        // Genuine service messages are still skipped.
        let service = r#"<div class="tgme_widget_message_service">joined the channel</div>"#;
        assert!(message_to_entry("chan/3", service, "Chan", Lang::Zh).is_none());
    }
}
