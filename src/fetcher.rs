use std::sync::LazyLock;

use anyhow::Context;
use feed_rs::model::Entry;
use url::Url;

use crate::htmltext::{unescape_html, RE_TAG};
use crate::state::AppState;

/// Static regexes: the HTML→Telegram-HTML conversion runs on every entry,
/// so compile once and reuse uniformly (previously every call recompiled a
/// dozen or so regexes).
/// RE_ENTITY_NUM/RE_ENTITY_HEX moved into crate::htmltext together with
/// unescape_html. RE_TAG is likewise shared from htmltext (single compiled
/// copy of `<[^>]+>` across the project).
static RE_AUTHOR_PAREN: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\(([^)]+)\)").unwrap());
pub static RE_BLANK_LINES: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\n{3,}").unwrap());
/// Line-leading indentation whitespace (HTML formatting indentation from
/// sources like RSSHub, left over after strip_tags).
static RE_LINE_INDENT: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?m)^[ \t]+").unwrap());
/// Fold runs of horizontal whitespace into a single space.
static RE_MULTI_SPACE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"[ \t]{2,}").unwrap());
pub static RE_VIDEO_BLOCK: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?is)<video\b[^>]*>.*?</video>").unwrap());
pub static RE_IMG_ALT: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"(?is)<img\b[^>]*?\balt\s*=\s*["']([^"']*)["'][^>]*>"#).unwrap());
pub static RE_IMG: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?is)<img\b[^>]*>").unwrap());
static RE_SOURCE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?is)<source\b[^>]*>").unwrap());
static RE_VIDEO_FALLBACK_TEXT: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"视频无法显示[，,]?\s*请前往.*?观看[。.]?").unwrap());
static RE_WEIBO_VIDEO_CARD_TEXT: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"[\w\u4e00-\u9fff]+的微博视频").unwrap());
pub static RE_BR: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)<br\s*/?>").unwrap());
/// Block-level closing tags whose boundaries must survive tag stripping.
/// Without this, `<li>a</li><li>b</li>` flattens to `ab` and table rows run
/// together. `<blockquote>`/`<pre>` are intentionally excluded: they have
/// their own sentinel/atomic handling.
pub static RE_BLOCK_CLOSE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)</(?:p|div|li|tr|td|th|h[1-6]|table|thead|tbody|ul|ol|section|article|header|footer|figure|figcaption)\s*>",
    )
    .unwrap()
});
static RE_BLOCKQUOTE_OPEN: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)<blockquote\b[^>]*>").unwrap());
static RE_BLOCKQUOTE_CLOSE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)</blockquote\s*>").unwrap());
static RE_ANCHOR: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"(?is)<a\b[^>]*?href\s*=\s*["']([^"']*)["'][^>]*>(.*?)</a>"#).unwrap());

/// Result of fetching a single feed.
pub struct FetchResult {
    pub feed_title: Option<String>,
    /// Entries sorted newest-first.
    pub entries: Vec<Entry>,
    /// Server returned 304 (source not updated): nothing to push; callers
    /// treat this as success.
    pub not_modified: bool,
    /// Negotiation headers carried by this response, stored in the DB for the
    /// next conditional request.
    pub http_etag: Option<String>,
    pub http_last_modified: Option<String>,
    /// Structured entry metadata (sidecar, group-C refactor):
    /// entry_id → EntryMeta. Replaces the in-body data-vote/data-rt markers
    /// and the content_type MIME secret handshake; populated on demand by
    /// platform sources, always empty for RSS sources.
    pub metas: crate::entrymeta::MetaMap,
}

impl FetchResult {
    /// Platform-source constructor (F6 convergence): native fetching does not
    /// go through HTTP conditional requests, so the negotiation headers are
    /// always empty. The three platforms previously duplicated the same
    /// literal verbatim; funneling them here prevents field drift.
    pub fn platform(feed_title: Option<String>, entries: Vec<Entry>) -> Self {
        Self {
            feed_title,
            entries,
            not_modified: false,
            http_etag: None,
            http_last_modified: None,
            metas: std::collections::HashMap::new(),
        }
    }
}

/// Normalize a feed URL to a canonical form so that logically identical
/// sources (different host/path variants) deduplicate correctly at subscribe
/// time. Platform sources are rewritten via the platform registry's
/// `canonical_url`; plain RSS URLs are returned unchanged.
///
/// Canonical forms:
/// - weibo: `https://weibo.com/u/<uid>` (m./www./weibo.cn variants collapse)
/// - x:     `https://x.com/<name>` (twitter.com / www / mobile collapse)
/// - tg:    `https://t.me/<name>` (telegram.me and /s/<name> collapse)
pub fn normalize_feed_url(url: &str) -> String {
    match crate::platforms::platform_for_url(url) {
        Some(p) => (p.canonical_url)(url).unwrap_or_else(|| url.to_string()),
        None => url.to_string(),
    }
}

/// Response body size cap (20MB): prevents abnormal/malicious sources from
/// exhausting memory with huge bodies. RSS fetching and Telegram channel page
/// fetching share this same criterion.
pub const MAX_BODY_BYTES: u64 = 20 * 1024 * 1024;

/// Stream-read a response body while enforcing the size cap (M8 fix: the
/// fetcher and telegram.rs previously duplicated the same bytes_stream loop
/// verbatim in two places). Checks while reading and aborts as soon as the
/// limit is exceeded — does not rely on a Content-Length pre-check (that
/// pre-check is ineffective for chunked responses without the header).
/// `ctx` identifies the source in error messages (e.g. feed URL or channel
/// name).
pub async fn read_body_capped(resp: reqwest::Response, ctx: &str) -> anyhow::Result<Vec<u8>> {
    if let Some(len) = resp.content_length() {
        if len > MAX_BODY_BYTES {
            anyhow::bail!("response too large ({} bytes) for {}", len, ctx);
        }
    }
    let mut body_buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("read body chunk failed for {}", ctx))?;
        body_buf.extend_from_slice(&chunk);
        if body_buf.len() as u64 > MAX_BODY_BYTES {
            anyhow::bail!("response too large (>{} bytes) for {}", MAX_BODY_BYTES, ctx);
        }
    }
    Ok(body_buf)
}

