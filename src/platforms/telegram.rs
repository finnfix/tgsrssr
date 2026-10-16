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
    // Reserved paths (t.me/s, /addstickers, bot commands, etc.).
    const RESERVED: &[&str] = &[
        "s", "i", "addstickers", "addemoji", "share", "proxy", "socks", "contact", "login", "setlanguage",
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
/// Message text container (main text only, not reply text).
static RE_MSG_TEXT: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?is)<div[^>]*tgme_widget_message_text js-message_text[^>]*>(.*?)</div>"#).unwrap()
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
/// Reply text content.
static RE_REPLY_TEXT: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?is)<div[^>]*js-message_reply_text[^>]*>(.*?)</div>"#).unwrap()
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
/// Timestamp of the message link.
static RE_DATETIME: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"datetime="([^"]+)""#).unwrap()
});

// ---------------------------------------------------------------------------
// Main fetch entry point
// ---------------------------------------------------------------------------

/// Registry fetch entry: parse the channel name from the full feed URL, then
/// delegate to [`fetch_telegram_channel`]. Called via `PlatformDef.fetch`
/// (URL passed owned; owner_id unused — public web preview needs no cookie).
pub async fn fetch_by_url(
    state: &AppState,
    url: String,
) -> anyhow::Result<crate::fetcher::FetchResult> {
    let channel = parse_channel_name(&url)
        .ok_or_else(|| anyhow::anyhow!("Invalid Telegram channel URL: {}", url))?;
    fetch_telegram_channel(state, &channel).await
}

/// Fetch a Telegram public channel's web preview and convert to FetchResult.
pub async fn fetch_telegram_channel(
    state: &AppState,
    channel: &str,
) -> anyhow::Result<crate::fetcher::FetchResult> {
    let url = format!("https://t.me/s/{}", channel);
    let client = state.client_for_url(&url);
    let resp = client
        .get(&url)
        .header("User-Agent", crate::state::BROWSER_UA)
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

    let feed_title = extract_channel_title(&html).unwrap_or_else(|| channel.to_string());

    // Locate message blocks by finding all data-post positions and slicing
    // between consecutive ones (Rust regex has no lookahead).
    let blocks = split_message_blocks(&html);

    let mut entries: Vec<Entry> = Vec::new();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (post, block) in &blocks {
        if let Some(e) = message_to_entry(post, block, &feed_title) {
            // Dedup by entry id within the batch, matching the Weibo/X
            // convention (fix for #8)
            if !e.id.is_empty() && seen_ids.insert(e.id.clone()) {
                entries.push(e);
            }
        }
    }

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
fn message_to_entry(data_post: &str, block: &str, feed_title: &str) -> Option<Entry> {
    // data_post must be in channel/msgid form.
    if !data_post.contains('/') {
        return None;
    }

    let published = RE_DATETIME
        .captures(block)
        .and_then(|c| DateTime::parse_from_rfc3339(&c[1]).ok())
        .map(|dt| dt.with_timezone(&Utc));

    // Main text: keep raw HTML so downstream html_to_telegram_html
    // preserves hyperlinks (<a> tags) correctly.
    let main_html = RE_MSG_TEXT
        .captures(block)
        .map(|c| c[1].to_string())
        .unwrap_or_default();

    // Reply / quoted content → <blockquote>.
    let reply_html = extract_reply_blockquote(block);

    // Compose body: reply quote at top (TG-style) + main text + media tags.
    let mut body = String::new();
    if !reply_html.is_empty() {
        body.push_str(&reply_html);
        if !main_html.is_empty() {
            body.push('\n');
        }
    }
    body.push_str(&main_html);
    append_media_tags(&mut body, block);

    // Skip empty service messages (no text, no media).
    if body.trim().is_empty() {
        return None;
    }

    // Title: plain-text first 50 visible chars (strip tags for display).
    let plain = strip_tags_for_title(&main_html);
    let title = crate::platforms::make_title(&plain, "(媒体消息)");

    let link = format!("https://t.me/{}", data_post);

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

    // Author name (the channel/user being replied to).
    let author = RE_REPLY_AUTHOR
        .captures(&inner)
        .map(|c| {
            crate::fetcher::strip_tags(&c[1]).trim().to_string()
        })
        .unwrap_or_default();

    // Reply text (keep raw HTML for link preservation).
    let text = RE_REPLY_TEXT
        .captures(&inner)
        .map(|c| c[1].to_string())
        .unwrap_or_default();

    if text.trim().is_empty() {
        return String::new();
    }

    // Format: "<a href>author</a>:\ntext" inside a blockquote (TG-style reply at top).
    let quoted = if author.is_empty() {
        text
    } else if href.is_empty() {
        format!("{}:\n{}", author, text)
    } else {
        format!("<a href=\"{}\">{}</a>:\n{}", href, author, text)
    };
    format!("<blockquote>{}</blockquote>", quoted)
}

/// Given `block` and the byte offset just after an opening `<a ...>` tag,
/// scan forward counting anchor nesting depth and return the inner HTML up to
/// (but excluding) the matching `</a>`. Handles nested `<a>` hyperlinks.
/// Falls back to the rest of the string if no balanced close is found.
fn extract_balanced_anchor_inner(block: &str, start: usize) -> String {
    static RE_OPEN: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?i)<a\b").unwrap());
    static RE_CLOSE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?i)</a\s*>").unwrap());
    // Collect open/close positions at or after `start`, then walk in order.
    let mut events: Vec<(usize, bool)> = Vec::new(); // (pos, is_open)
    for m in RE_OPEN.find_iter(block) {
        if m.start() >= start {
            events.push((m.start(), true));
        }
    }
    for m in RE_CLOSE.find_iter(block) {
        if m.start() >= start {
            events.push((m.start(), false));
        }
    }
    events.sort_by_key(|e| e.0);
    let mut depth = 1i32; // we are already inside the reply <a>
    for (pos, is_open) in events {
        if is_open {
            depth += 1;
        } else {
            depth -= 1;
            if depth == 0 {
                return block[start..pos].to_string();
            }
        }
    }
    // No balanced close found: return the remainder.
    block[start..].to_string()
}

/// Strip all HTML tags and decode entities, producing plain text for titles.
fn strip_tags_for_title(html: &str) -> String {
    crate::fetcher::html_to_plain(html)
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
        assert_eq!(strip_tags_for_title(html), "Hello world link");
        assert_eq!(strip_tags_for_title(""), "");
    }

    #[test]
    fn extract_reply_blockquote_works() {
        let block = r#"<div class="tgme_widget_message_bubble"><a class="tgme_widget_message_reply user-color-default" href="https://t.me/tnews365/35618"><div class="tgme_widget_message_author accent_color"><span class="tgme_widget_message_author_name" dir="auto">News Channel</span></div><div class="tgme_widget_message_text js-message_reply_text" dir="auto">quoted original content</div></a><div class="tgme_widget_message_text js-message_text">main text</div></div>"#;
        let q = extract_reply_blockquote(block);
        assert!(q.starts_with("<blockquote>"));
        assert!(q.ends_with("</blockquote>"));
        // Author wrapped in hyperlink to original message
        assert!(q.contains(r#"<a href="https://t.me/tnews365/35618">News Channel</a>:"#));
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
}
