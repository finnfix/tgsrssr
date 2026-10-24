//! Pure HTML text utilities for Telegram message preparation.
//!
//! This module contains **no async code and no bot/db dependencies** — only
//! synchronous string/HTML manipulation: escaping, UTF-16 measurement,
//! message splitting, blockquote folding, and entity decoding.
//! Extracted from push.rs (batch-2 architecture refactor) to isolate the
//! text-processing concern from the send-orchestration concern.

use std::sync::LazyLock;

/// Strip HTML tags (visible-length computation, plain-text degradation).
pub static RE_TAG: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"<[^>]+>").unwrap());
/// Anchor opening tag carrying a non-empty href (the only shape
/// `html_to_telegram_html` emits; feeds may also provide this form directly).
static RE_ANCHOR_OPEN: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?i)^<a\b[^>]*?\bhref\s*=\s*["'][^"']+["']"#).unwrap()
});
static RE_ANCHOR_CLOSE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)^</a\s*>").unwrap());
/// HTML entity reference anchored at the start of a slice.
static RE_ENTITY_REF: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^&(?:[a-zA-Z][a-zA-Z0-9]{1,31}|#[1-9][0-9]{0,6}|#[xX][0-9a-fA-F]{1,6});").unwrap()
});
/// Blockquote open/close tags used by the splitter. Kept module-level so the
/// balanced-close scan and the re-wrap branch use the same patterns.
static RE_BQ_OPEN_TAG: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)<blockquote\b[^>]*>").unwrap());
static RE_BQ_CLOSE_TAG: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)</blockquote\s*>").unwrap());
static RE_PRE_CLOSE_TAG: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)</pre\s*>").unwrap());

/// Find the matching close tag for a block opened just before `open_end`,
/// counting nested opens. Returns the close tag's (start, end); None when the
/// region never closes.
fn find_balanced_close(
    text: &str,
    open_end: usize,
    open_re: &regex::Regex,
    close_re: &regex::Regex,
) -> Option<(usize, usize)> {
    let mut depth = 1usize;
    let mut cursor = open_end;
    loop {
        let next_open = open_re
            .find(&text[cursor..])
            .map(|m| (cursor + m.start(), cursor + m.end()));
        let next_close = close_re
            .find(&text[cursor..])
            .map(|m| (cursor + m.start(), cursor + m.end()));
        let (span, is_open) = match (next_open, next_close) {
            (Some(o), Some(c)) => {
                if o.0 <= c.0 {
                    (o, true)
                } else {
                    (c, false)
                }
            }
            (Some(o), None) => (o, true),
            (None, Some(c)) => (c, false),
            (None, None) => return None,
        };
        if is_open {
            depth += 1;
        } else {
            depth -= 1;
            if depth == 0 {
                return Some(span);
            }
        }
        cursor = span.1;
    }
}

// ---------------------------------------------------------------------------
// Length measurement (Telegram counts in UTF-16 code units)
// ---------------------------------------------------------------------------

/// Visible length of HTML text (tags stripped), in UTF-16 code units.
pub fn visible_len(html: &str) -> usize {
    let plain = RE_TAG.replace_all(html, "");
    utf16_len(&plain)
}

/// Length in UTF-16 code units. Telegram measures message/caption limits
/// (4096 / 1024) and entity offsets in UTF-16 code units, NOT Unicode chars.
/// Supplementary plane chars (most emoji) are 1 Rust `char` but 2 UTF-16
/// units, so counting chars underestimates and can yield "message is too long".
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(|c| c.len_utf16()).sum()
}

/// Longest prefix of `s` whose UTF-16 length is ≤ `max_units`, never splitting
/// a char. Always keeps at least one char so callers make forward progress.
pub fn take_utf16_prefix(s: &str, max_units: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        let lu = c.len_utf16();
        if units + lu > max_units {
            if i == 0 {
                return &s[..c.len_utf8()];
            }
            return &s[..i];
        }
        units += lu;
    }
    s
}

// ---------------------------------------------------------------------------
// Visible-text truncation with hyperlink preservation
// ---------------------------------------------------------------------------

/// Truncate Telegram-HTML to at most `max_visible` display characters while
/// keeping `<a href="…">…</a>` hyperlinks clickable.
///
/// - Anchor tags emitted by `html_to_telegram_html` (non-empty quoted href)
///   pass through untouched; the href never counts against the budget.
/// - Every other tag (and href-less anchors) flattens to a single space, the
///   same way the previous plain-text summary path flattened them.
/// - The character budget counts *display* units: each entity reference
///   (`&amp;`, `&#39;`, …) counts as one unit and is never cut mid-reference
///   (a split entity would make Telegram reject the whole message with a
///   parse-entities error).
/// - When the budget runs out inside an open anchor, `</a>` is appended so
///   the output is always well-formed.
/// - An anchor opening at or after the cut point is dropped entirely unless
///   at least one visible character of its text fits the budget (no empty or
///   dangling links).
///
/// Returns `(html, truncated)` where `truncated` means visible content
/// existed beyond the cut point (caller appends its ellipsis suffix there).
pub fn truncate_html_keep_links(html: &str, max_visible: usize) -> (String, bool) {
    let mut out = String::with_capacity(html.len().min(256));
    // Buffered <a …> opening tag, emitted only when the anchor yields its
    // first visible character within the budget (no empty links in output).
    let mut pending_open: Option<String> = None;
    let mut in_link = false;
    let mut visible = 0usize;
    let mut truncated = false;
    let mut i = 0usize;

    while i < html.len() {
        let rest = &html[i..];
        // A raw '<' only starts a tag in this pipeline: visible text is
        // escape_html'd (`&lt;`) and attribute values escape '>' as well.
        if rest.starts_with('<') {
            if let Some(gt) = rest.find('>') {
                let tag = &rest[..gt + 1];
                if RE_ANCHOR_CLOSE.is_match(tag) {
                    if in_link {
                        out.push_str(tag);
                        in_link = false;
                    }
                    // Orphan close (or buffered empty anchor): drop it and
                    // the buffered open.
                    pending_open = None;
                } else if RE_ANCHOR_OPEN.is_match(tag) && !in_link {
                    // Replace any still-buffered empty anchor with this one.
                    pending_open = Some(tag.to_string());
                } else {
                    // Non-anchor markup, or an anchor nested inside a link:
                    // flatten to a space (never emit an unbalanced open).
                    out.push(' ');
                }
                let after = gt + 1;
                i += after;
                if visible >= max_visible && !in_link && pending_open.is_none() {
                    // Budget exhausted with nothing left to close: further
                    // visible content can only mean truncation.
                    truncated = has_visible_content(&rest[after..]);
                    break;
                }
                continue;
            }
        }
        // Entity reference (display unit counted as 1, emitted atomically).
        let mut entity: Option<usize> = None;
        let mut ch = ' ';
        let mut ch_adv = 0usize;
        if let Some(m) = RE_ENTITY_REF.find(rest) {
            entity = Some(m.end());
        } else {
            ch = rest.chars().next().unwrap();
            ch_adv = ch.len_utf8();
        }
        let is_space = match entity {
            Some(len) => entity_is_space(&rest[..len]),
            None => ch.is_whitespace(),
        };
        if visible >= max_visible {
            // Past the budget: whitespace may trail the cut point without
            // counting as dropped content; anything else stops the scan.
            if is_space {
                i += entity.unwrap_or(ch_adv);
                continue;
            }
            truncated = true;
            break;
        }
        if let Some(tag) = pending_open.take() {
            out.push_str(&tag);
            in_link = true;
        }
        match entity {
            Some(len) => {
                out.push_str(&rest[..len]);
                i += len;
            }
            None => {
                out.push(ch);
                i += ch_adv;
            }
        }
        visible += 1;
    }

    if in_link {
        // Close the anchor whenever one is still open (truncated or not).
        out.push_str("</a>");
    }
    (out, truncated)
}