/// Read a response body under the size cap (via `read_body_capped`) and parse
/// it as JSON. Platform fetch code previously called `resp.json()` directly,
/// which bypasses MAX_BODY_BYTES and lets a hostile/oversized API response
/// buffer unbounded. This routes every platform JSON read through the same cap
/// the RSS pipeline uses — keep it that way: new platform JSON calls must use
/// this helper, never bare `resp.json()`. `ctx` identifies the source in
/// error messages.
pub async fn read_json_capped(resp: reqwest::Response, ctx: &str) -> anyhow::Result<serde_json::Value> {
    let bytes = read_body_capped(resp, ctx).await?;
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("JSON parse failed for {}", ctx))?;
    Ok(json)
}

/// Fetch and parse an RSS/Atom/JSON feed.
/// Pass in the previously saved ETag / Last-Modified for a conditional
/// request: when the source has not been updated the server returns 304,
/// saving body transfer and parsing (`not_modified` = true).
pub async fn fetch_feed(
    state: &AppState,
    feed_url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
    known_sent: Option<&std::collections::HashSet<String>>,
) -> anyhow::Result<FetchResult> {
    // Native platform fetch (weibo / X / Telegram): bypass the RSS/HTTP
    // pipeline entirely. Registry-driven — new platforms route automatically.
    // `known_sent` lets platform fetchers skip enrichment requests for entries
    // the scheduler already pushed (Weibo long-text/vote detail, X pagination).
    if let Some(p) = crate::platforms::platform_for_url(feed_url) {
        return (p.fetch)(state, feed_url.to_string(), known_sent).await;
    }

    let _permit = state.fetch_semaphore.acquire().await
        .context("fetch semaphore closed")?;

    // SSRF protection (entry point): /sub and /test accept arbitrary URLs,
    // and the bot runs in host network mode; without validation, whitelisted
    // users could use the bot to probe the host's intranet ports / cloud
    // metadata. Both this entry pre-check (`ssrf_check`, three-state so a
    // transient DNS blip reports LookupFailed instead of masking as a block)
    // and the media download path (`is_private_url`) share the same
    // private/loopback classification defined in ssrf.rs.
    // Requests routed via a proxy have their connections established on the
    // proxy side, so the local DNS resolution result does not affect the
    // actual connection target — skip the check (v1.10.14: fixed false
    // blocking caused by local DNS resolving a domain to a private address).
    if !state.is_routed_via_proxy(feed_url) {
        match crate::ssrf::ssrf_check(feed_url).await {
            crate::ssrf::SsrfOutcome::Private => {
                anyhow::bail!("blocked: private/loopback address: {}", feed_url);
            }
            // DNS blip: distinct text so the transient-error classifier can
            // match it — sendpolicy::is_net_error matches the exact substring
            // "dns resolve failed" (contract: keep this wording in sync).
            crate::ssrf::SsrfOutcome::LookupFailed => {
                anyhow::bail!("dns resolve failed: {}", feed_url);
            }
            crate::ssrf::SsrfOutcome::InvalidUrl => {
                anyhow::bail!("invalid feed URL: {}", feed_url);
            }
            crate::ssrf::SsrfOutcome::Allowed => {}
        }
    }

    // RSS feed fetching does NOT go through img-relay (that's only for media).
    // Disable client auto-redirects (SSRF protection): follow manually hop by
    // hop and re-run is_private_url on every hop, preventing a malicious
    // source from 302-ing to an intranet address to bypass the entry check.
    const MAX_REDIRECTS: u32 = 5;
    let mut cur_url = feed_url.to_string();
    let resp;
    let mut hops = 0u32;
    loop {
        let client = state.media_client_for_url(&cur_url);
        let mut req = client
            .get(&cur_url)
            .header("Accept", "application/rss+xml, application/atom+xml, application/xml, text/xml, */*");
        if let Some(e) = etag {
            req = req.header("If-None-Match", e);
        }
        if let Some(lm) = last_modified {
            req = req.header("If-Modified-Since", lm);
        }
        let r = req
            .send()
            .await
            .with_context(|| format!("request failed: {}", cur_url))?;
        // 304 Not Modified is also a 3xx, so it must be checked before
        // is_redirection() — otherwise it would be treated as a redirect, and
        // a 304 without a Location header would falsely report
        // "redirect without Location header" (v1.10.4 fix).
        if r.status() == reqwest::StatusCode::NOT_MODIFIED {
            resp = r;
            break;
        }
        if r.status().is_redirection() {
            hops += 1;
            if hops > MAX_REDIRECTS {
                anyhow::bail!("too many redirects (>{}): {}", MAX_REDIRECTS, feed_url);
            }
            let loc = r.headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow::anyhow!("redirect without Location header"))?;
            let next = url::Url::parse(&cur_url)
                .and_then(|base| base.join(loc))
                .map_err(|e| anyhow::anyhow!("bad redirect Location: {}", e))?;
            let next_str = next.to_string();
            // When routed via a proxy, redirects are also followed on the
            // proxy side, so a local DNS check is meaningless (same as the
            // entry point). Three-state check (same contract as the entry): a
            // transient DNS blip on a redirect target must not masquerade as a
            // "private/loopback block" (which matches no retry rule); it is
            // reported as "dns resolve failed" so is_net_error treats it as
            // transient. Substring contract: keep both wordings in sync with
            // sendpolicy::is_net_error.
            if !state.is_routed_via_proxy(&next_str) {
                match crate::ssrf::ssrf_check(&next_str).await {
                    crate::ssrf::SsrfOutcome::Private => {
                        anyhow::bail!("blocked: redirect to private/loopback address");
                    }
                    crate::ssrf::SsrfOutcome::LookupFailed => {
                        anyhow::bail!("dns resolve failed: {}", next_str);
                    }
                    crate::ssrf::SsrfOutcome::InvalidUrl => {
                        anyhow::bail!("invalid redirect URL: {}", next_str);
                    }
                    crate::ssrf::SsrfOutcome::Allowed => {}
                }
            }
            cur_url = next_str;
            continue;
        }
        resp = r;
        break;
    }

    // 304 Not Modified: the source has no updates
    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(FetchResult {
            feed_title: None,
            entries: Vec::new(),
            not_modified: true,
            // A 304 response may not carry the headers; keep the old values
            http_etag: None,
            http_last_modified: None,
            metas: std::collections::HashMap::new(),
        });
    }

    if !resp.status().is_success() {
        anyhow::bail!("HTTP {} for {}", resp.status(), feed_url);
    }

    // Capture this response's negotiation headers (only overwrite the old
    // values when non-empty)
    let http_etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    let http_last_modified = resp
        .headers()
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());

    // Response body size cap (20MB): stream-read and abort as soon as the
    // limit is exceeded (read_body_capped).
    let bytes = read_body_capped(resp, feed_url).await?;

    let feed = feed_rs::parser::parse(&bytes[..])
        .with_context(|| format!("parse failed: {}", feed_url))?;

    let feed_title = feed.title.map(|t| t.content).filter(|s| !s.trim().is_empty());

    // Sort entries newest first by published/updated date.
    let mut entries = feed.entries;
    crate::platforms::sort_entries_newest_first(&mut entries);

    Ok(FetchResult {
        feed_title,
        entries,
        not_modified: false,
        http_etag,
        http_last_modified,
        metas: std::collections::HashMap::new(),
    })
}

