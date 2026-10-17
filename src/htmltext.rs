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
/// Strip entire <a> link blocks (preprocessing before summary extraction).
pub static RE_LINK_BLOCK: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?is)<a\b[^>]*>.*?</a>").unwrap());

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

/// Decode HTML entities (common named entities + numeric references
/// &#NNN; / &#xHH;).
/// Shared uniformly by the plain-text degradation paths
/// (strip_html_to_plain / strip_html_tags).
/// Edge handling: invalid code points (surrogate range / beyond u32) keep
/// the original text; control characters (including NUL decoded from &#0;)
/// are dropped rather than injected into messages. &amp; is decoded last to
/// avoid double decoding (&amp;lt; → &lt;).
pub fn decode_entities(s: &str) -> String {
    let text = s
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ");
    static RE_NUM: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"&#([xX][0-9a-fA-F]+|\d+);").unwrap());
    let text = RE_NUM.replace_all(&text, |caps: &regex::Captures| {
        let raw = &caps[1];
        let code = if let Some(hex) = raw.strip_prefix('x').or_else(|| raw.strip_prefix('X')) {
            u32::from_str_radix(hex, 16).ok()
        } else {
            raw.parse::<u32>().ok()
        };
        match code.and_then(char::from_u32) {
            Some(c) if !c.is_control() => c.to_string(),
            Some(_) => String::new(), // Control char (incl. NUL): drop
            None => caps[0].to_string(), // Invalid code point: keep original
        }
    }).to_string();
    text.replace("&amp;", "&")
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
/// attribute decoding (numeric references first, no characters dropped),
/// while decode_entities targets visible text (&amp; decoded last to prevent
/// double decoding, control characters dropped).
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

/// Strip HTML tags and decode common entities, yielding visible plain text.
/// Tags are replaced with spaces (preserving word separation; used for
/// summaries and other visible text).
pub fn strip_html_to_plain(html: &str) -> String {
    decode_entities(&RE_TAG.replace_all(html, " "))
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
        // has visible length ≤ 50 chars, treat it as an RSSHub TG channel
        // author header line and compress the \n\n there to \n.
        if let Some(p) = inner.find("\n\n") {
            let first_para = &inner[..p];
            let first_visible = RE_TAG.replace_all(first_para, "").chars().count();
            if first_visible <= HEADER_MAX_VISIBLE {
                inner = format!("{}\n{}", first_para, &inner[p + 2..]);
            }
        }
        let visible = RE_TAG.replace_all(&inner, "").chars().count();
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
                // re-wrapping each chunk in its own <pre><code>.
                // Extract the opening tag (may contain class="language-xxx")
                // and the closing tag.
                static RE_PRE_OPEN: std::sync::LazyLock<regex::Regex> =
                    std::sync::LazyLock::new(|| {
                        regex::Regex::new(r"(?s)<pre\b[^>]*><code\b[^>]*>").unwrap()
                    });
                let open_match = RE_PRE_OPEN.find(para.as_str());
                let (open_tag, close_tag) = match open_match {
                    Some(m) => (m.as_str().to_string(), "</code></pre>".to_string()),
                    None => ("<pre><code>".to_string(), "</code></pre>".to_string()),
                };
                let pre_start = para.find("<pre").unwrap_or(0);
                let prefix = para[..pre_start].trim_end_matches('\n').to_string();
                let inner = RE_PRE_OPEN
                    .replace_all(&para[pre_start..], "")
                    .replace("</code></pre>", "");
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
            } else if para.contains("<blockquote") {
                const BQ_OPEN: &str = "<blockquote expandable>";
                const BQ_CLOSE: &str = "</blockquote>";
                // When stripping the opening tag, both <blockquote> and
                // <blockquote expandable> forms must be matched (Weibo
                // retweets produce plain <blockquote>); otherwise a leftover
                // opening tag in inner would get wrapped again, forming a
                // nested blockquote.
                static RE_BQ_OPEN_ANY: std::sync::LazyLock<regex::Regex> =
                    std::sync::LazyLock::new(|| {
                        regex::Regex::new(r"(?i)<blockquote\b[^>]*>").unwrap()
                    });

                // Strip the original-text prefix stuck in front of the quote
                // block (Weibo retweet: the original text and the quote
                // block are joined by a single \n, and pre-segmentation
                // splits on \n\n, so both belong to the same paragraph).
                // The prefix must stay outside the quote block; otherwise,
                // when splitting for the limit, the original text would get
                // wrapped inside the quote block.
                let bq_start = para.find("<blockquote").unwrap_or(0);
                let prefix = para[..bq_start].trim_end_matches('\n').to_string();
                let bq_part = &para[bq_start..];
                let inner = RE_BQ_OPEN_ANY
                    .replace_all(bq_part, "")
                    .replace(BQ_CLOSE, "");
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
                    split_html(&inner, budget)
                } else {
                    split_html(&inner, inner_limit)
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
    chunks
}

/// Split a single HTML paragraph that exceeds `limit` at safe positions:
/// after closing tags (</a>, </b>, </i>) or before opening tags (<a, <b, <i).
/// Falls back to char-boundary split if no safe position found.
pub fn split_html_safe(text: &str, limit: usize) -> Vec<String> {
    // Defense: limit=0 would cause a zero-progress infinite loop (all
    // current call sites use constants, so this cannot trigger).
    let limit = limit.max(1);
    let mut chunks: Vec<String> = Vec::new();
    let mut remaining = text.to_string();

    while utf16_len(&remaining) > limit {
        // Find the best split position within the first `limit` UTF-16 units
        // (Telegram's length unit).
        let window: String = take_utf16_prefix(&remaining, limit).to_string();

        // Try to find a safe split point (search backwards from the end).
        let mut split_pos = None;

        // Priority 1: after a closing tag like </a>, </b>, </i>, </blockquote>
        for tag in ["</blockquote>", "</a>", "</b>", "</i>"] {
            if let Some(pos) = window.rfind(tag) {
                let end = pos + tag.len();
                // Make sure we're at a char boundary.
                if window.is_char_boundary(end) {
                    split_pos = Some(end);
                    break;
                }
            }
        }

        // Priority 2: before an opening tag like <a, <b, <i
        if split_pos.is_none() {
            for tag in ["<a ", "<b>", "<i>"] {
                if let Some(pos) = window.rfind(tag) {
                    if pos > 0 && window.is_char_boundary(pos) {
                        split_pos = Some(pos);
                        break;
                    }
                }
            }
        }

        // Priority 3: at a space or newline
        if split_pos.is_none() {
            if let Some(pos) = window.rfind([' ', '\n']) {
                if window.is_char_boundary(pos + 1) {
                    split_pos = Some(pos + 1);
                }
            }
        }

        // Fallback: hard split at limit (char boundary).
        let split_at = split_pos.unwrap_or_else(|| {
            // Find the last valid char boundary at or before limit.
            let mut pos = window.len();
            while pos > 0 && !window.is_char_boundary(pos) {
                pos -= 1;
            }
            pos
        });

        chunks.push(window[..split_at].to_string());
        remaining = remaining[split_at..].to_string();
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
}