/// True when `s` contains at least one visible display character: tags are
/// skipped, entity references count as one character (space entities are not
/// visible content), raw text counts unless whitespace.
fn has_visible_content(s: &str) -> bool {
    let mut i = 0usize;
    while i < s.len() {
        let rest = &s[i..];
        if rest.starts_with('<') {
            if let Some(gt) = rest.find('>') {
                i += gt + 1;
                continue;
            }
        }
        if let Some(m) = RE_ENTITY_REF.find(rest) {
            if !entity_is_space(&rest[..m.end()]) {
                return true;
            }
            i += m.end();
            continue;
        }
        let c = rest.chars().next().unwrap();
        if !c.is_whitespace() {
            return true;
        }
        i += c.len_utf8();
    }
    false
}

/// True for entity references that decode to whitespace (mirrors the
/// space-producing mappings of `decode_entities`).
fn entity_is_space(entity: &str) -> bool {
    matches!(
        entity.to_ascii_lowercase().as_str(),
        "&nbsp;" | "&#160;" | "&#xa0;"
    )
}

// ---------------------------------------------------------------------------
// Escaping / entity decoding
// ---------------------------------------------------------------------------

/// Escape HTML special characters.
pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escape for use inside HTML attribute values.
pub fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Display a channel identifier: numeric IDs are wrapped in inline code;
/// @usernames are shown as-is.
pub fn format_chat_id_html(chat_id: &str) -> String {
    if chat_id.starts_with('@') {
        escape_html(chat_id)
    } else {
        format!("<code>{}</code>", escape_html(chat_id))
    }
}

/// Display a channel name (bold); if the "name" is actually the ID fallback
/// (unnamed channel), do not bold it, to avoid presenting the ID in bold.
pub fn format_chat_title_html(title: &str, chat_id: &str) -> String {
    if title.is_empty() || title == chat_id {
        escape_html(title)
    } else {
        format!("<b>{}</b>", escape_html(title))
    }
}

/// Decode HTML entities for visible text: HTML5 named references plus
/// numeric references `&#NNN;` / `&#xHHH;`. The scan is single-pass, so an
/// encoded entity (`&amp;lt;`) decodes exactly once instead of being re-read
/// as a new entity.
///
/// Edge handling: unknown/invalid references stay literal; encodings of
/// non-whitespace control characters (including NUL) are dropped rather than
/// injected into messages; numeric newline/tab are preserved because feeds
/// use them as real line breaks. The named table is in
/// [`crate::htmlentities`].
pub fn decode_entities(s: &str) -> String {
    const MAX_ENTITY_BODY: usize = 32;
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let candidate = &after[1..];
        let Some(semi) = candidate.find(';') else {
            // No terminator: copy the rest verbatim (no further entity can
            // start in it without an ampersand we would have found).
            out.push_str(after);
            return out;
        };
        if semi == 0 || semi > MAX_ENTITY_BODY {
            // Bare "&" or an implausibly long name: keep it literal and let
            // the scanner continue after this ampersand.
            out.push('&');
            rest = candidate;
            continue;
        }
        let body = &candidate[..semi];
        let decoded = if let Some(num) = body.strip_prefix('#') {
            let code = if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
                u32::from_str_radix(hex, 16).ok()
            } else {
                num.parse::<u32>().ok()
            };
            code.and_then(char::from_u32).map(|c| {
                if c == '\u{a0}' {
                    " ".to_string()
                } else if c.is_control() && !matches!(c, '\n' | '\r' | '\t') {
                    String::new()
                } else {
                    c.to_string()
                }
            })
        } else {
            crate::htmlentities::named(body).map(str::to_string)
        };
        match decoded {
            Some(value) => {
                out.push_str(&value);
                rest = &candidate[semi + 1..];
            }
            None => {
                // Unknown name / invalid code point: keep the original `&`.
                out.push('&');
                rest = candidate;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Strip all HTML tags for plain-text fallback (tags are deleted outright;
/// entities are decoded uniformly).
pub fn strip_html_tags(s: &str) -> String {
    decode_entities(&RE_TAG.replace_all(s, ""))
}

/// Decode HTML entities in URLs extracted from HTML attributes.
/// Feeds embed `&amp;` etc. in src/href; using them raw corrupts signed
/// query strings (weibo CDN returns 403). Mirrors rsstt's html.unescape step.
/// Semantically different from decode_entities: this function targets URL
/// attribute decoding (numeric references first, no characters dropped, only
/// the markup-critical names), while decode_entities targets visible text
/// (HTML5 names in a single pass, whitespace control characters kept,
/// other control characters dropped).
pub fn unescape_html(s: &str) -> String {
    static RE_ENTITY_NUM: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"&#(\d+);").unwrap());
    static RE_ENTITY_HEX: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)&#x([0-9a-f]+);").unwrap());
    let s = RE_ENTITY_NUM
        .replace_all(s, |caps: &regex::Captures| {
            caps[1]
                .parse::<u32>()
                .ok()
                .and_then(char::from_u32)
                .map(|c| c.to_string())
                .unwrap_or_else(|| caps[0].to_string())
        })
        .to_string();
    let s = RE_ENTITY_HEX
        .replace_all(&s, |caps: &regex::Captures| {
            u32::from_str_radix(&caps[1], 16)
                .ok()
                .and_then(char::from_u32)
                .map(|c| c.to_string())
                .unwrap_or_else(|| caps[0].to_string())
        })
        .to_string();
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// Extract one cookie value from a Set-Cookie header string by name.
/// Pure text manipulation, so it lives here alongside the other HTML/HTTP
/// text helpers. Scans every `;`-separated attribute segment: the target
/// cookie is not guaranteed to be first (e.g. `guest_id` may precede `ct0`,
/// and some servers prepend attributes before it). Shared by the X ct0
/// bootstrap (platforms/x.rs) and the Weibo visitor-cookie fetch (media.rs) —
/// previously two near-identical private copies existed and had drifted
/// (one only inspected the first segment, missing later cookies).
pub fn extract_cookie_value(set_cookie: &str, name: &str) -> Option<String> {
    let prefix = format!("{}=", name);
    set_cookie.split(';').find_map(|part| {
        part.trim()
            .strip_prefix(prefix.as_str())
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string())
    })
}

// ---------------------------------------------------------------------------
// Blockquote folding
// ---------------------------------------------------------------------------