/// Apply image-relay / proxy resolution to a media URL if it matches the whitelist.
pub fn resolve_url(state: &AppState, raw: &str) -> String {
    let cfg = &state.config;
    if !cfg.img_relay_enabled || cfg.img_relay_base_url.is_empty() {
        return raw.to_string();
    }
    if let Ok(u) = Url::parse(raw) {
        let host = u.host_str().unwrap_or_default().to_lowercase();
        // Single host-matching implementation shared with proxy/insecure
        // routing (trim leading dot, lowercase, exact-or-subdomain).
        if crate::config::host_in_list(&host, &cfg.img_relay_domains) {
            return format!("{}/{}", cfg.img_relay_base_url.trim_end_matches('/'), raw);
        }
    }
    raw.to_string()
}

/// Extract a stable unique id for an entry.
pub fn entry_id(entry: &Entry) -> String {
    if !entry.id.trim().is_empty() {
        return entry.id.clone();
    }
    if let Some(l) = &entry.links.first() {
        return l.href.clone();
    }
    // Fallback: title + published timestamp + body hash. The hash keeps
    // distinct entries with the same title and no timestamp from sharing a
    // dedup key (the old `t:title:` key made one of them skip forever).
    let title = entry.title.as_ref().map(|t| t.content.clone()).unwrap_or_default();
    let ts = entry.published.or(entry.updated)
        .map(|d| d.timestamp().to_string())
        .unwrap_or_default();
    let body = entry
        .content
        .as_ref()
        .and_then(|c| c.body.clone())
        .or_else(|| entry.summary.as_ref().map(|s| s.content.clone()))
        .unwrap_or_default();
    let hash: String = {
        use sha2::{Digest, Sha256};
        Sha256::digest(body.as_bytes())
            .iter()
            .take(8)
            .map(|b| format!("{:02x}", b))
            .collect()
    };
    format!("t:{}:{}:{}", title, ts, hash)
}