/// Unified blockquote folding rules: visible content > 200 chars uses
/// `<blockquote expandable>` (collapsed by default), ≤ 200 uses a plain
/// `<blockquote>` (shown expanded).
/// Also trims leading/trailing whitespace inside the blockquote; for the
/// RSSHub telegram channel pattern of "short header line + blank line +
/// body" (e.g. "Zhao Xin:\n\nbody"), only the \n\n after the first short
/// paragraph (≤50 visible chars) is compressed to \n, while spacing between
/// other paragraphs is kept as-is.
/// Called at every send funnel point to ensure consistent behavior across
/// the whole project.
///
/// Nested blockquotes (which can appear in any RSS source's HTML) are
/// handled with a positional scan that counts depth to locate the outermost
/// closing tag — the non-greedy regex `(.*?)</blockquote>` would truncate
/// at the first inner closing tag, leaving an orphan `</blockquote>` that
/// makes Telegram's parse entities fail.
pub fn normalize_blockquote_fold(html: &str) -> String {
    const FOLD_THRESHOLD: usize = 200;
    const HEADER_MAX_VISIBLE: usize = 50;
    static RE_BQ_OPEN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)<blockquote\b[^>]*>").unwrap()
    });
    static RE_BQ_CLOSE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)</blockquote\s*>").unwrap()
    });

    let mut out = String::with_capacity(html.len());
    let mut pos = 0usize;
    while let Some(open) = RE_BQ_OPEN.find(&html[pos..]) {
        let open_start = pos + open.start();
        let inner_start = pos + open.end();
        out.push_str(&html[pos..open_start]);
        // Depth-counting scan to locate the outermost closing tag (when
        // nested, depth increments then decrements).
        let mut depth = 1usize;
        let mut cursor = inner_start;
        // Start of the closing tag (inner's right boundary) and end (the
        // position where scanning continues).
        let mut close_span: Option<(usize, usize)> = None;
        while depth > 0 {
            let rest = &html[cursor..];
            let next_open = RE_BQ_OPEN.find(rest).map(|m| (cursor + m.start(), cursor + m.end(), true));
            let next_close = RE_BQ_CLOSE.find(rest).map(|m| (cursor + m.start(), cursor + m.end(), false));
            let next = match (next_open, next_close) {
                (Some(o), Some(c)) => {
                    if o.0 <= c.0 { o } else { c }
                }
                (Some(o), None) => o,
                (None, Some(c)) => c,
                (None, None) => break, // Unclosed: keep original text
            };
            if next.2 {
                depth += 1;
            } else {
                depth -= 1;
                if depth == 0 {
                    close_span = Some((next.0, next.1));
                    break;
                }
            }
            cursor = next.1;
        }
        let Some((close_start, close_end)) = close_span else {
            out.push_str(&html[open_start..]);
            pos = html.len();
            break;
        };
        let mut inner = html[inner_start..close_start].trim().to_string();
        // Targeted handling: if the first paragraph (before the first \n\n)
        // has visible length ≤ 50 units, treat it as an RSSHub TG channel
        // author header line and compress the \n\n there to \n.
        // Measured in UTF-16 code units (visible_len), consistent with the
        // rest of the pipeline — Telegram measures entity offsets the same
        // way, so an emoji-heavy header is no longer under-counted by
        // chars().count().
        if let Some(p) = inner.find("\n\n") {
            let first_para = &inner[..p];
            let first_visible = visible_len(first_para);
            if first_visible <= HEADER_MAX_VISIBLE {
                inner = format!("{}\n{}", first_para, &inner[p + 2..]);
            }
        }
        let visible = visible_len(&inner);
        if visible > FOLD_THRESHOLD {
            out.push_str(&format!("<blockquote expandable>{}</blockquote>", inner));
        } else {
            out.push_str(&format!("<blockquote>{}</blockquote>", inner));
        }
        pos = close_end;
    }
    out.push_str(&html[pos..]);
    out
}

// ---------------------------------------------------------------------------
// Message splitting
// ---------------------------------------------------------------------------