/// Extract entry link.
pub fn entry_link(entry: &Entry) -> Option<String> {
    entry.links.first().map(|l| l.href.clone())
}

/// Extract entry title.
pub fn entry_title(entry: &Entry) -> String {
    entry
        .title
        .as_ref()
        .map(|t| t.content.clone())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "(无标题)".to_string())
}

/// Extract entry author name (first author), if any.
pub fn entry_author(entry: &Entry) -> Option<String> {
    entry.authors.iter().find_map(|a| {
        // Per the spec, RSS2's <author> is an email field; feed-rs puts the
        // tag content into Person.email, leaving name with only the role
        // placeholder (the literal "author"). So prefer a meaningful name,
        // and in the placeholder case take the real name from email instead.
        // Since feed-rs 3.0, Person.name is Option<String>.
        let name = a.name.as_deref().unwrap_or("").trim();
        if !name.is_empty()
            && !matches!(name, "author" | "creator" | "managingEditor" | "webMaster")
        {
            return Some(name.to_string());
        }
        a.email.as_ref().and_then(|e| {
            let e = e.trim();
            if e.is_empty() {
                return None;
            }
            // Standard forms like "bob@example.com (Bob Smith)": extract the
            // name inside the parentheses
            if let Some(cap) = RE_AUTHOR_PAREN.captures(e) {
                let inner = cap[1].trim();
                if !inner.is_empty() {
                    return Some(inner.to_string());
                }
            }
            // A bare email address is not shown as the author name (avoid
            // pushing email addresses into the channel)
            if e.contains('@') {
                return None;
            }
            Some(e.to_string())
        })
    })
}

/// Extract entry summary/content as Telegram-HTML (media stripped, links kept
/// as `<a>` hyperlinks), trimmed and truncated to `max`. See
/// `html_to_telegram_html`. On overflow falls back to truncated plain text so
/// the result is always valid Telegram HTML.
///
/// IMPORTANT: `max` is compared against the VISIBLE text length (tags
/// stripped), not the raw HTML length. Weibo search URLs inside <a href>
/// are very long but the visible text is short — using HTML length would
/// incorrectly trigger the plain-text fallback and lose all hyperlinks.
/// Note: every push-path caller passes 100_000 deliberately (splitting into
/// multiple messages happens at send time via split_html), so the overflow
/// plain-text fallback is a safety net for absurdly large bodies rather than a
/// routine branch; do not lower these call-site values without checking that.
///
/// When `meta.prerendered_html` is true (weibo markdown-rendered posts,
/// group-C sidecar, replacing the former content_type `text/x-tg-html` MIME
/// secret handshake), the body
/// is already valid Telegram HTML — only media tags are stripped,
/// formatting tags (<b>/<i>/<s>/<code>/<a>/<blockquote>) are preserved.
pub fn entry_body_html(entry: &Entry, max: usize, meta: Option<&crate::entrymeta::EntryMeta>) -> String {
    let content = entry.content.as_ref();
    let html = content
        .and_then(|c| c.body.clone())
        .or_else(|| entry.summary.as_ref().map(|s| s.content.clone()))
        .unwrap_or_default();

    // Pre-rendered Telegram HTML (weibo markdown posts): skip the generic
    // html_to_telegram_html conversion which would strip formatting tags.
    // Only remove media tags (<img>/<video>/<source>) — media is sent as
    // separate attachments by the media pipeline.
    // Group C: explicitly marked via EntryMeta.prerendered_html (replacing
    // the text/x-tg-html MIME secret handshake).
    let is_prerendered = meta.map(|m| m.prerendered_html).unwrap_or(false);

    let tg = if is_prerendered {
        let s = RE_VIDEO_BLOCK.replace_all(&html, "").to_string();
        let s = RE_IMG.replace_all(&s, "").to_string();
        let s = RE_SOURCE.replace_all(&s, "").to_string();
        collapse_blank_lines(&s)
    } else {
        html_to_telegram_html(&html)
    };

    // Check visible text length (strip all tags) against the limit.
    // Visible text uses the text decoder (`decode_entities`); only URLs keep
    // the URL decoder (`unescape_html`) semantics.
    let visible = crate::htmltext::decode_entities(&strip_tags(&tg));
    if visible.chars().count() <= max {
        return tg;
    }
    // Overflow: emit truncated plain text (always parseable, no links).
    let plain = crate::htmltext::decode_entities(&strip_tags(&html));
    let plain = collapse_blank_lines(&plain);
    crate::htmltext::escape_html(&truncate_text(plain.trim(), max))
}

/// Strip all HTML tags.
pub fn strip_tags(s: &str) -> String {
    RE_TAG.replace_all(s, "").to_string()
}

/// Collapse runs of 3+ newlines down to 2.
fn collapse_blank_lines(s: &str) -> String {
    RE_BLANK_LINES.replace_all(s, "\n\n").to_string()
}

/// Strip HTML tags, unescape entities, and trim. Produces plain text
/// suitable for titles and summaries.
pub fn html_to_plain(html: &str) -> String {
    // Visible-text decoder, not the URL decoder: titles, author names and
    // document labels must decode named entities like `&nbsp;`/`&mdash;` too.
    // Block boundaries become spaces, otherwise list items/table cells would
    // concatenate ("firstsecond").
    let s = RE_BR.replace_all(html, " ");
    let s = RE_BLOCK_CLOSE.replace_all(&s, " ");
    let s = strip_tags(&s);
    let s = crate::htmltext::decode_entities(&s);
    s.trim().to_string()
}

/// True when an anchor href points at a media resource (a video page or an
/// image/video file). Such links are dropped from the text because the media
/// is sent as a separate attachment.
pub fn is_media_href(href: &str) -> bool {
    let lower = href.to_lowercase();
    // Weibo video page / card links. Note: wblive (livestream) links are NOT
    // treated as media — live posts have no media to send, so the link must
    // stay in the text as a clickable hyperlink for users to watch.
    if lower.contains("video.weibo.com") || lower.contains("weibo.com/tv/") {
        return true;
    }
    // Direct media file extensions.
    let path = lower.split('?').next().unwrap_or(&lower);
    const MEDIA_EXT: [&str; 9] = [".mp4", ".mov", ".webm", ".m4v", ".jpg", ".jpeg", ".png", ".gif", ".webp"];
    MEDIA_EXT.iter().any(|e| path.ends_with(e))
}