/// Split HTML text into chunks whose VISIBLE text fits within `limit` chars.
/// Splits preferentially at paragraph boundaries (\n\n), then line breaks,
/// then at safe HTML positions (after closing tags / before opening tags).
/// Blockquote regions are kept atomic (never split across chunks).
pub fn split_html(text: &str, limit: usize) -> Vec<String> {
    if visible_len(text) <= limit {
        return vec![text.to_string()];
    }

    // Pre-segment: split into "segments" at \n\n boundaries, but merge
    // segments that fall inside a <blockquote>…</blockquote> or
    // <pre>…</pre> pair so neither is split across chunks.
    let raw_paras: Vec<&str> = text.split("\n\n").collect();
    let mut segments: Vec<String> = Vec::new();
    let mut in_block = false;
    let mut block_buf = String::new();

    for para in raw_paras {
        if para.is_empty() {
            continue;
        }
        if in_block {
            block_buf.push_str("\n\n");
            block_buf.push_str(para);
            if para.contains("</blockquote>") || para.contains("</pre>") {
                in_block = false;
                segments.push(block_buf.clone());
                block_buf.clear();
            }
        } else if (para.contains("<blockquote") && !para.contains("</blockquote>"))
            || (para.contains("<pre") && !para.contains("</pre>"))
        {
            // Block element opens but doesn't close in this paragraph.
            in_block = true;
            block_buf = para.to_string();
        } else {
            segments.push(para.to_string());
        }
    }
    // Unclosed block (shouldn't happen, but be safe).
    if !block_buf.is_empty() {
        segments.push(block_buf);
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();

    for para in segments {
        let candidate = if current.is_empty() {
            para.clone()
        } else {
            format!("{}\n\n{}", current, para)
        };

        if visible_len(&candidate) <= limit {
            current = candidate;
        } else if visible_len(&para) > limit {
            // Segment alone exceeds the limit — must be split.
            // Flush any accumulated content first.
            if !current.is_empty() {
                chunks.push(current.clone());
                current.clear();
            }
            if para.contains("<pre") {
                // Code block exceeds the limit: split on \n line boundaries,
                // re-wrapping each chunk in its own wrapper tags.
                // Extract the opening tag (may contain class="language-xxx")
                // and the closing tag. A bare <pre> without an inner <code>
                // (common in external feeds) must be matched too: otherwise
                // the original tag survives the strip and ends up nested
                // inside the re-wrapped wrapper (malformed double-pre HTML).
                static RE_PRE_OPEN: std::sync::LazyLock<regex::Regex> =
                    std::sync::LazyLock::new(|| {
                        regex::Regex::new(r"(?s)<pre\b[^>]*><code\b[^>]*>").unwrap()
                    });
                static RE_PRE_BARE: std::sync::LazyLock<regex::Regex> =
                    std::sync::LazyLock::new(|| {
                        regex::Regex::new(r"(?s)<pre\b[^>]*>").unwrap()
                    });
                let code_match = RE_PRE_OPEN.find(para.as_str());
                let bare_match = RE_PRE_BARE.find(para.as_str());
                let open_match = code_match.as_ref().or(bare_match.as_ref());
                let (open_tag, close_tag) = if let Some(m) = code_match.as_ref() {
                    (m.as_str().to_string(), "</code></pre>".to_string())
                } else if let Some(m) = bare_match.as_ref() {
                    (m.as_str().to_string(), "</pre>".to_string())
                } else {
                    ("<pre><code>".to_string(), "</code></pre>".to_string())
                };
                let pre_start = open_match.map(|m| m.start()).unwrap_or(0);
                let pre_open_end = open_match.map(|m| m.end()).unwrap_or(pre_start);
                let close_span = RE_PRE_CLOSE_TAG
                    .find(&para[pre_open_end..])
                    .map(|m| (pre_open_end + m.start(), pre_open_end + m.end()));
                // Text after </pre> in the same paragraph must stay outside
                // the re-wrapped code block. Without this, "…</pre> trailing"
                // renders trailing prose as code.
                let (block_end, suffix) = match close_span {
                    Some((_, end)) => (end, &para[end..]),
                    None => (para.len(), ""),
                };
                let prefix = para[..pre_start].trim_end_matches('\n').to_string();
                let block = &para[pre_start..block_end];
                let inner = {
                    let stripped = if code_match.is_some() {
                        RE_PRE_OPEN.replace_all(block, "").to_string()
                    } else {
                        RE_PRE_BARE.replace_all(block, "").to_string()
                    };
                    stripped.replace(&close_tag, "")
                };
                let inner_limit = limit.saturating_sub(
                    utf16_len(&open_tag) + utf16_len(&close_tag),
                );
                // Split by lines and bin-pack greedily; when a single line
                // itself exceeds the limit, hard-split it on UTF-16
                // boundaries (otherwise an oversized line would pass through
                // whole → chunk exceeds 4096 → deterministic TG 400 failure
                // → outbox retries fail every round → entry dropped after
                // 72h).
                let mut code_chunks: Vec<String> = Vec::new();
                let mut cur = String::new();
                for line in inner.split('\n') {
                    // Line-level expansion: oversized lines are cut into
                    // multiple segments that join the packing one by one.
                    let mut rest = line;
                    while utf16_len(rest) > inner_limit {
                        let head = take_utf16_prefix(rest, inner_limit);
                        let candidate = if cur.is_empty() {
                            head.to_string()
                        } else {
                            format!("{}\n{}", cur, head)
                        };
                        if utf16_len(&candidate) <= inner_limit {
                            cur = candidate;
                        } else {
                            code_chunks.push(std::mem::take(&mut cur));
                            cur = head.to_string();
                        }
                        rest = &rest[head.len()..];
                    }
                    // When a line is fully consumed by hard cuts (its length
                    // is an exact multiple of inner_limit), nothing remains;
                    // skip it.
                    if !rest.is_empty() {
                        let candidate = if cur.is_empty() {
                            rest.to_string()
                        } else {
                            format!("{}\n{}", cur, rest)
                        };
                        if utf16_len(&candidate) <= inner_limit {
                            cur = candidate;
                        } else {
                            code_chunks.push(std::mem::take(&mut cur));
                            cur = rest.to_string();
                        }
                    }
                }
                if !cur.is_empty() {
                    code_chunks.push(cur);
                }
                // Prose before the <pre> block. When it exceeds the limit it
                // must be split (mirroring the blockquote branch below), not
                // dropped — the old `visible_len(&prefix) <= limit` guard
                // silently discarded an oversized prefix, losing content with
                // no log (e.g. a long intro before a code block, especially
                // when a caption is cut at CAPTION_LIMIT=1024).
                if !prefix.is_empty() {
                    if visible_len(&prefix) <= limit {
                        chunks.push(prefix);
                    } else {
                        for sub in split_html_safe(&prefix, limit) {
                            chunks.push(sub);
                        }
                    }
                }
                for c in &code_chunks {
                    chunks.push(format!("{}{}{}", open_tag, c, close_tag));
                }
                if !suffix.trim().is_empty() {
                    // Same paragraph after the </pre>: keep it outside the
                    // code block and split it normally (it may contain another
                    // block element).
                    chunks.extend(split_html(suffix, limit));
                }
            } else if RE_BQ_OPEN_TAG.is_match(&para) {
                const BQ_OPEN: &str = "<blockquote expandable>";
                const BQ_CLOSE: &str = "</blockquote>";

                // Strip the original-text prefix stuck in front of the quote
                // block (Weibo retweet: the original text and the quote
                // block are joined by a single \n, and pre-segmentation
                // splits on \n\n, so both belong to the same paragraph).
                // The prefix must stay outside the quote block; otherwise,
                // when splitting for the limit, the original text would get
                // wrapped inside the quote block. Find the balanced outer
                // close (nested quotes are counted) and keep any suffix after
                // it outside as well.
                let open_match = RE_BQ_OPEN_TAG.find(para.as_str());
                let bq_start = open_match.map(|m| m.start()).unwrap_or(0);
                let open_end = open_match.map(|m| m.end()).unwrap_or(bq_start);
                let close_span =
                    find_balanced_close(&para, open_end, &RE_BQ_OPEN_TAG, &RE_BQ_CLOSE_TAG);
                let (inner_end, suffix) = match close_span {
                    Some((close_start, close_end)) => (close_start, &para[close_end..]),
                    None => (para.len(), ""),
                };
                let prefix = para[..bq_start].trim_end_matches('\n').to_string();
                let inner = &para[open_end..inner_end];
                let inner_limit = limit.saturating_sub(
                    BQ_OPEN.chars().count() + BQ_CLOSE.chars().count(),
                );

                // Budget available to the quote block: if there is an
                // original-text prefix, deduct the prefix length (+1 for the
                // newline) so the first chunk of the quote block can share a
                // message with the original text.
                // When the budget is too small (<100 visible chars), do not
                // merge, to avoid the quote block being cut too finely.
                let prefix_visible = visible_len(&prefix);
                let budget = if prefix.is_empty() {
                    inner_limit
                } else {
                    inner_limit.saturating_sub(prefix_visible + 1)
                };
                let merge_first = !prefix.is_empty()
                    && prefix_visible <= limit
                    && budget >= 100;

                let subs = if merge_first {
                    split_html(inner, budget)
                } else {
                    split_html(inner, inner_limit)
                };

                if prefix.is_empty() {
                    // No prefix: wrap each chunk directly.
                    for sub in &subs {
                        chunks.push(format!("{}{}{}", BQ_OPEN, sub, BQ_CLOSE));
                    }
                } else if merge_first {
                    // Merge the prefix + the first chunk of the quote block
                    // into one message (separated by a single \n, consistent
                    // with the spacing between original text and quote block
                    // in the source data).
                    let first_wrapped = format!(
                        "{}{}{}",
                        BQ_OPEN,
                        subs.first().map(String::as_str).unwrap_or(""),
                        BQ_CLOSE
                    );
                    chunks.push(format!("{}\n{}", prefix, first_wrapped));
                    for sub in subs.iter().skip(1) {
                        chunks.push(format!("{}{}{}", BQ_OPEN, sub, BQ_CLOSE));
                    }
                } else {
                    // Prefix too long or budget insufficient: send the prefix
                    // alone, and wrap the quote block chunk by chunk.
                    if prefix_visible <= limit {
                        chunks.push(prefix.clone());
                    } else {
                        for sub in split_html_safe(&prefix, limit) {
                            chunks.push(sub);
                        }
                    }
                    for sub in &subs {
                        chunks.push(format!("{}{}{}", BQ_OPEN, sub, BQ_CLOSE));
                    }
                }
                if !suffix.trim().is_empty() {
                    // Content after the matching </blockquote> belongs outside
                    // the quote; re-split it normally in case it is oversized
                    // or contains another block element.
                    chunks.extend(split_html(suffix, limit));
                }
            } else {
                // Non-blockquote oversized segment.
                if !current.is_empty() {
                    chunks.push(current.clone());
                    current.clear();
                }
                for sub in split_html_safe(&para, limit) {
                    chunks.push(sub);
                }
            }
        } else {
            // Segment fits within the limit but not alongside current.
            if !current.is_empty() {
                chunks.push(current.clone());
                current.clear();
            }
            current = para;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    rebalance_chunks(&mut chunks);
    chunks
}

/// Close tags left open at the end of each chunk and re-open them at the
/// start of the next one. The paragraph segmentation above can cut inside an
/// anchor whose text spans a blank line, or inside a nested blockquote, and
/// the resulting chunks would be rejected by Telegram's HTML parser (the
/// text path degrades to plain text; the media caption path loses the
/// media). Void tags are never carried.
fn rebalance_chunks(chunks: &mut [String]) {
    let mut carried: Vec<String> = Vec::new();
    for i in 0..chunks.len() {
        if !carried.is_empty() {
            let mut s = String::new();
            for tag in carried.drain(..) {
                s.push_str(&tag);
            }
            s.push_str(&chunks[i]);
            chunks[i] = s;
        }
        if i + 1 == chunks.len() {
            break;
        }
        // usize::MAX scans the whole chunk; only the final stack is needed.
        let scan = scan_split_candidates(&chunks[i], usize::MAX);
        if scan.end_stack.is_empty() {
            continue;
        }
        let mut closers = String::new();
        for (name, _) in scan.end_stack.iter().rev() {
            closers.push_str(&format!("</{}>", name));
        }
        chunks[i].push_str(&closers);
        carried = scan.end_stack.into_iter().map(|(_, tag)| tag).collect();
    }
}

/// HTML void elements: they never have a closing tag, so they must never be
/// pushed onto the split-inheritance stack.
const VOID_TAGS: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// A safe split position inside the scan window, with the tag-balance
/// fragments needed to keep both sides well-formed on their own.
struct CutCandidate {
    /// Byte offset where the head chunk ends (always > 0).
    pos: usize,
    /// Full opening tags to prepend to the tail chunk (outermost first).
    openers: String,
    /// Closing tags to append to the head chunk (innermost first).
    closers: String,
}

impl CutCandidate {
    fn at(pos: usize, stack: &[(String, String)]) -> Self {
        Self {
            pos,
            openers: stack.iter().map(|(_, tag)| tag.as_str()).collect(),
            closers: stack
                .iter()
                .rev()
                .map(|(name, _)| format!("</{name}>"))
                .collect(),
        }
    }
}

/// Lowercased tag name of a tag token (`<name …>` or `</name>`), None when the
/// token carries no name (`<>`).
fn tag_name(tag: &str) -> Option<String> {
    let inner = tag.strip_prefix('<')?.strip_suffix('>')?;
    let inner = inner.strip_prefix('/').unwrap_or(inner);
    let name: String = inner.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
    (!name.is_empty()).then(|| name.to_lowercase())
}

/// Result of scanning one split window.
struct SplitScan {
    /// Safe-position candidates by priority (see `scan_split_candidates`).
    best: [Option<CutCandidate>; 4],
    /// Open tags still spanning the window end: their fragments are needed to
    /// balance the raw fallback cut.
    end_stack: Vec<(String, String)>,
    /// Largest byte offset a raw fallback cut can use without landing inside
    /// a tag (an unterminated tag shrinks this to the tag's start).
    window_end: usize,
}

/// True when `s` starts with `&` and could still grow into a complete entity
/// reference — so the scan window ended inside an entity and a raw cut must
/// back off before the `&`. A body containing `;` is a completed (or dead)
/// reference, never a pending prefix.
fn is_entity_prefix(s: &str) -> bool {
    let Some(body) = s.strip_prefix('&') else {
        return false;
    };
    if body.is_empty() || body.contains(';') {
        return body.is_empty();
    }
    if let Some(hex) = body.strip_prefix('#') {
        if let Some(x) = hex.strip_prefix(['x', 'X']) {
            return x.chars().all(|c| c.is_ascii_hexdigit());
        }
        return hex.chars().all(|c| c.is_ascii_digit());
    }
    let mut chars = body.chars();
    chars
        .next()
        .map(|c| c.is_ascii_alphabetic())
        .unwrap_or(false)
        && chars.all(|c| c.is_ascii_alphanumeric())
}

/// Back a raw cut position off the end of `window[..pos]` when the text there
/// ends inside a partial entity reference (`&amp` without the `;` yet), so a
/// fallback split never amputates an entity.
fn entity_backoff(text: &str, pos: usize) -> usize {
    let head = &text[..pos];
    match head.rfind('&') {
        Some(amp) if is_entity_prefix(&head[amp..]) => amp,
        _ => pos,
    }
}

/// Scan the first `limit` UTF-16 units of `text` for safe split positions.
/// A position is safe only when it sits outside every tag and every entity
/// reference, so no chunk can ever contain half of `<a href="…">` or `&amp;`.
/// Returns candidates by priority: 0 = right after a closing tag, 1 = right
/// before an opening tag, 2 = after whitespace in text, 3 = after a completed
/// atomic token (entity, void or self-closing tag). The last occurrence
/// within each priority wins. Text runs without whitespace (e.g. a long CJK
/// anchor text) get no candidate here; they fall back to the raw cut, which
/// `split_html_safe` balances with the stack state reported in `end_stack`.
fn scan_split_candidates(text: &str, limit: usize) -> SplitScan {
    let window = take_utf16_prefix(text, limit);
    let mut best: [Option<CutCandidate>; 4] = Default::default();
    // Open tags spanning the current scan position: (name, full opening tag).
    let mut stack: Vec<(String, String)> = Vec::new();
    let mut i = 0usize;
    while i < window.len() {
        let rest = &window[i..];
        if rest.starts_with('<') {
            // A raw '<' only starts a tag in this pipeline: visible text is
            // escape_html'd (`&lt;`) and attribute values escape '>' as well.
            let Some(gt) = rest.find('>') else {
                // Unterminated tag: the raw cut cannot pass its start, and no
                // further safe position exists in this window. Drop any
                // candidate that would land inside the unterminated tag.
                for slot in best.iter_mut() {
                    if slot.as_ref().is_some_and(|c| c.pos > i) {
                        *slot = None;
                    }
                }
                return SplitScan {
                    best,
                    end_stack: stack,
                    window_end: i,
                };
            };
            let tag = &rest[..gt + 1];
            let end = i + gt + 1;
            match tag_name(tag) {
                // Unnamed token (`<>`): inert, safe to split after.
                None => best[3] = Some(CutCandidate::at(end, &stack)),
                Some(name) if tag.starts_with("</") => {
                    // Close: pop the matching open (implicitly closing any
                    // unclosed inner tags). Orphan closes leave the stack.
                    if let Some(idx) = stack.iter().rposition(|(n, _)| *n == name) {
                        stack.truncate(idx);
                    }
                    best[0] = Some(CutCandidate::at(end, &stack));
                }
                Some(name) if tag.ends_with("/>") || VOID_TAGS.contains(&name.as_str()) => {
                    // Self-contained opening token: nothing left unbalanced.
                    if i > 0 {
                        best[1] = Some(CutCandidate::at(i, &stack));
                    }
                    best[3] = Some(CutCandidate::at(end, &stack));
                }
                Some(name) => {
                    // Opening tag that demands a close: split before it, never
                    // right after it (the inherited opener would cost more
                    // than such a cut consumes, risking zero progress).
                    if i > 0 {
                        best[1] = Some(CutCandidate::at(i, &stack));
                    }
                    stack.push((name, tag.to_string()));
                }
            }
            i = end;
            continue;
        }
        if let Some(m) = RE_ENTITY_REF.find(rest) {
            i += m.end();
            best[3] = Some(CutCandidate::at(i, &stack));
            continue;
        }
        let c = rest.chars().next().unwrap();
        let next = i + c.len_utf8();
        if c.is_whitespace() {
            best[2] = Some(CutCandidate::at(next, &stack));
        }
        i = next;
    }
    SplitScan {
        best,
        end_stack: stack,
        window_end: window.len(),
    }
}

/// Split a single HTML paragraph that exceeds `limit` at safe positions:
/// after closing tags, before opening tags, or after whitespace — never
/// inside a tag or an entity reference. Tags still open at the cut are
/// balanced by inheritance: the head chunk gets explicit closers and the tail
/// chunk is re-opened with the same opening tags, so every chunk is
/// well-formed Telegram HTML on its own. Text runs with no whitespace (a long
/// CJK link text) fall back to a raw UTF-16 window cut, which is balanced the
/// same way via the scan's end-of-window stack; the raw cut also backs off a
/// pending entity so it never amputates `&amp`. A cut is taken at the limit
/// itself when the window is pure text (nothing to balance).
pub fn split_html_safe(text: &str, limit: usize) -> Vec<String> {
    // Defense: limit=0 would cause a zero-progress infinite loop (all
    // current call sites use constants, so this cannot trigger).
    let limit = limit.max(1);
    let mut chunks: Vec<String> = Vec::new();
    let mut remaining = text.to_string();

    // Absolute convergence guard: each iteration consumes at least one unit
    // from `remaining`, so this can never trip for real inputs, but it turns
    // any theoretical non-progress (pathological deep nesting vs a tiny
    // limit) into a benign over-long tail instead of an infinite loop.
    let mut iters = 0usize;
    while utf16_len(&remaining) > limit {
        iters += 1;
        if iters > 200_000 {
            chunks.push(remaining);
            return chunks;
        }
        let scan = scan_split_candidates(&remaining, limit);
        // A safe candidate is usable only when the head it produces stays
        // within `limit` once its re-wrapped closers are appended, AND the
        // consumed prefix is longer than the openers re-prepended to the tail
        // (otherwise the same fragment would be split forever).
        let usable = |c: &&CutCandidate| {
            utf16_len(&remaining[..c.pos]) + utf16_len(&c.closers) <= limit
                && utf16_len(&remaining[..c.pos]) > utf16_len(&c.openers)
        };
        if let Some(c) = scan.best.iter().flatten().find(usable) {
            chunks.push(format!("{}{}", &remaining[..c.pos], c.closers));
            remaining = format!("{}{}", c.openers, &remaining[c.pos..]);
            continue;
        }
        // Raw fallback for a text run with no whitespace (e.g. one long CJK
        // link text): cut at the UTF-16 window boundary minus the space the
        // appended closers need, never inside an unterminated tag, and back
        // off a pending entity so the head's end never splits `&amp`. Balance
        // both sides with the stack that spans the cut.
        let openers: String = scan.end_stack.iter().map(|(_, t)| t.as_str()).collect();
        let closers: String = scan
            .end_stack
            .iter()
            .rev()
            .map(|(n, _)| format!("</{n}>"))
            .collect();
        // Reserve room for the closers so the head stays within `limit`; the
        // reserve is capped at half the limit so the window keeps real width.
        let reserve = utf16_len(&closers).min(limit / 2);
        let raw_window = take_utf16_prefix(&remaining, limit - reserve)
            .len()
            .min(scan.window_end);
        let mut cut = entity_backoff(&remaining, raw_window);
        // Guarantee strict progress: the consumed prefix must outweigh the
        // openers we hand to the tail. (Deep nesting under a tiny limit is the
        // only way to hit this; accept a slightly over-limit head to advance.)
        if utf16_len(&remaining[..cut]) <= utf16_len(&openers) {
            cut = entity_backoff(
                &remaining,
                take_utf16_prefix(&remaining, utf16_len(&openers) + 1).len(),
            );
        }
        if cut == 0 {
            cut = remaining
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
        }
        chunks.push(format!("{}{}", &remaining[..cut], closers));
        remaining = format!("{}{}", openers, &remaining[cut..]);
    }

    if !remaining.is_empty() {
        chunks.push(remaining);
    }
    chunks
}

/// Merge adjacent `<blockquote expandable>` fragments back into a single blockquote.
///
/// When `split_html` splits a large blockquote at CAPTION_LIMIT (1024), each
/// fragment gets its own wrapper tags. Before re-splitting the overflow at
/// SAFE_LIMIT (4096), merge them so the follow-up message shows one collapsible
/// block instead of many separate ones.
///
/// Pattern: `</blockquote>\n\n<blockquote expandable>` → `\n\n`
/// (strips the closing/opening pair, joining inner content directly).
pub fn merge_adjacent_blockquotes(text: &str) -> String {
    const BQ_OPEN: &str = "<blockquote expandable>";
    const BQ_CLOSE: &str = "</blockquote>";
    let seam = format!("{}\n\n{}", BQ_CLOSE, BQ_OPEN);
    text.replace(&seam, "\n\n")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ===== utf16_len =====

    #[test]
    fn utf16_len_ascii_and_cjk() {
        assert_eq!(utf16_len("hello"), 5);
        assert_eq!(utf16_len("中文"), 2); // BMP chars are 1 unit each
    }

    #[test]
    fn utf16_len_emoji_counts_two_units() {
        // emoji is 1 char in Rust, but Telegram counts it as 2 UTF-16 units.
        assert_eq!(utf16_len("😎"), 2);
        assert_eq!(utf16_len("a😎b"), 4);
    }

    // ===== take_utf16_prefix =====

    #[test]
    fn take_utf16_prefix_never_splits_char() {
        // "😎" takes 2 units: with limit 1 we cannot cut half an emoji, so
        // at least 1 complete char is kept.
        assert_eq!(take_utf16_prefix("😎abc", 1), "😎");
        // Limit 3: emoji(2) + 'a'(1) fits exactly; 'b' does not fit.
        assert_eq!(take_utf16_prefix("😎abc", 3), "😎a");
        // Limit sufficient: the whole string is returned.
        assert_eq!(take_utf16_prefix("abc", 10), "abc");
    }

    // ===== decode_entities =====

    #[test]
    fn decodes_html5_named_entities() {
        assert_eq!(
            decode_entities("A &mdash; B &hellip; C &rsquo;"),
            "A — B … C ’"
        );
        assert_eq!(decode_entities("x&nbsp;y &copy;"), "x y ©");
        // HTML5-only names and multi-codepoint replacements.
        assert_eq!(decode_entities("&check; &NotEqualTilde;"), "✓ \u{2242}\u{338}");
        assert_eq!(decode_entities("a&NewLine;b"), "a\nb");
        assert_eq!(decode_entities("&amp;lt;"), "&lt;", "entity must decode exactly once");
    }

    #[test]
    fn decodes_numeric_entities_and_keeps_whitespace_controls() {
        assert_eq!(decode_entities("a&#10;b&#9;c"), "a\nb\tc");
        assert_eq!(decode_entities("a&#x4E2D;b"), "a中b");
        assert_eq!(decode_entities("a&#160;b"), "a b", "numeric nbsp becomes a plain space");
        // NUL and other non-whitespace controls are dropped.
        assert_eq!(decode_entities("a&#0;b&#7;c"), "abc");
        // Invalid references stay literal.
        assert_eq!(decode_entities("&unknown; &#x110000;"), "&unknown; &#x110000;");
    }

    // ===== visible_len =====

    #[test]
    fn visible_len_ignores_tags() {
        assert_eq!(visible_len("<b>hello</b>"), 5);
        assert_eq!(visible_len(r#"<a href="https://example.com">链接</a>"#), 2);
        assert_eq!(visible_len("😎"), 2);
    }

    // ===== escape_html =====

    #[test]
    fn escape_html_escapes_special_chars() {
        assert_eq!(escape_html("a < b & c > d"), "a &lt; b &amp; c &gt; d");
    }

    // ===== split_html =====

    #[test]
    fn split_html_short_text_single_chunk() {
        let out = split_html("short text", 100);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], "short text");
    }

    #[test]
    fn split_html_respects_limit_per_chunk() {
        // 4 paragraphs of 10 chars each, limit 25 → each chunk holds at most
        // 2 paragraphs.
        let text = ["aaaaaaaaaa", "bbbbbbbbbb", "cccccccccc", "dddddddddd"].join("\n\n");
        let out = split_html(&text, 25);
        assert!(out.len() >= 2, "expected multiple chunks, got {}", out.len());
        for c in &out {
            assert!(visible_len(c) <= 25, "chunk exceeds limit: {:?}", c);
        }
    }

    #[test]
    fn split_html_preserves_all_visible_content() {
        let paras = ["第一段内容", "第二段内容", "第三段内容"];
        let text = paras.join("\n\n");
        let out = split_html(&text, 8);
        // All visible content should appear in some chunk (lossless at the
        // paragraph level).
        for p in &paras {
            assert!(out.iter().any(|c| c.contains(p)), "missing paragraph: {}", p);
        }
    }

    #[test]
    fn split_html_keeps_blockquote_atomic() {
        // A blockquote containing \n\n inside must be kept intact in a
        // single chunk (atomicity) when the limit can hold the whole block;
        // it must not be torn apart.
        let bq = "<blockquote>引用第一行\n\n引用第二行</blockquote>";
        let text = format!("前置段落\n\n{}\n\n后置段落", bq);
        let out = split_html(&text, 100);
        // Exactly one chunk contains the blockquote, with both lines inside.
        let holders: Vec<_> = out.iter().filter(|c| c.contains("<blockquote")).collect();
        assert_eq!(holders.len(), 1, "blockquote spread across chunks: {:?}", out);
        assert!(holders[0].contains("引用第一行"), "chunk: {:?}", holders[0]);
        assert!(holders[0].contains("引用第二行"), "chunk: {:?}", holders[0]);
        assert!(holders[0].contains("</blockquote>"), "chunk: {:?}", holders[0]);
    }

    #[test]
    fn split_html_oversized_blockquote_split_and_rewrapped() {
        // When a blockquote exceeds the limit it is split at \n\n paragraph
        // boundaries, and each fragment is re-wrapped as a complete
        // blockquote (well-formed), with no visible content lost.
        // Overhead = len("<blockquote expandable>") + len("</blockquote>") = 36.
        // 40 chars per paragraph, total visible 80 > limit=78,
        // inner_limit = 78-36 = 42 >= 40.
        let para1 = "A".repeat(40);
        let para2 = "B".repeat(40);
        let bq = format!("<blockquote>{}\n\n{}</blockquote>", para1, para2);
        let out = split_html(&bq, 78);
        // Should be split into 2 chunks (each paragraph wrapped on its own).
        assert!(out.len() >= 2, "expected >=2 chunks, got {}: {:?}", out.len(), out);
        // No chunk should contain only half a blockquote (well-formedness).
        for c in &out {
            let has_open = c.contains("<blockquote");
            let has_close = c.contains("</blockquote>");
            assert_eq!(has_open, has_close, "half blockquote in chunk: {:?}", c);
        }
        // Visible content of both paragraphs is preserved.
        let joined = out.join("\u{FFFC}");
        assert!(joined.contains(&para1), "out: {:?}", out);
        assert!(joined.contains(&para2), "out: {:?}", out);
    }

    #[test]
    fn split_html_keeps_pre_block_atomic() {
        // A pre code block containing a blank line (\n\n) inside must be
        // kept intact in a single chunk (atomicity) when the limit can hold
        // the whole block; it must not be torn apart.
        let pre = "<pre><code>line1\n\nline2</code></pre>";
        let text = format!("前置段落\n\n{}\n\n后置段落", pre);
        let out = split_html(&text, 100);
        let holders: Vec<_> = out.iter().filter(|c| c.contains("<pre>")).collect();
        assert_eq!(holders.len(), 1, "pre block spread across chunks: {:?}", out);
        assert!(holders[0].contains("line1"), "chunk: {:?}", holders[0]);
        assert!(holders[0].contains("line2"), "chunk: {:?}", holders[0]);
        assert!(holders[0].contains("</pre>"), "chunk: {:?}", holders[0]);
    }

    #[test]
    fn split_html_oversized_pre_block_split_and_rewrapped() {
        // When a pre block exceeds the limit it is split by lines, and each
        // fragment is re-wrapped as a complete <pre><code> (well-formed),
        // with no code content lost.
        // Overhead = len("<pre><code>") + len("</code></pre>") = 24.
        // 20 chars per line × 3 lines, total visible 64 > limit=50,
        // inner_limit = 50-24 = 26 >= 20.
        let line1 = "A".repeat(20);
        let line2 = "B".repeat(20);
        let line3 = "C".repeat(20);
        let pre = format!("<pre><code>{}\n\n{}\n\n{}</code></pre>", line1, line2, line3);
        let out = split_html(&pre, 50);
        // Should be split into multiple chunks.
        assert!(out.len() >= 2, "expected >=2 chunks, got {}: {:?}", out.len(), out);
        for c in &out {
            let has_open = c.contains("<pre>");
            let has_close = c.contains("</pre>");
            assert_eq!(has_open, has_close, "half pre block in chunk: {:?}", c);
        }
        let joined = out.join("\u{FFFC}");
        assert!(joined.contains(&line1), "out: {:?}", out);
        assert!(joined.contains(&line2), "out: {:?}", out);
        assert!(joined.contains(&line3), "out: {:?}", out);
    }

    #[test]
    fn split_html_keeps_text_after_oversized_blockquote_outside() {
        let quote = "引".repeat(200);
        let text = format!("<blockquote>{quote}</blockquote>AFTER");
        let out = split_html(&text, 80);
        let tail = out.last().expect("at least one chunk");
        assert!(tail.contains("AFTER"), "suffix lost: {:?}", out);
        assert!(!tail.contains("<blockquote"), "suffix wrapped into quote: {tail:?}");
        assert!(out.iter().any(|c| c.contains("引")), "quote content lost: {:?}", out);
    }

    #[test]
    fn split_html_keeps_text_after_oversized_pre_outside() {
        let code = "line\n".repeat(80);
        let text = format!("<pre>{code}</pre>AFTER");
        let out = split_html(&text, 60);
        let tail = out.last().expect("at least one chunk");
        assert!(tail.contains("AFTER"), "suffix lost: {:?}", out);
        assert!(!tail.contains("<pre"), "suffix wrapped into pre: {tail:?}");
        assert!(out.iter().any(|c| c.contains("line")), "code content lost: {:?}", out);
    }

    #[test]
    fn split_html_long_single_paragraph_hard_split() {
        // Single oversized paragraph (no \n\n to split on): it must still
        // be cut, and every chunk must stay within the limit.
        let text = "字".repeat(100);
        let out = split_html(&text, 30);
        assert!(out.len() >= 4);
        for c in &out {
            assert!(visible_len(c) <= 30, "chunk exceeds limit");
        }
    }

    #[test]
    fn split_html_emoji_limit_accounting() {
        // All emoji: visible length 2 units each. The limit is judged in
        // UTF-16 units.
        let text = ["😎😎😎😎😎"; 4].join("\n\n"); // 10 units per paragraph
        let out = split_html(&text, 10);
        // Each paragraph is exactly 10 units; two paragraphs must not be
        // merged (merged would be 20 > 10).
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn split_pre_oversized_single_line_hard_split() {
        // An oversized single line in a pre code block (no \n) must be
        // hard-split on UTF-16 boundaries; it must not pass through whole
        // and produce a chunk over 4096 (H1 regression test).
        let long_line = "x".repeat(9000); // Oversized single line with no newline
        let html = format!("<pre><code>{}</code></pre>", long_line);
        let chunks = split_html(&html, 4096);
        assert!(chunks.len() > 1, "oversized single line must be split, got {} chunk(s)", chunks.len());
        for c in &chunks {
            assert!(utf16_len(c) <= 4096, "chunk exceeds limit: len={}", utf16_len(c));
            // Every chunk must be re-wrapped as valid pre/code
            assert!(c.contains("<pre"), "chunk missing pre open: {}", &c[..c.len().min(60)]);
            assert!(c.contains("</pre>"), "chunk missing pre close");
        }
    }

    // ===== normalize_blockquote_fold =====

    #[test]
    fn fold_short_blockquote_stays_plain() {
        let out = normalize_blockquote_fold("<blockquote>短引用</blockquote>");
        assert_eq!(out, "<blockquote>短引用</blockquote>");
    }

    #[test]
    fn fold_long_blockquote_becomes_expandable() {
        let inner = "长".repeat(201); // visible 201 > 200 threshold
        let out = normalize_blockquote_fold(&format!("<blockquote>{}</blockquote>", inner));
        assert!(out.starts_with("<blockquote expandable>"), "got: {}", out);
    }

    #[test]
    fn fold_compresses_short_header_paragraph_gap() {
        // RSSHub TG channel pattern: short header line (≤50 chars) + \n\n +
        // body → compressed to \n.
        let out = normalize_blockquote_fold("<blockquote>竹新社:\n\n正文内容</blockquote>");
        assert_eq!(out, "<blockquote>竹新社:\n正文内容</blockquote>");
    }

    #[test]
    fn fold_keeps_gap_for_long_header_paragraph() {
        // First paragraph exceeds 50 visible chars: not a header line, so
        // the paragraph gap is kept as-is.
        let header = "头".repeat(51);
        let input = format!("<blockquote>{}\n\n正文</blockquote>", header);
        let out = normalize_blockquote_fold(&input);
        assert!(out.contains("\n\n"), "long header should keep paragraph gap: {}", out);
    }

    #[test]
    fn fold_trims_inner_whitespace() {
        let out = normalize_blockquote_fold("<blockquote>  引用  </blockquote>");
        assert_eq!(out, "<blockquote>引用</blockquote>");
    }

    #[test]
    fn fold_preserves_expandable_input_tag() {
        // Input already has expandable: re-normalize by visible length
        // (short content folds back to the plain tag).
        let out = normalize_blockquote_fold("<blockquote expandable>短</blockquote>");
        assert_eq!(out, "<blockquote>短</blockquote>");
    }

    #[test]
    fn fold_nested_blockquote_no_orphan_close_tag() {
        // Nested blockquote: the positional scan must locate the outermost
        // closing tag and must not truncate at the first inner
        // </blockquote>, leaving an orphan closing tag (M10 regression test).
        let out = normalize_blockquote_fold("<blockquote>外层 <blockquote>内层</blockquote> 结尾</blockquote>");
        let opens = out.matches("<blockquote").count();
        let closes = out.matches("</blockquote>").count();
        assert_eq!(opens, closes, "orphan close tag: {}", out);
        assert!(out.contains("内层"), "inner content lost: {}", out);
        assert!(out.contains("结尾"), "trailing content lost: {}", out);
    }

    #[test]
    fn fold_adjacent_blockquotes_both_processed() {
        // Both adjacent blockquotes must be processed (the positional scan
        // loop must advance pos correctly).
        let out = normalize_blockquote_fold("<blockquote>甲</blockquote><blockquote>乙</blockquote>");
        assert_eq!(out.matches("</blockquote>").count(), 2, "got: {}", out);
        assert!(out.contains("甲") && out.contains("乙"), "got: {}", out);
    }

    // ===== split_html_safe: tag/entity atomicity (P1 batch A) =====

    fn tag_balanced(c: &str) -> bool {
        c.matches("<a ").count() == c.matches("</a>").count()
            && c.matches("<b>").count() == c.matches("</b>").count()
            && c.matches("<pre").count() == c.matches("</pre>").count()
    }

    #[test]
    fn split_safe_long_anchor_never_splits_tag() {
        // The original defect (P1#1): a single oversized anchor paragraph was
        // hard-split inside the opening `<a href=…>` tag, leaving a chunk
        // starting with a bare `href="…"`. Every chunk must now be well-formed.
        // The limit stays above the open-tag width so a split point exists.
        let inner = "字".repeat(100);
        let html = format!(r#"<a href="https://ex.com/a/b?x=1&y=2">{inner}</a>"#);
        let out = split_html_safe(&html, 60);
        assert!(out.len() > 2);
        for c in &out {
            assert!(tag_balanced(c), "unbalanced anchor chunk: {c:?}");
        }
        // Content is preserved across the balanced re-wrapped fragments (the
        // inner text is contiguous per chunk, not across chunk boundaries).
        assert_eq!(out.iter().map(|c| c.matches('字').count()).sum::<usize>(), 100);
    }

    #[test]
    fn split_safe_never_splits_entity() {
        // A chunk must never end with a partial entity reference (`&amp;` cut
        // to `&am`). The old fallback hard-cut in the middle of entities.
        let text = "ab&amp;".repeat(60);
        let out = split_html_safe(&text, 23);
        assert!(out.len() >= 5);
        for c in &out {
            assert!(!c.ends_with('&'), "chunk ends with dangling & {c}");
            for probe in ["&am", "&amp", "&#3", "&not", "&gt", "&lt"] {
                assert!(
                    !c.ends_with(probe) || c.ends_with(&format!("{probe};")),
                    "split entity {probe:?} in {c}"
                );
            }
        }
    }

    #[test]
    fn split_safe_balances_open_tags() {
        // Text split while inside an open <b> must close it in the head and
        // re-open it in the tail — never leave a chunk with a stray closer.
        let text = format!("<b>{}</b>", "x".repeat(90));
        let out = split_html_safe(&text, 20);
        for c in &out {
            assert!(
                c.matches("<b>").count() == c.matches("</b>").count(),
                "unbalanced <b>: {c}"
            );
        }
        assert!(out.concat().matches('x').count() == 90, "content lost");
    }

    #[test]
    fn split_html_oversized_bare_pre_rewraps_wellformed() {
        // End-to-end (P1#2): external feeds commonly emit bare <pre> with no
        // inner <code>. The old code matched only <pre><code>, left the
        // original <pre> inside the extracted content and re-wrapped it,
        // producing <pre><code><pre>… (double open). Every chunk must now have
        // matched pre/code counts.
        let text = format!("<pre>{}</pre>", "line of code here\n".repeat(30));
        let out = split_html(&text, 50);
        assert!(out.len() >= 2, "expected multiple chunks");
        for c in &out {
            assert_eq!(
                c.matches("<pre").count(),
                c.matches("</pre>").count(),
                "unbalanced pre: {c}"
            );
            assert!(
                !c.contains("<pre><code><pre"),
                "double pre open in chunk: {c}"
            );
        }
    }

    #[test]
    fn split_html_oversized_anchor_wellformed() {
        // End-to-end (P1#1): an anchor whose text alone exceeds the limit is
        // routed to split_html_safe via the no-tag branch; the old fallback
        // cut inside `<a href=…>`. Now every chunk must keep <a> balanced.
        let inner = "字".repeat(200);
        let html = format!(r#"<a href="https://ex.com/x?y=1">{inner}</a>"#);
        let out = split_html(&html, 60);
        assert!(out.len() >= 3);
        for c in &out {
            assert_eq!(
                c.matches("<a ").count(),
                c.matches("</a>").count(),
                "unbalanced anchor chunk: {c}"
            );
        }
    }

    #[test]
    fn split_html_balances_anchor_spanning_blank_lines() {
        // An anchor whose text contains a blank line is split at that
        // boundary by paragraph pre-segmentation. The rebalance pass must
        // close the anchor in the head chunk and re-open it in the tail, or
        // Telegram rejects the chunk (text path loses links; caption path
        // loses the media).
        let html = r#"<a href="https://ex.com/x">first paragraph

second paragraph</a> tail"#;
        let out = split_html(html, 25);
        assert!(out.len() >= 2);
        for c in &out {
            let scan = scan_split_candidates(c, usize::MAX);
            assert!(
                scan.end_stack.is_empty(),
                "unclosed tags in chunk: {c}"
            );
            assert!(visible_len(c) <= 25, "chunk exceeds limit: {c}");
            assert_eq!(c.matches("<a ").count(), c.matches("</a>").count(), "unbalanced anchor: {c}");
        }
    }

    #[test]
    fn split_html_balances_nested_blockquote() {
        // Nested blockquotes: pre-segmentation stops at the inner close, so
        // the outer open tag can end up unclosed in a chunk. Rebalancing must
        // make every chunk well-formed.
        let html = "<blockquote>outer <blockquote>inner</blockquote> text\n\nmore</blockquote> tail";
        let out = split_html(html, 20);
        assert!(out.len() >= 2);
        for c in &out {
            let scan = scan_split_candidates(c, usize::MAX);
            assert!(scan.end_stack.is_empty(), "unclosed tags in chunk: {c}");
            assert!(visible_len(c) <= 20, "chunk exceeds limit: {c}");
        }
    }
}