/// Convert an HTML fragment into Telegram HTML:
/// - media (`<img>`/`<video>`/`<source>`) is dropped entirely — media is sent
///   as separate photo/video, so its URL must never leak into the text;
/// - hyperlinks (including Weibo `#topic#` hashtag search links) become `<a href>…</a>`
///   so hashtags render as clickable hyperlinks, not markdown source;
/// - `<br>` / block closes become newlines;
/// - all other markup is flattened to escaped plain text.
///
/// Mirrors rsstt's approach (strip media, keep hashtag/topic links).
pub fn html_to_telegram_html(html: &str) -> String {
    let mut s = html.to_string();

    // 0) Strip sentinel characters already present in the input (U+FFFC object
    //    replacement character / U+FFFE noncharacter). These two characters
    //    are used as internal markers by this function (hyperlink
    //    placeholders / blockquote boundaries); if the feed body itself
    //    contains them they would be wrongly restored as markers, causing
    //    layout injection.
    s.retain(|c| c != '\u{FFFC}' && c != '\u{FFFE}');

    // 1) Drop media entirely. For <img>, preserve the alt text ONLY when it
    //    looks like a weibo emoji icon (e.g. <img alt="[酷]">) so it survives
    //    as plain text instead of vanishing. Regular images are sent as media
    //    attachments, so their alt text (e.g. Unsplash filenames like
    //    "peter-burdon-5aJi4Dw2_Gs-unsplash") is noise and must not leak.
    s = RE_VIDEO_BLOCK.replace_all(&s, "").to_string();
    s = RE_IMG_ALT
        .replace_all(&s, |caps: &regex::Captures| {
            let alt = &caps[1];
            if alt.starts_with('[') && alt.ends_with(']') {
                alt.to_string()
            } else {
                String::new()
            }
        })
        .to_string();
    s = RE_IMG.replace_all(&s, "").to_string();
    s = RE_SOURCE.replace_all(&s, "").to_string();
    // Safety: remove any leftover video fallback text that may have leaked
    // (e.g. the "video cannot be displayed, please go to Weibo Video to
    // watch" text from RSSHub <video> blocks).
    s = RE_VIDEO_FALLBACK_TEXT.replace_all(&s, "").to_string();
    // Also remove the "XX's Weibo Video" card text that may leak if the <a>
    // block wasn't caught by is_media_href (e.g. unusual href format).
    s = RE_WEIBO_VIDEO_CARD_TEXT.replace_all(&s, "").to_string();

    // 2) Line breaks and block boundaries. Other block closers (</li>, </tr>,
    //    headings, ...) also become newlines; otherwise their text runs
    //    together after tag stripping.
    s = RE_BR.replace_all(&s, "\n").to_string();
    s = RE_BLOCK_CLOSE.replace_all(&s, "\n").to_string();
    s = s.replace("</br>", "\n");

    // 2.5) Preserve <blockquote> boundaries (RSSHub wraps retweeted content in
    //      <blockquote>). Replace with sentinel tokens that survive strip_tags
    //      and escape_tg, then restore as Telegram <blockquote expandable>.
    s = RE_BLOCKQUOTE_OPEN.replace_all(&s, "\u{FFFE}BQO\u{FFFE}").to_string();
    s = RE_BLOCKQUOTE_CLOSE.replace_all(&s, "\u{FFFE}BQC\u{FFFE}").to_string();

    // 3) Extract anchors to placeholders, remembering (href, text).
    //    Anchors whose href is itself a media resource (video pages / image or
    //    video files) are dropped entirely — that media is sent as an
    //    attachment, so its link must not leak into the text.
    let mut links: Vec<(String, String)> = Vec::new();
    let mut buf = String::new();
    let mut last = 0usize;
    for caps in RE_ANCHOR.captures_iter(&s) {
        let whole = caps.get(0).unwrap();
        buf.push_str(&s[last..whole.start()]);
        let href = unescape_html(&caps[1]);
        if is_media_href(&href) {
            // Drop the media link (text + href).
            last = whole.end();
            continue;
        }
        let text = crate::htmltext::decode_entities(&strip_tags(&caps[2]));
        let text = text.trim().to_string();
        let idx = links.len();
        links.push((href, text));
        // Object-replacement char as a marker that survives tag-strip/escape.
        buf.push_str(&format!("\u{FFFC}{}\u{FFFC}", idx));
        last = whole.end();
    }
    buf.push_str(&s[last..]);

    // 4) Flatten remaining markup, decode entities, escape for Telegram.
    //    `decode_entities` is the visible-text decoder: it maps HTML5
    //    named entities like `&nbsp;`/`&mdash;` and numeric references in a
    //    single pass (so `&amp;lt;` decodes once, not twice). The URL decoder
    //    `unescape_html` leaves named entities literal, and the following
    //    `escape_html` would then re-escape the `&` into a visible
    //    `&amp;nbsp;` leak.
    let plain = crate::htmltext::decode_entities(&strip_tags(&buf));
    let mut out = crate::htmltext::escape_html(&collapse_blank_lines(&plain));

    // 5) Restore anchors as Telegram hyperlinks (skip empty-text links).
    for (i, (href, text)) in links.iter().enumerate() {
        let token = format!("\u{FFFC}{}\u{FFFC}", i);
        let replacement = if text.is_empty() || href.is_empty() {
            String::new()
        } else {
            format!("<a href=\"{}\">{}</a>", crate::htmltext::escape_attr(href), crate::htmltext::escape_html(text))
        };
        out = out.replace(&token, &replacement);
    }

    // 6) Restore blockquote sentinels as Telegram expandable blockquote.
    out = out.replace("\u{FFFE}BQO\u{FFFE}", "<blockquote expandable>");
    out = out.replace("\u{FFFE}BQC\u{FFFE}", "</blockquote>");

    // Collapse HTML formatting whitespace: strip line-leading indentation and
    // fold runs of spaces/tabs (RSSHub etc. indent tags heavily; after tag
    // stripping these become visible blank runs).
    out = RE_LINE_INDENT.replace_all(&out, "").to_string();
    out = RE_MULTI_SPACE.replace_all(&out, " ").to_string();

    out = collapse_blank_lines(&out);
    out.trim().to_string()
}

/// Truncate text at a char boundary, appending ellipsis if truncated.
pub fn truncate_text(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weibo_hashtag_becomes_hyperlink_and_media_stripped() {
        // Real RSSHub/Weibo fragment: hashtag wrapped in <a><span class="surl-text">,
        // plus <img>/<video> media that must NOT leak into the text.
        let html = r#"#话题A#<a href="https://m.weibo.cn/search?containerid=231522type%3D1&amp;q=%23%E8%AF%9D%E9%A2%98B%23" data-hide=""><span class="surl-text">#话题B#</span></a> 正文内容。<a href="https://video.weibo.com/show?fid=1034:1" data-hide=""><span class="url-icon"><img src="https://h5.sinaimg.cn/x.png"></span><span class="surl-text">微博视频</span></a><br clear="both"><img src="https://tvax4.sinaimg.cn/mw2000/abc.jpg" referrerpolicy="no-referrer"><video controls="controls" poster="https://tvax1.sinaimg.cn/p.jpg"><source src="https://f.video.weibocdn.com/v.mp4"></video>"#;

        let out = html_to_telegram_html(html);

        // Hashtag kept as a clickable hyperlink with decoded href.
        assert!(out.contains(r#"<a href="https://m.weibo.cn/search?containerid=231522type%3D1&amp;q=%23%E8%AF%9D%E9%A2%98B%23">#话题B#</a>"#), "out: {}", out);
        // Plain hashtag text preserved.
        assert!(out.contains("#话题A#"));
        // Media URLs must not leak.
        assert!(!out.contains("sinaimg.cn"), "out: {}", out);
        assert!(!out.contains("weibocdn.com"), "out: {}", out);
        assert!(!out.contains("![") && !out.contains("]("), "md syntax leaked: {}", out);
        // Body text preserved.
        assert!(out.contains("正文内容。"));
    }

    #[test]
    fn weibo_emoji_alt_text_preserved() {
        // Weibo emoji are icon <img alt="[酷]"> tags; the image is dropped
        // but the alt text must survive as plain text.
        let html = r#"今天心情不错<span class="url-icon"><img alt="[酷]" src="https://face.t.sinajs.cn/t4/appstyle/expression/ext/normal/cool.png"></span>继续加油<span class="url-icon"><img alt="[doge]" src="https://face.t.sinajs.cn/t4/doge.png"></span>"#;
        let out = html_to_telegram_html(html);
        assert!(out.contains("[酷]"), "out: {}", out);
        assert!(out.contains("[doge]"), "out: {}", out);
        assert!(!out.contains("sinajs.cn"), "img src leaked: {}", out);
        assert!(!out.contains("<img"), "img tag leaked: {}", out);
    }

    #[test]
    fn plain_text_escaped_and_br_to_newline() {
        let out = html_to_telegram_html("a &lt;b&gt;<br>c");
        assert_eq!(out, "a &lt;b&gt;\nc");
    }

    #[test]
    fn block_elements_are_separated_not_concatenated() {
        let out = html_to_telegram_html("<ul><li>first</li><li>second</li></ul>");
        assert!(out.contains("first\nsecond"), "list items concatenated: {out:?}");
        let title = html_to_plain("<h1>Head</h1><p>Body</p>");
        assert_eq!(title, "Head Body");
        let cells = html_to_plain("<table><tr><td>A</td><td>B</td></tr></table>");
        assert_eq!(cells, "A B");
    }

    #[test]
    fn html_to_plain_uses_visible_text_decoder() {
        assert_eq!(html_to_plain("A&nbsp;B &mdash; C&hellip;"), "A B — C…");
        assert_eq!(html_to_plain("<b>x</b>&amp;lt;"), "x&lt;");
    }

    #[test]
    fn nbsp_and_named_entities_decoded_not_double_escaped() {
        // huxiu (RSSHub) bodies carry &nbsp; inside plain text. The visible
        // path must run the text decoder: nbsp becomes a real space. The URL
        // decoder left it literal and the following escape turned the & into
        // &amp; — users saw "黄怡&amp;nbsp;苏洋".
        let out = html_to_telegram_html("<p>作者：黄怡&nbsp;苏洋 &amp; 伙伴</p>");
        assert!(out.contains("黄怡 苏洋"), "nbsp not decoded: {out}");
        assert!(out.contains("&amp; 伙伴"), "ampersand not re-escaped: {out}");
        assert!(!out.contains("nbsp"), "entity leaked: {out}");
    }

    #[test]
    fn blockquote_becomes_expandable_tg_blockquote() {
        // RSSHub non-readable retweet: <blockquote> - retweet @author: text</blockquote>
        let html = r#"转发语<br><blockquote> - 转发 <a href="https://weibo.com/n/某人">@某人</a>: 原博内容</blockquote>"#;
        let out = html_to_telegram_html(html);
        assert!(out.contains("<blockquote expandable>"), "out: {}", out);
        assert!(out.contains("</blockquote>"), "out: {}", out);
        assert!(out.contains("- 转发"), "out: {}", out);
        assert!(out.contains("原博内容"), "out: {}", out);
        assert!(out.contains(r#"<a href="https://weibo.com/n/某人">@某人</a>"#), "out: {}", out);
    }

    #[test]
    fn blockquote_readable_mode_with_style() {
        // RSSHub readable mode: <blockquote style="background: ...">
        let html = r#"正文<br clear="both" /><div style="clear: both"></div><blockquote style="background: #80808010;border-top:1px solid #80808030;">原博文字</blockquote>"#;
        let out = html_to_telegram_html(html);
        assert!(out.contains("<blockquote expandable>"), "out: {}", out);
        assert!(out.contains("原博文字"), "out: {}", out);
        assert!(out.contains("</blockquote>"), "out: {}", out);
    }

    // ===== entry_body_html prerendered path (group-C sidecar) =====

    fn entry_with_body_html(body: &str) -> feed_rs::model::Entry {
        use feed_rs::model::{Content, Entry};
        Entry {
            id: "test".to_string(),
            title: None,
            updated: None,
            published: None,
            authors: Vec::new(),
            contributors: Vec::new(),
            content: Some(Content {
                body: Some(body.to_string()),
                content_type: "text/html".parse().unwrap(),
                length: None,
                src: None,
            }),
            links: Vec::new(),
            summary: None,
            categories: Vec::new(),
            media: Vec::new(),
            rights: None,
            source: None,
            language: None,
            base: None,
        }
    }

    #[test]
    fn entry_body_html_prerendered_preserves_formatting_strips_media() {
        // prerendered_html=true: preserve <b>/<i>/<a>/<blockquote>, strip
        // only the media tags.
        let body = r#"<b>粗体</b> <i>斜体</i> <a href="https://x.com">链接</a><img src="https://img.example.com/a.jpg"><video src="https://v.example.com/b.mp4"></video><blockquote>引用</blockquote>"#;
        let entry = entry_with_body_html(body);
        let meta = crate::entrymeta::EntryMeta {
            prerendered_html: true,
            poll: None,
            retweet_images: Vec::new(),
        };
        let out = entry_body_html(&entry, 100_000, Some(&meta));
        assert!(out.contains("<b>粗体</b>"), "out: {}", out);
        assert!(out.contains("<i>斜体</i>"), "out: {}", out);
        assert!(out.contains(r#"<a href="https://x.com">链接</a>"#), "out: {}", out);
        assert!(out.contains("<blockquote>引用</blockquote>"), "out: {}", out);
        assert!(!out.contains("img.example.com"), "img leaked: {}", out);
        assert!(!out.contains("v.example.com"), "video leaked: {}", out);
    }

    #[test]
    fn entry_body_html_no_meta_runs_generic_conversion() {
        // meta=None (RSS source): goes through the generic
        // html_to_telegram_html conversion.
        let body = r#"普通文本<br>第二行<img src="https://img.example.com/a.jpg">"#;
        let entry = entry_with_body_html(body);
        let out = entry_body_html(&entry, 100_000, None);
        assert!(out.contains("普通文本"), "out: {}", out);
        assert!(out.contains("第二行"), "out: {}", out);
        assert!(!out.contains("img.example.com"), "img leaked: {}", out);
    }
}
