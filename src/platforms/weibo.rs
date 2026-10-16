//! Weibo native fetcher: directly calls m.weibo.cn getIndex API instead of
//! relying on an external RSSHub instance. Requires a login cookie because
//! the visitor flow needs JS fingerprinting.
//!
//! Cookie resolution: global runtime store (`state.cookies["weibo"]`)
//! — set via the /cookies command by admins, persisted in the `settings`
//! table under `cookie:weibo`. (Since v1.0.0 there is no env seed; pure DB storage.)
//!
//! Subscription URL format: `https://weibo.com/u/<uid>` (a weibo user
//! homepage link; www/m variants also accepted).
//!
//! The fetched statuses are converted into feed-rs `Entry` objects with
//! media embedded as `<img>`/`<video>` tags in the content body, so the
//! existing media extraction / weibo detection / rendering pipeline works
//! unchanged.

use std::sync::LazyLock;

use anyhow::Context;
use chrono::{DateTime, Datelike, Utc};
use feed_rs::model::Entry;
use serde_json::Value;
use tracing::warn;

use crate::state::AppState;

// Weibo uses the mobile UA (state::MOBILE_BROWSER_UA, iPhone Safari).
// m.weibo.cn is a mobile endpoint, so a mobile UA is the most natural fit;
// the desktop UA is used by the other endpoints.

/// Static regexes: HTML cleaning runs on every weibo status.
static RE_HREF_DQ: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"(?i)href\s*=\s*"(/[^"]*)""#).unwrap());
static RE_HREF_SQ: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)href\s*=\s*'(/[^']*)'").unwrap());

// ── Markdown rendering regexes (for posts with md_render_mark == 1) ──
// RE_BR / RE_BLANK_LINES / RE_IMG_ALT / RE_IMG are identical to the ones in
// fetcher.rs; reuse the fetcher exports directly (L5 fix: previously defined
// twice in both places).
/// Protect existing <a> tags (output of absolutize_hrefs) so they are not
/// escaped during rendering.
static RE_MD_ANCHOR: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?is)<a\b[^>]*>.*?</a>").unwrap());
static RE_MD_INLINE_CODE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"`([^`\n]+)`").unwrap());
static RE_MD_BOLD: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\*\*(.+?)\*\*").unwrap());
static RE_MD_STRIKE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"~~(.+?)~~").unwrap());
static RE_MD_ITALIC: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\*([^*\n]+)\*").unwrap());
static RE_MD_LINK: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\[([^\]]*)\]\(([^)]*)\)").unwrap());
static RE_MD_OL: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^\d+[.)]\s").unwrap());
/// Markdown form of [label](sinaurl) inside the post body. The label is not
/// fixed ("View image" / "Comment image" etc.), so [^\]]+ is used as a wildcard;
/// sinaurl?u= identifies a sinaurl card link, and after decoding,
/// is_image_url filters out non-image URLs (v1.10.11).
/// Note: [^)]+ assumes the u parameter value contains no unencoded literal
/// `)` — in practice the sinaurl u value is always fully percent-encoded, so
/// this premise holds; if an unencoded `)` ever appears, the URL is truncated.
static RE_SINAURL_MD: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\[([^\]]+)\]\((https://weibo\.cn/sinaurl\?u=[^)]+)\)").unwrap());
/// HTML form of <a href="sinaurl">…label…</a> inside weibo post bodies. The
/// label is not fixed ("View image" / "Comment image" etc.), so the anchor text is
/// no longer constrained; only sinaurl?u= is used as the identifier.
/// The actual API form is <a href="sinaurl" data-hide=""><span class='url-icon'>
/// <img …></span><span class="surl-text">Comment image</span></a>; strip_ui_icons
/// removes the inner <img> but keeps the span shells, so (.*?) tolerates any
/// inner tags and captures them so the label can be extracted.
/// Note: [^"']+ assumes the href value contains no quotes — percent-encoding
/// guarantees this premise holds.
static RE_SINAURL_HTML: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"(?is)<a\b[^>]*href\s*=\s*["'](https://weibo\.cn/sinaurl\?u=[^"']+)["'][^>]*>(.*?)</a>"#).unwrap());
/// Video card links (video.weibo.com/show): the video has already been sent
/// as media, so the card link in the body is removed.
/// Tightened to the card form: must contain the url-icon icon span inside
/// (the card signature), consistent with the vote card regex (same fix as
/// #2), to avoid `.*?</a>` swallowing adjacent links under abnormal anchor
/// structures.
static RE_VIDEO_CARD_LINK: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"(?is)<a\b[^>]*href\s*=\s*["'][^"']*video\.weibo\.com/show[^"']*["'][^>]*>.*?url-icon.*?</a>"#).unwrap());
/// Vote card links (t.cn short links): the vote has already been converted
/// to a TG native Poll, so the card link in the body is removed.
/// Tightened to the vote card form: href is a t.cn short link and the anchor
/// contains the url-icon icon span inside (the card signature), to avoid
/// wrongly removing other t.cn short links that users share normally (#2 fix).
static RE_VOTE_CARD_LINK: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"(?is)<a\b[^>]*href\s*=\s*["'][^"']*t\.cn/[^"']*["'][^>]*>.*?url-icon.*?</a>"#).unwrap());

/// True when the feed URL is a weibo user homepage that should be fetched
/// natively: https://weibo.com/u/<uid> (also m.weibo.cn / www variants).
pub fn is_weibo_url(url: &str) -> bool {
    parse_uid(url).is_some()
}

/// Parse uid from a weibo user homepage URL:
/// https://weibo.com/u/<uid>, https://www.weibo.com/u/<uid>,
/// https://m.weibo.cn/u/<uid> (query/fragment stripped).
pub fn parse_uid(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?.to_lowercase();
    let is_weibo_host = host == "weibo.com"
        || host == "www.weibo.com"
        || host == "m.weibo.com"
        || host == "weibo.cn"
        || host == "m.weibo.cn"
        || host == "www.weibo.cn";
    if !is_weibo_host {
        return None;
    }
    let segs: Vec<&str> = u.path_segments()?.filter(|s| !s.is_empty()).collect();
    // /u/<uid>
    if segs.len() >= 2 && segs[0] == "u" {
        let uid = segs[1];
        if !uid.is_empty() && uid.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Some(uid.to_string());
        }
    }
    None
}

/// Registry fetch entry: parse the uid from the full feed URL, then delegate
/// to [`fetch_weibo_user`]. Called via `PlatformDef.fetch` (URL passed owned
/// so the returned future borrows only from `state`).
pub async fn fetch_by_url(
    state: &AppState,
    url: String,
) -> anyhow::Result<crate::fetcher::FetchResult> {
    let uid = parse_uid(&url)
        .ok_or_else(|| anyhow::anyhow!("invalid weibo user homepage URL: {}", url))?;
    fetch_weibo_user(state, &uid).await
}

/// Fetch a weibo user timeline and convert to FetchResult.
pub async fn fetch_weibo_user(
    state: &AppState,
    uid: &str,
) -> anyhow::Result<crate::fetcher::FetchResult> {
    let cookies = match state.get_cookie("weibo").await {
        Some(c) if !c.trim().is_empty() => c,
        _ => anyhow::bail!("weibo feeds require an admin-configured login cookie (/cookies command)"),
    };

    let api = format!(
        "https://m.weibo.cn/api/container/getIndex?type=uid&value={}&containerid=107603{}",
        uid, uid
    );
    // Use a client with automatic redirects disabled: when the cookie expires,
    // weibo may 302 → login page; following the redirect would return the login
    // page HTML and produce a "JSON parse failed" error (without the "cookie"
    // substring), so neither the expiry flag nor the notification would trigger.
    // With redirects disabled, the is_redirection() branch below actually works.
    let client = state.fetch_client_no_redirect_for_url(&api);
    let resp = client
        .get(&api)
        .header("Cookie", &cookies)
        .header("Referer", format!("https://m.weibo.cn/u/{}", uid))
        .header("User-Agent", crate::state::MOBILE_BROWSER_UA)
        .header("Accept", "application/json, text/plain, */*")
        .header("MWeibo-Pwa", "1")
        .send()
        .await
        .with_context(|| format!("weibo request failed: {}", api))?;

    let status = resp.status();
    // 302 redirect (usually to the login page) = cookie expired. The error
    // string must contain the "cookie" substring: the scheduler uses it to set
    // the expiry flag (/cookies panel status + expiry notification).
    if status.is_redirection() {
        anyhow::bail!("weibo cookie expired ({} redirected to login); please update via /cookies", status);
    }
    if !status.is_success() {
        anyhow::bail!("weibo HTTP {}", status);
    }

    let json: Value = resp.json().await.context("weibo JSON parse failed")?;
    if json.get("ok").and_then(|v| v.as_i64()) != Some(1) {
        let msg = json.get("msg").and_then(|v| v.as_str()).unwrap_or("unknown");
        // When the cookie expires, the weibo API returns ok!=1 (common msg
        // values contain the Chinese words for "login" / "verify"). The error
        // string must contain the "cookie" substring — the scheduler uses it
        // to set the cookie_expired_notified flag (/cookies panel status +
        // expiry notice).
        if msg.contains("登录") || msg.contains("验证") || msg.contains("login") {
            anyhow::bail!("weibo cookie expired (API returned an error: {}); please update via /cookies", msg);
        }
        anyhow::bail!("weibo API returned an error: {}", msg);
    }

    let data = &json["data"];

    // screen_name: prefer userInfo, fallback to first mblog.user
    let screen_name = data["userInfo"]["screen_name"]
        .as_str()
        .or_else(|| {
            data["cards"].as_array()?.iter().find_map(|c| {
                c["mblog"]["user"]["screen_name"].as_str()
            })
        })
        .unwrap_or("weibo")
        .to_string();

    let cards = data["cards"].as_array().cloned().unwrap_or_default();
    let mut entries: Vec<Entry> = Vec::new();
    let mut metas: crate::entrymeta::MetaMap = std::collections::HashMap::new();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for card in &cards {
        // Only process normal timeline cards (card_type 9). Other types
        // (recommendations / trending / ads etc.) may contain content that
        // duplicates timeline posts but with different IDs, causing duplicate pushes.
        let card_type = card["card_type"].as_i64().unwrap_or(0);
        if card_type != 9 {
            continue;
        }
        let mblog = &card["mblog"];
        if mblog.is_null() {
            continue;
        }
        // Dedup mblog IDs within the same batch (guards against the same post
        // appearing at multiple card positions)
        let mblog_id = mblog["id"].as_str().unwrap_or("");
        if mblog_id.is_empty() || !seen_ids.insert(mblog_id.to_string()) {
            continue;
        }
        // Vote signal diagnostics (added in v1.9.5, downgraded to debug in
        // v1.9.6): logs each mblog's is_vote flag and body characteristics,
        // only emitted when RUST_LOG=debug to avoid flooding the log.
        {
            let text = mblog["text"].as_str().unwrap_or("");
            tracing::debug!(
                mid = %mblog_id,
                is_vote = ?mblog["is_vote"].as_i64(),
                has_card_link = RE_VOTE_CARD_LINK.is_match(text),
                has_tcn = text.contains("t.cn/"),
                page_info_type = ?mblog["page_info"]["type"].as_str(),
                text_len = text.len(),
                "weibo mblog vote signals"
            );
        }
        // Mutable copy: once detail data is fetched, overwrite pics in place /
        // extract the full text
        let mut mblog = mblog.clone();
        let mut full_text: Option<String> = None;
        let mut rt_full_text: Option<String> = None;

        // Main post: when long text or images are truncated by the list API,
        // make one statuses/show call to fill them in.
        // The getIndex timeline truncates the pics array for multi-image
        // weibos (observed: an 11-image post returned only the first 9, while
        // pic_num still reported the real count); only statuses/show returns
        // the complete list.
        // The long-text full content and the extra images share this single
        // request; on failure fall back to the list data without losing the entry.
        if status_needs_detail(&mblog) {
            if let Some(detail) = fetch_status_show(state, &cookies, mblog_id).await {
                if mblog["isLongText"].as_bool() == Some(true) {
                    full_text = detail["text"]
                        .as_str()
                        .filter(|t| !t.is_empty())
                        .map(|t| t.to_string());
                }
                patch_pics(&mut mblog, &detail);
            }
        }
        // The retweeted status can also be long text / pic-truncated; same fix.
        if status_needs_detail(&mblog["retweeted_status"]) {
            let rt_mid = mblog["retweeted_status"]["id"].as_str().unwrap_or("");
            if let Some(detail) = fetch_status_show(state, &cookies, rt_mid).await {
                if mblog["retweeted_status"]["isLongText"].as_bool() == Some(true) {
                    rt_full_text = detail["text"]
                        .as_str()
                        .filter(|t| !t.is_empty())
                        .map(|t| t.to_string());
                }
                patch_pics(&mut mblog["retweeted_status"], &detail);
            }
        }
        // Vote posts: getIndex only carries the is_vote=1 flag; the vote
        // details (question/options/multi-select) must be fetched via the
        // desktop ajax/statuses/show endpoint. A failure does not block the
        // entry push (it degrades to plain text).
        let vote_obj = if mblog["is_vote"].as_i64() == Some(1) {
            fetch_vote_object(state, &cookies, mblog_id).await
        } else {
            None
        };

        match mblog_to_entry(&mblog, uid, &screen_name, full_text.as_deref(), rt_full_text.as_deref(), vote_obj.as_ref()) {
            Some((e, meta)) => {
                if !meta.is_empty() {
                    metas.insert(e.id.clone(), meta);
                }
                entries.push(e);
            }
            None => {
                warn!(mblog_id = %mblog_id, uid = %uid, "mblog_to_entry returned None, entry silently dropped");
                continue;
            }
        }
    }

    // Sort by published time descending (newest first).
    // Pinned posts appear first in the API but may have older timestamps;
    // sorting ensures chronological order regardless.
    crate::platforms::sort_entries_newest_first(&mut entries);

    // cards non-empty yet zero entries produced: most likely a weibo layout
    // change (card_type values changed etc.). Must not silently return empty —
    // the scheduler would treat it as success and reset the error count, and
    // the feed would silently stop updating with zero logs.
    if entries.is_empty() && !cards.is_empty() {
        let types: Vec<i64> = cards
            .iter()
            .map(|c| c["card_type"].as_i64().unwrap_or(0))
            .collect();
        warn!(
            uid = %uid,
            cards = cards.len(),
            card_types = ?types,
            "weibo getIndex returned cards but zero entries extracted (card_type may have changed)"
        );
    }

    let mut result = crate::fetcher::FetchResult::platform(Some(screen_name), entries);
    result.metas = metas;
    Ok(result)
}

/// True when a status needs a statuses/show detail fetch: long text
/// (isLongText) or a truncated pics list. The getIndex timeline API caps
/// the pics array for multi-image posts (observed: an 11-image post
/// returned only 9, while pic_num still reported 11); statuses/show is
/// the only source of the complete list.
fn status_needs_detail(mblog: &Value) -> bool {
    if mblog.is_null() {
        return false;
    }
    if mblog["isLongText"].as_bool() == Some(true) {
        return true;
    }
    // For video posts, pic_num counts the video cover (the cover lives in
    // page_info.page_pic, not the pics[] array); it must not trigger a
    // detail fetch — statuses/show likewise returns no pics for video posts,
    // so each round would waste one API call.
    if mblog["page_info"]["type"].as_str() == Some("video") {
        return false;
    }
    let pic_num = mblog["pic_num"].as_u64().unwrap_or(0);
    let pics_len = mblog["pics"].as_array().map(|a| a.len() as u64).unwrap_or(0);
    pic_num > pics_len
}

/// Overwrite a status's pics with the complete list from statuses/show
/// when the detail response carries more images than the (truncated) list
/// data. Never shrinks an existing list.
fn patch_pics(mblog: &mut Value, detail: &Value) {
    let detail_len = detail["pics"].as_array().map(|a| a.len()).unwrap_or(0);
    let cur_len = mblog["pics"].as_array().map(|a| a.len()).unwrap_or(0);
    if detail_len > cur_len {
        mblog["pics"] = detail["pics"].clone();
    }
}

/// Shared request skeleton for weibo JSON APIs (M4 fix: fetch_vote_object
/// and fetch_status_show each used to write the request → status check →
/// JSON parse → warn template once, ~30 lines matched line by line).
/// Returns the parsed JSON Value; any failure returns None and leaves a warn log.
/// `ua` distinguishes desktop (BROWSER_UA) from mobile (MOBILE_BROWSER_UA).
async fn get_weibo_json(state: &AppState, api: &str, cookies: &str, referer: &str, ua: &str, ctx: &str) -> Option<Value> {
    let client = state.client_for_url(api);
    let resp = match client
        .get(api)
        .header("Cookie", cookies)
        .header("Referer", referer)
        .header("User-Agent", ua)
        .header("Accept", "application/json, text/plain, */*")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(ctx = ctx, error = %e, "weibo json request failed");
            return None;
        }
    };
    if !resp.status().is_success() {
        warn!(ctx = ctx, status = %resp.status(), "weibo json non-success HTTP status");
        return None;
    }
    match resp.json().await {
        Ok(j) => Some(j),
        Err(e) => {
            warn!(ctx = ctx, error = %e, "weibo json parse failed");
            None
        }
    }
}

/// Fetch vote details for a status via the desktop ajax API.
/// The mobile getIndex API only carries `is_vote: 1`; the full vote object
/// (question, options, multi-select flag, expiry) lives in the desktop
/// endpoint's `page_info.card_info.vote_object`. Returns None on failure
/// (the entry is still pushed as text, just without a poll).
async fn fetch_vote_object(state: &AppState, cookies: &str, mid: &str) -> Option<Value> {
    if mid.is_empty() {
        warn!("fetch_vote_object: mid is empty, skipping");
        return None;
    }
    let api = format!("https://weibo.com/ajax/statuses/show?id={}", mid);
    let ctx = format!("fetch_vote_object mid={}", mid);
    let json = get_weibo_json(state, &api, cookies, "https://weibo.com/", crate::state::BROWSER_UA, &ctx).await?;
    let vo = json["page_info"]["card_info"]["vote_object"].clone();
    if vo.is_null() {
        warn!(mid = %mid, "fetch_vote_object: vote_object not found in response (page_info.card_info.vote_object is null)");
        return None;
    }
    // At least a question and 2 valid options are needed to justify converting to a poll
    let has_question = vo["content"].as_str().map(|s| !s.is_empty()).unwrap_or(false);
    let option_count = vo["vote_list"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|v| v["content"].as_str().map(|s| !s.is_empty()).unwrap_or(false))
                .count()
        })
        .unwrap_or(0);
    if !has_question || option_count < 2 {
        warn!(
            mid = %mid,
            has_question = has_question,
            option_count = option_count,
            "fetch_vote_object: vote_object incomplete (missing question or <2 options)"
        );
        return None;
    }
    Some(vo)
}

/// Fetch one status's full data via statuses/show API (used for long text
/// and for recovering the complete pics list). Returns None on failure
/// (caller falls back to the truncated list data).
/// Falling back to the list data on failure is intentional, but every failure
/// exit leaves a warn — otherwise truncated long text or multi-image posts
/// reduced to 9 images would be completely invisible in production debugging
/// (consistent with fetch_vote_object).
async fn fetch_status_show(state: &AppState, cookies: &str, mid: &str) -> Option<Value> {
    if mid.is_empty() {
        return None;
    }
    let api = format!("https://m.weibo.cn/statuses/show?id={}", mid);
    let ctx = format!("statuses/show mid={}", mid);
    let json = get_weibo_json(state, &api, cookies, "https://m.weibo.cn/", crate::state::MOBILE_BROWSER_UA, &ctx).await?;
    if json.get("ok").and_then(|v| v.as_i64()) != Some(1) {
        warn!(mid = %mid, msg = %json["msg"].as_str().unwrap_or(""), "statuses/show ok!=1, falling back to list data");
        return None;
    }
    let data = json["data"].clone();
    if data.is_null() {
        warn!(mid = %mid, "statuses/show data is null, falling back to list data");
        return None;
    }
    Some(data)
}

/// Convert relative hrefs in weibo HTML text to absolute m.weibo.cn URLs.
/// The native API returns relative paths for @user links (`/n/xxx`), status
/// links (`/status/xxx`), and hashtag/search links (`/search?containerid=...`).
/// Telegram cannot render relative-path hyperlinks, so these must be made
/// absolute or the links are silently dropped. Handles both single- and
/// double-quoted href attributes.
fn absolutize_hrefs(html: &str) -> String {
    // Rust's regex crate has no backreferences, so handle the two quote
    // styles with separate patterns.
    let s = RE_HREF_DQ
        .replace_all(html, |caps: &regex::Captures| {
            format!("href=\"https://m.weibo.cn{}\"", &caps[1])
        })
        .to_string();
    RE_HREF_SQ
        .replace_all(&s, |caps: &regex::Captures| {
            format!("href='https://m.weibo.cn{}'", &caps[1])
        })
        .to_string()
}

/// Decode a `https://weibo.cn/sinaurl?u=<percent-encoded>` redirect URL into
/// the real image URL (e.g. `https://wx2.sinaimg.cn/large/...`).
/// `Url::query_pairs()` already percent-decodes the value.
/// Returns None when the `u` parameter is missing, not an http(s) URL, or
/// **not an image URL** (v1.10.11: non-image URLs such as HTML pages are
/// filtered out to prevent IMAGE_PROCESS_FAILED when Telegram receives a
/// non-image payload).
///
/// Image detection: the decoded URL must either be on a `sinaimg.cn` host
/// (weibo's image CDN) or carry a recognised image file extension.
fn decode_sinaurl(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let decoded = u.query_pairs().find(|(k, _)| k == "u")?.1.to_string();
    if !(decoded.starts_with("http://") || decoded.starts_with("https://")) {
        return None;
    }
    // Filter non-image URLs: only accept sinaimg.cn (weibo image CDN) or
    // URLs with a recognised image extension.  This prevents HTML pages
    // (e.g. peopleapp.com "web page link" cards) from being downloaded and sent
    // as photos, which Telegram rejects with IMAGE_PROCESS_FAILED.
    if !is_image_url(&decoded) {
        return None;
    }
    Some(decoded)
}

/// Returns true when `url` looks like an image resource:
/// - host ends with `sinaimg.cn` (weibo image CDN, covers wx1/wx2/…/tvax), or
/// - the path (ignoring query/fragment) ends with a known image extension.
fn is_image_url(url: &str) -> bool {
    if let Ok(parsed) = url::Url::parse(url) {
        if let Some(host) = parsed.host_str() {
            if host == "sinaimg.cn" || host.ends_with(".sinaimg.cn") {
                return true;
            }
        }
        // Check extension on the path component only.
        let path = parsed.path();
        let lower = path.to_ascii_lowercase();
        return lower.ends_with(".jpg")
            || lower.ends_with(".jpeg")
            || lower.ends_with(".png")
            || lower.ends_with(".gif")
            || lower.ends_with(".webp")
            || lower.ends_with(".bmp");
    }
    false
}

/// Extract inline `[label](sinaurl)` / `<a href="sinaurl">…label…</a>`
/// image links from weibo text (main post body only — the image card
/// appears in the main post; retweeted statuses carry images in pics[]).
/// The label is not fixed ("View image" / "Comment image" etc.); sinaurl?u= is the
/// only identifier.
///
/// Returns (cleaned_text, image_urls):
/// - cleaned_text: the original link is preserved as an `<a>` hyperlink so it
///   remains clickable in the Telegram message.
/// - image_urls: decoded real image URLs (sinaimg.cn large) for media download.
fn extract_inline_images(text: &str) -> (String, Vec<String>) {
    let mut urls: Vec<String> = Vec::new();

    // HTML form: <a href="https://weibo.cn/sinaurl?u=...">…label…</a>
    // caps[1] = sinaurl URL, caps[2] = inner content (may contain span shells)
    let s = RE_SINAURL_HTML
        .replace_all(text, |caps: &regex::Captures| {
            let sinaurl = &caps[1];
            if let Some(real) = decode_sinaurl(sinaurl) {
                urls.push(real);
            }
            // Keep the original link as a clickable hyperlink.
            // Extract visible label from inner content (strip tags).
            let label = crate::fetcher::html_to_plain(&caps[2]);
            let label = if label.trim().is_empty() { "查看图片".to_string() } else { label.trim().to_string() };
            format!("<a href=\"{}\">{}</a>", crate::push::escape_attr(sinaurl), crate::push::escape_html(&label))
        })
        .to_string();

    // Markdown form: [label](https://weibo.cn/sinaurl?u=...)
    // caps[1] = label, caps[2] = sinaurl URL
    let s = RE_SINAURL_MD
        .replace_all(&s, |caps: &regex::Captures| {
            let sinaurl = &caps[2];
            if let Some(real) = decode_sinaurl(sinaurl) {
                urls.push(real);
            }
            format!("<a href=\"{}\">{}</a>", crate::push::escape_attr(sinaurl), crate::push::escape_html(&caps[1]))
        })
        .to_string();

    (s, urls)
}

/// Append a status's media to the HTML body: pics[].large.url images
/// (fallback pics[].url, then original_pic for single-image posts where
/// getIndex omits the pics array), the page_info video stream, and the
/// article (top-article) cover image page_info.page_pic when no pics exist.
/// Shared by the main post and the retweeted status.
/// Returns true if a video was actually attached (used to conditionally
/// remove the video card link from the text).
fn append_weibo_media(body: &mut String, mblog: &Value) -> bool {
    // Images: pics[].large.url (fallback to pics[].url)
    let mut has_pics = false;
    if let Some(pics) = mblog["pics"].as_array() {
        for pic in pics {
            let url = pic["large"]["url"]
                .as_str()
                .or_else(|| pic["url"].as_str())
                .unwrap_or_default();
            if !url.is_empty() {
                body.push_str(&format!("<img src=\"{}\" referrerpolicy=\"no-referrer\">", crate::push::escape_attr(url)));
                has_pics = true;
            }
        }
    }
    // Fallback: single-image posts may carry the image only in original_pic
    // while the pics array is empty/absent (getIndex API inconsistency).
    if !has_pics {
        if let Some(url) = mblog["original_pic"].as_str().filter(|s| !s.is_empty()) {
            body.push_str(&format!("<img src=\"{}\" referrerpolicy=\"no-referrer\">", crate::push::escape_attr(url)));
            has_pics = true;
        } else {
            let pic_num = mblog["pic_num"].as_u64().unwrap_or(0);
            // For video posts, pic_num counts the video cover (in
            // page_info.page_pic); this is not a case of "has images but
            // extraction failed", so no warn is emitted.
            let is_video = mblog["page_info"]["type"].as_str() == Some("video");
            if pic_num > 0 && !is_video {
                // Include mblog_id/uid/page_info_type to make it easy to locate
                // which post it is and what form it takes (video cover counted in
                // pic_num / image deleted / member-only content etc.).
                tracing::warn!(
                    mblog_id = %mblog["id"].as_str().unwrap_or(""),
                    uid = %mblog["user"]["id"].as_u64().map(|v| v.to_string()).unwrap_or_default(),
                    pic_num = pic_num,
                    page_info_type = ?mblog["page_info"]["type"].as_str(),
                    "weibo mblog has pic_num>0 but no extractable image URLs"
                );
            }
        }
    }

    // Video: page_info.media_info.stream_url_hd / stream_url; for some posts
    // media_info is empty and the stream URL is in page_info.urls (pick the
    // highest available quality by key name).
    // The poster attribute carries the cover image URL (page_pic), used as a
    // fallback to send the cover when the video is too large / sending fails.
    let page_info = &mblog["page_info"];
    let page_pic_url = page_info["page_pic"]["url"]
        .as_str()
        .or_else(|| page_info["page_pic"].as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let mut video_attached = false;
    if page_info["type"].as_str() == Some("video") {
        let media_info = &page_info["media_info"];
        let video_url = media_info["stream_url_hd"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| media_info["stream_url"].as_str().filter(|s| !s.is_empty()))
            .map(|s| s.to_string())
            .or_else(|| {
                const URL_KEYS: &[&str] = &[
                    "mp4_4k_mp4", "mp4_1080p_mp4", "mp4_720p_mp4", "hevc_mp4_hd",
                    "mp4_hd_mp4", "ultra", "hd", "high", "mp4_ld_mp4", "medium",
                ];
                let urls = &page_info["urls"];
                URL_KEYS
                    .iter()
                    .find_map(|k| urls[k].as_str().filter(|s| !s.is_empty()))
                    .map(|s| s.to_string())
            })
            .unwrap_or_default();
        if !video_url.is_empty() {
            if !page_pic_url.is_empty() {
                body.push_str(&format!("<video src=\"{}\" poster=\"{}\"></video>", crate::push::escape_attr(&video_url), crate::push::escape_attr(page_pic_url)));
            } else {
                body.push_str(&format!("<video src=\"{}\"></video>", crate::push::escape_attr(&video_url)));
            }
            video_attached = true;
        }
    } else if page_info["type"].as_str() == Some("live") {
        // Live posts: no video stream to push; extract the cover image
        // (page_pic) and send it as an image.
        if !has_pics && !page_pic_url.is_empty() {
            body.push_str(&format!("<img src=\"{}\" referrerpolicy=\"no-referrer\">", crate::push::escape_attr(page_pic_url)));
        }
    } else if !has_pics && page_info["type"].as_str() == Some("article") {
        // Article (top-article) posts have no pics array; use the cover image
        // from page_info.page_pic so the push is not purely text-only.
        if !page_pic_url.is_empty() {
            body.push_str(&format!("<img src=\"{}\" referrerpolicy=\"no-referrer\">", crate::push::escape_attr(page_pic_url)));
        }
    }
    video_attached
}

/// Convert one mblog JSON object into a feed-rs Entry + structured metadata.
/// `full_text` (when Some) replaces the truncated list text for long weibos.
/// `rt_full_text` (when Some) does the same for the retweeted status.
fn mblog_to_entry(
    mblog: &Value,
    uid: &str,
    screen_name: &str,
    full_text: Option<&str>,
    rt_full_text: Option<&str>,
    vote_obj: Option<&Value>,
) -> Option<(Entry, crate::entrymeta::EntryMeta)> {
    let id = mblog["id"].as_str()?.to_string();
    let bid = mblog["bid"].as_str().unwrap_or(&id);

    // Text body (HTML with <a>, <br/> etc.). Long weibos use the full text
    // fetched from statuses/show; the list text ends with a "full text" link.
    // Absolutize relative hrefs so @user / hashtag / status links survive.
    let text = absolutize_hrefs(
        full_text.unwrap_or_else(|| mblog["text"].as_str().unwrap_or_default()),
    );
    // Strip UI icon <img> tags (card thumbnails, emoji icons without alt).
    // These are decorative and must not be extracted as media — Telegram
    // rejects them with PHOTO_INVALID_DIMENSIONS.
    let text = strip_ui_icons(&text);

    // Extract inline [View image](sinaurl) image links from the main post text.
    // These appear before the "Retweeted:" blockquote when the user attaches an
    // image via the "View image" card. The original link stays as a clickable
    // hyperlink; the decoded real image URLs are collected into
    // EntryMeta.retweet_images so the media pipeline sends them as a
    // separate message after the main one (group C sidecar, replacing the
    // former data-rt injection).
    let (text, main_inline_imgs) = extract_inline_images(&text);

    // Markdown rendering: md_render_mark == 1 indicates the post was authored
    // with weibo's markdown editor (desktop web). The text field contains
    // markdown syntax mixed with <br /> tags. Convert to Telegram HTML so
    // headings/bold/italic/links/lists render properly instead of showing
    // raw markdown source.
    let is_markdown = mblog["md_render_mark"].as_i64() == Some(1)
        || mblog["style_config"]["md_render_mark"].as_i64() == Some(1);
    let text = if is_markdown {
        render_weibo_markdown(&text)
    } else {
        text
    };

    // Build content HTML: original text + media tags + retweet blockquote
    let mut body = text.clone();

    // Images + video (shared helper, also used for retweets below).
    // Returns true if a video was actually attached.
    let video_attached = append_weibo_media(&mut body, mblog);

    // Remove video/vote card links (the corresponding media/poll has already
    // been sent separately, so the card link in the body is redundant).
    // Only remove when the media/poll was actually attached, to avoid losing
    // the only link when extraction fails (#1 fix).
    // - Video posts: video.weibo.com/show card link removed (video already
    //   sent as media)
    // - Vote posts: t.cn short-link card removed (vote already converted to a
    //   TG native Poll)
    // - Live posts: wblive links kept (users need the link to watch the live
    //   stream; there is no media to send)
    let page_type = mblog["page_info"]["type"].as_str().unwrap_or("");
    if page_type == "video" && video_attached {
        body = RE_VIDEO_CARD_LINK.replace_all(&body, "").to_string();
    } else if vote_obj.is_some() {
        // vote_obj is only fetched when is_vote==1 (see the fetch loop), so
        // there is no need to re-check is_vote here.
        body = RE_VOTE_CARD_LINK.replace_all(&body, "").to_string();
    }

    // Collect main-post inline images (from [View image] links) into sidecar.
    // Dedup: skip URLs already present in body (pics array may carry the same).
    // Group C: no longer inject <img data-rt="1"> into body; pass them via
    // EntryMeta.retweet_images instead.
    // Escaping consistency: URLs inside body are written through escape_attr
    // (&→&amp;); a plain contains() on the raw URL would miss URLs containing
    // &, so both the raw and escaped forms are compared.
    let mut rt_images: Vec<String> = Vec::new();
    for img_url in &main_inline_imgs {
        let escaped = crate::push::escape_attr(img_url);
        if body.contains(img_url.as_str()) || body.contains(escaped.as_str()) {
            continue;
        }
        rt_images.push(img_url.clone());
    }

    // Retweeted status → blockquote (RSSHub style). The blockquote wraps the
    // entire retweet: header line + original text + original media, so the
    // whole thing renders as a single quoted block in Telegram.
    let rt = &mblog["retweeted_status"];
    if !rt.is_null() {
        let rt_user = rt["user"]["screen_name"].as_str().unwrap_or("已注销");
        // Hyperlink the retweeted user's name to the original weibo itself
        // (https://weibo.com/<uid>/<bid>); fall back to the user homepage
        // when bid is missing, and to plain text when uid is missing too.
        let rt_bid = rt["bid"].as_str().unwrap_or("");
        let rt_user_html = match rt["user"]["id"].as_u64() {
            Some(rt_uid) if !rt_bid.is_empty() => format!(
                "<a href=\"https://weibo.com/{}/{}\">@{}</a>",
                rt_uid,
                rt_bid,
                crate::push::escape_html(rt_user)
            ),
            Some(rt_uid) => format!(
                "<a href=\"https://weibo.com/u/{}\">@{}</a>",
                rt_uid,
                crate::push::escape_html(rt_user)
            ),
            None => format!("@{}", crate::push::escape_html(rt_user)),
        };
        // Long retweets: use the full text fetched from statuses/show.
        // Absolutize relative hrefs so @user / hashtag links survive.
        let rt_text = strip_ui_icons(&absolutize_hrefs(
            rt_full_text.unwrap_or_else(|| rt["text"].as_str().unwrap_or_default()),
        ));
        // Markdown posts bypass html_to_telegram_html in entry_body_html,
        // so the retweet text (raw weibo HTML with <br/>/<a>) must be
        // converted here to avoid leaking unconverted markup into the message.
        let rt_text = if is_markdown {
            crate::fetcher::html_to_telegram_html(&rt_text)
        } else {
            rt_text
        };
        // Retweet media (pics[] / video / article cover).
        let mut rt_body = rt_text.clone();
        append_weibo_media(&mut rt_body, rt);
        // Header + retweet body in ONE blockquote, single line:
        // "Retweeted @user: original post content" (the template below starts
        // with the Chinese word for "Retweeted").
        // A single \n separates the original text from the quote block
        // (visually one line break); split_html recognises original text
        // stuck right before a quote block and handles it separately, so the
        // original text is not wrapped into the quote block.
        body.push_str(&format!(
            "\n<blockquote>转发 {}: {}</blockquote>",
            rt_user_html, rt_body
        ));
    }

    // Vote data: build EntryMeta.poll (group C sidecar, replacing the former
    // <div data-vote> body injection).
    let poll_meta = vote_obj.and_then(|vo| {
        let question = vo["content"].as_str()?.to_string();
        if question.is_empty() {
            return None;
        }
        let options: Vec<String> = vo["vote_list"]
            .as_array()?
            .iter()
            .filter_map(|v| v["content"].as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        if options.len() < 2 {
            return None;
        }
        let is_multi_select = vo["is_multi_select"].as_i64() == Some(1);
        Some(crate::entrymeta::PollData {
            question,
            options,
            is_multi_select,
        })
    });

    // Parse created_at: "Wed Sep 25 20:09:08 +0800 2026" or relative forms
    let published = parse_weibo_time(mblog["created_at"].as_str().unwrap_or(""));

    // Title: first 50 visible chars of text (strip tags)
    let plain = strip_tags(&text);
    let title = crate::platforms::make_title(&plain, "(微博)");

    let link = format!("https://weibo.com/{}/{}", uid, bid);

    // Markdown-rendered posts carry pre-converted Telegram HTML in the body.
    // Group C: now explicitly marked via EntryMeta.prerendered_html (replacing
    // the content_type MIME code); content_type uniformly goes back to standard
    // text/html.
    let content_mime = "text/html";

    // The prerendered flag is carried by EntryMeta.prerendered_html (group C
    // sidecar, replacing the former MIME code).
    let meta = crate::entrymeta::EntryMeta {
        prerendered_html: is_markdown,
        poll: poll_meta,
        retweet_images: rt_images,
    };

    Some((crate::platforms::make_entry(id, title, screen_name.to_string(), body, content_mime, link, published), meta))
}

/// Parse weibo created_at. Handles absolute format and relative forms.
fn parse_weibo_time(s: &str) -> Option<DateTime<Utc>> {
    // Absolute: "Wed Sep 25 20:09:08 +0800 2026" — same format as Twitter's
    // created_at, but weibo's weekday field is not guaranteed to match the
    // date. So strip the first token (weekday) and let chrono parse the
    // remainder "%b %d %H:%M:%S %z %Y" — this both converges the code and is
    // immune to weekday/date mismatches (which is exactly the fault-tolerance
    // intent of the original hand-written parsing that ignored the weekday).
    if let Some(rest) = s.split_once(' ').map(|(_, r)| r) {
        if let Ok(dt) = chrono::DateTime::parse_from_str(rest, "%b %d %H:%M:%S %z %Y") {
            return Some(dt.with_timezone(&Utc));
        }
    }
    // Relative: "刚刚" (just now) / "x分钟前" (x minutes ago) /
    // "x小时前" (x hours ago) / "昨天 HH:MM" (yesterday HH:MM)
    let now = Utc::now();
    if s == "刚刚" {
        return Some(now);
    }
    if let Some(m) = s.strip_suffix("分钟前") {
        if let Ok(n) = m.trim().parse::<i64>() {
            return Some(now - chrono::Duration::minutes(n));
        }
    }
    if let Some(m) = s.strip_suffix("小时前") {
        if let Ok(n) = m.trim().parse::<i64>() {
            return Some(now - chrono::Duration::hours(n));
        }
    }
    if let Some(hm) = s.strip_prefix("昨天 ") {
        if let Ok(t) = chrono::NaiveTime::parse_from_str(hm, "%H:%M") {
            let cst = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
            let yesterday = (now.with_timezone(&cst) - chrono::Duration::days(1)).date_naive();
            let dt = yesterday.and_time(t).and_local_timezone(cst).single()?;
            return Some(dt.with_timezone(&Utc));
        }
    }
    // "MM-DD" (this year)
    if s.len() == 5 && s.chars().nth(2) == Some('-') {
        if let Ok(md) = chrono::NaiveDate::parse_from_str(
            &format!("{}-{}", now.format("%Y"), s),
            "%Y-%m-%d",
        ) {
            let cst = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
            let dt = md.and_hms_opt(0, 0, 0)?.and_local_timezone(cst).single()?;
            // Cross-year correction: weibo relative times carry no year, so a
            // post from last December fetched in January would be parsed as a
            // future time. If it is ahead by more than one day, treat it as
            // last year.
            let dt = if dt > now + chrono::Duration::days(1) {
                md.with_year(md.year() - 1)?
                    .and_hms_opt(0, 0, 0)?
                    .and_local_timezone(cst)
                    .single()?
            } else {
                dt
            };
            return Some(dt.with_timezone(&Utc));
        }
    }
    None
}

// ── Markdown rendering (posts with md_render_mark == 1) ──

/// Inline markdown syntax → Telegram HTML.
/// Processing order: links → inline code → escape → bold → strikethrough →
/// italic → restore.
/// Links and code are extracted into placeholders first, so their contents are
/// not corrupted by the subsequent escaping/formatting passes.
fn render_md_inline(text: &str) -> String {
    // 1) Extract markdown links [text](url) → placeholders
    let mut links: Vec<(String, String)> = Vec::new();
    let s = RE_MD_LINK
        .replace_all(text, |caps: &regex::Captures| {
            let idx = links.len();
            links.push((caps[1].to_string(), caps[2].to_string()));
            format!("\u{FFFE}L{}\u{FFFE}", idx)
        })
        .to_string();

    // 2) Extract inline code `code` → placeholders (code content gets no
    // further formatting)
    let mut codes: Vec<String> = Vec::new();
    let s = RE_MD_INLINE_CODE
        .replace_all(&s, |caps: &regex::Captures| {
            let idx = codes.len();
            codes.push(caps[1].to_string());
            format!("\u{FFFE}C{}\u{FFFE}", idx)
        })
        .to_string();

    // 3) Escape HTML special characters (placeholders contain no &<> at this
    // point, so this is safe)
    let s = crate::push::escape_html(&s);

    // 4) Bold **...** (before italic, to avoid ** being split into two *)
    let s = RE_MD_BOLD.replace_all(&s, "<b>$1</b>").to_string();

    // 5) Strikethrough ~~...~~
    let s = RE_MD_STRIKE.replace_all(&s, "<s>$1</s>").to_string();

    // 6) Italic *...* (** already handled; remaining single * is italic)
    let s = RE_MD_ITALIC.replace_all(&s, "<i>$1</i>").to_string();

    // 7) Restore inline code
    let mut s = s;
    for (i, code) in codes.iter().enumerate() {
        let token = format!("\u{FFFE}C{}\u{FFFE}", i);
        s = s.replace(&token, &format!("<code>{}</code>", crate::push::escape_html(code)));
    }

    // 8) Restore markdown links (link text is only escaped, no recursive
    // formatting)
    for (i, (link_text, url)) in links.iter().enumerate() {
        let token = format!("\u{FFFE}L{}\u{FFFE}", i);
        if link_text.is_empty() || url.is_empty() {
            s = s.replace(&token, "");
        } else {
            s = s.replace(
                &token,
                &format!(
                    "<a href=\"{}\">{}</a>",
                    crate::push::escape_attr(url),
                    crate::push::escape_html(link_text)
                ),
            );
        }
    }

    s
}

/// Convert weibo markdown post text into Telegram HTML.
///
/// The input is the mixed format returned by the weibo API: markdown syntax
/// combined with `<br />` HTML line breaks. absolutize_hrefs / strip_ui_icons /
/// extract_inline_images have already run before this call, so the input may
/// contain `<a>` hyperlink tags (which must be protected from escaping).
///
/// The output is valid Telegram HTML (`<b>`/`<i>`/`<s>`/`<code>`/`<a>`/
/// `<blockquote>`) and can be sent directly as the message body without going
/// through html_to_telegram_html again.
fn render_weibo_markdown(text: &str) -> String {
    // 0) Strip sentinel characters already present in the input (same as
    //    html_to_telegram_html), to prevent conflicts with the placeholders
    //    used below.
    let mut text = text.to_string();
    text.retain(|c| c != '\u{FFFC}' && c != '\u{FFFE}');

    // 1) <br /> → \n (weibo markdown uses <br /> instead of real newlines)
    let text = crate::fetcher::RE_BR.replace_all(&text, "\n").to_string();

    // 2) Protect existing <a> tags (output of absolutize_hrefs /
    // extract_inline_images)
    let mut anchors: Vec<String> = Vec::new();
    let text = RE_MD_ANCHOR
        .replace_all(&text, |caps: &regex::Captures| {
            let idx = anchors.len();
            anchors.push(caps[0].to_string());
            format!("\u{FFFC}{}\u{FFFC}", idx)
        })
        .to_string();

    // 3) Process block-level syntax line by line
    let mut out_lines: Vec<String> = Vec::new();
    let mut bq_lines: Vec<String> = Vec::new();
    // Fenced code block state machine
    let mut in_code_block = false;
    let mut code_lines: Vec<String> = Vec::new();
    let mut code_lang = String::new();
    // pre block placeholders (prevent the trailing RE_BLANK_LINES from
    // collapsing blank lines inside code)
    let mut pre_blocks: Vec<String> = Vec::new();

    // Flush the accumulated blockquote lines into a single <blockquote>
    // element, adding blank lines before and after so that split_html treats
    // the blockquote as an independent paragraph when splitting on \n\n
    // (atomic, never split).
    let flush_bq = |out: &mut Vec<String>, bq: &mut Vec<String>| {
        if bq.is_empty() {
            return;
        }
        if out.last().map(|s| !s.is_empty()).unwrap_or(false) {
            out.push(String::new());
        }
        let content = bq.join("\n");
        out.push(format!("<blockquote>{}</blockquote>", content));
        out.push(String::new());
        bq.clear();
    };

    // Flush the fenced code block as a <pre><code> element (stored in
    // out_lines as a placeholder).
    // Code content is only HTML-escaped, not markdown-formatted; blank lines
    // are preserved as-is.
    // split_html treats <pre> blocks atomically (prevents internal \n\n from
    // being split into separate paragraphs).
    // The language tag is preserved as class="language-xxx" (Telegram client
    // syntax highlighting).
    let flush_code = |out: &mut Vec<String>, code: &mut Vec<String>, lang: &str, pres: &mut Vec<String>| {
        if code.is_empty() && lang.is_empty() {
            return;
        }
        if out.last().map(|s| !s.is_empty()).unwrap_or(false) {
            out.push(String::new());
        }
        // Remove trailing blank lines (leading blank lines are kept, faithful
        // to the original)
        while code.last().map(|l| l.is_empty()).unwrap_or(false) {
            code.pop();
        }
        let content = code.join("\n");
        let open_tag = if lang.is_empty() {
            "<pre><code>".to_string()
        } else {
            format!("<pre><code class=\"language-{}\">", crate::push::escape_attr(lang))
        };
        let html = format!(
            "{}{}</code></pre>",
            open_tag,
            crate::push::escape_html(&content)
        );
        // Store as a placeholder to prevent the trailing RE_MD_BLANK_LINES
        // from collapsing blank lines inside the code
        let idx = pres.len();
        pres.push(html);
        out.push(format!("\u{FFFE}PB{}\u{FFFE}", idx));
        out.push(String::new());
        code.clear();
    };

    for line in text.split('\n') {
        let trimmed = line.trim();

        // ── Fenced code block state machine ──
        if in_code_block {
            if trimmed.starts_with("```") {
                // Close the fence
                flush_code(&mut out_lines, &mut code_lines, &code_lang, &mut pre_blocks);
                in_code_block = false;
                code_lang.clear();
            } else {
                // Code content: preserve original indentation and blank lines
                // (use line, not trimmed)
                code_lines.push(line.to_string());
            }
            continue;
        }
        if trimmed.starts_with("```") {
            // Open the fence, extract the optional language tag (e.g. ```python)
            // First flush any unclosed blockquote
            if !bq_lines.is_empty() {
                flush_bq(&mut out_lines, &mut bq_lines);
            }
            in_code_block = true;
            code_lines.clear();
            code_lang = trimmed.strip_prefix("```").unwrap_or("").trim().to_string();
            continue;
        }

        // Blockquote end detection: when the current line no longer starts
        // with >, flush the accumulated blockquote lines
        if !bq_lines.is_empty() && !trimmed.starts_with('>') {
            flush_bq(&mut out_lines, &mut bq_lines);
        }

        if trimmed.is_empty() {
            out_lines.push(String::new());
        } else if let Some(content) = trimmed.strip_prefix("###### ")
            .or_else(|| trimmed.strip_prefix("##### "))
            .or_else(|| trimmed.strip_prefix("#### "))
            .or_else(|| trimmed.strip_prefix("### "))
            .or_else(|| trimmed.strip_prefix("## "))
            .or_else(|| trimmed.strip_prefix("# "))
        {
            // Heading (H1-H6) → bold (Telegram has no heading tags)
            out_lines.push(format!("<b>{}</b>", render_md_inline(content)));
        } else if trimmed.starts_with("> ") || trimmed == ">" {
            // Blockquote line
            let content = trimmed.strip_prefix("> ").unwrap_or("");
            bq_lines.push(render_md_inline(content));
        } else if trimmed == "---" || trimmed == "***" || trimmed == "___" {
            // Horizontal rule
            out_lines.push("———————————".to_string());
        } else if let Some(content) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
            // Unordered list
            out_lines.push(format!("• {}", render_md_inline(content)));
        } else if RE_MD_OL.is_match(trimmed) {
            // Ordered list: keep the original numbering
            out_lines.push(render_md_inline(trimmed));
        } else {
            // Normal paragraph
            out_lines.push(render_md_inline(trimmed));
        }
    }

    // Flush any unclosed trailing code block (fault tolerance: still output
    // the accumulated content when the fence is not closed)
    if in_code_block {
        flush_code(&mut out_lines, &mut code_lines, &code_lang, &mut pre_blocks);
    }
    // Flush trailing blockquote
    flush_bq(&mut out_lines, &mut bq_lines);

    // 4) Join and restore the protected <a> tags
    let mut result = out_lines.join("\n");
    for (i, anchor) in anchors.iter().enumerate() {
        let token = format!("\u{FFFC}{}\u{FFFC}", i);
        result = result.replace(&token, anchor);
    }

    // 5) Collapse excess blank lines (3+ → 2) + trim leading/trailing
    //    whitespace. pre blocks are protected by placeholders, so their
    //    internal blank lines are unaffected.
    let result = crate::fetcher::RE_BLANK_LINES.replace_all(&result, "\n\n").to_string();

    // 6) Restore the pre code block placeholders
    let mut result = result;
    for (i, html) in pre_blocks.iter().enumerate() {
        let token = format!("\u{FFFE}PB{}\u{FFFE}", i);
        result = result.replace(&token, html);
    }

    result.trim().to_string()
}

/// Strip decorative UI icon <img> tags from weibo text HTML.
/// The weibo API embeds emoji icons (face.t.sinajs.cn) and card thumbnails
/// (h5.sinaimg.cn/upload/) as <img> tags in the text body. These are NOT
/// content images (real content images come from mblog["pics"] and are added
/// separately). If left in the body, media extraction picks them up and
/// Telegram rejects them with PHOTO_INVALID_DIMENSIONS.
///
/// - <img> with alt text → replaced by the alt text (emoji marker text is
///   preserved and later converted to Unicode emojis)
/// - <img> without alt → removed entirely (card thumbnails, decorative icons)
fn strip_ui_icons(html: &str) -> String {
    // 1) Imgs WITH alt text → keep the alt text (emoji icons).
    let s = crate::fetcher::RE_IMG_ALT.replace_all(html, "$1").to_string();

    // 2) Any remaining <img> tags (no alt) → remove entirely.
    let s = crate::fetcher::RE_IMG.replace_all(&s, "").to_string();

    // 3) Convert common platform emoji text markers to Unicode emojis.
    //    Platform-specific ones without a universal equivalent stay as-is.
    convert_emoji_markers(&s)
}

/// Weibo emoji marker → Unicode emoji mapping.
/// Merged with rsstt (Rongronggg9/RSS-to-Telegram-Bot) EMOJIFY_MAP for
/// broader coverage. Unmapped markers are left unchanged.
const EMOJI_MAP: &[(&str, &str)] = &[
    // Emotions - positive
    ("[心]", "❤️"),
    ("[爱心]", "❤️"),
    ("[红心]", "❤️"),
    ("[给你小心心]", "💝"),
    ("[赞]", "👍"),
    ("[good]", "👍"),
    ("[ok]", "👌"),
    ("[开心]", "😄"),
    ("[哈哈]", "😄"),
    ("[笑哈哈]", "😆"),
    ("[太开心]", "🥳"),
    ("[嘻嘻]", "😁"),
    ("[可爱]", "🥰"),
    ("[亲亲]", "😘"),
    ("[爱你]", "😍"),
    ("[色]", "😍"),
    ("[花心]", "😍"),
    ("[害羞]", "😊"),
    ("[微笑]", "🙂"),
    ("[挤眼]", "😜"),
    ("[馋嘴]", "😋"),
    ("[偷笑]", "🤭"),
    ("[阴险]", "😏"),
    ("[酷]", "😎"),
    ("[并不简单]", "🧐"),
    ("[思考]", "🤔"),
    ("[抱抱]", "🤗"),
    // Emotions - laugh-cry family
    ("[笑哭]", "😂"),
    ("[笑cry]", "😂"),
    ("[允悲]", "😂"),
    // Emotions - negative
    ("[悲伤]", "😢"),
    ("[泪]", "😢"),
    ("[哭]", "😭"),
    ("[大哭]", "😭"),
    ("[难过]", "😞"),
    ("[失望]", "😞"),
    ("[伤心]", "💔"),
    ("[生气]", "😠"),
    ("[怒]", "😠"),
    ("[哼]", "😠"),
    ("[怒骂]", "🤬"),
    ("[骂]", "🤬"),
    ("[吃惊]", "😲"),
    ("[惊讶]", "😲"),
    ("[傻眼]", "😮"),
    ("[晕]", "😵"),
    ("[抓狂]", "😖"),
    ("[捂脸]", "🤦"),
    ("[黑线]", "😑"),
    ("[汗]", "😓"),
    ("[可怜]", "🥺"),
    ("[鄙视]", "🖕"),
    ("[白眼]", "🙄"),
    ("[疑问]", "❓"),
    // Emotions - states
    ("[困]", "😴"),
    ("[睡觉]", "😴"),
    ("[睡]", "😪"),
    ("[哈欠]", "🥱"),
    ("[感冒]", "😷"),
    ("[生病]", "🤒"),
    ("[吐]", "🤮"),
    // Actions
    ("[鼓掌]", "👏"),
    ("[握手]", "🤝"),
    ("[抱拳]", "🙏"),
    ("[合十]", "🙏"),
    ("[作揖]", "🙏"),
    ("[加油]", "💪"),
    ("[强]", "💪"),
    ("[胜利]", "✌️"),
    ("[耶]", "✌️"),
    ("[666]", "🤙"),
    ("[弱]", "👎"),
    ("[NO]", "✋"),
    ("[拳头]", "✊"),
    ("[haha]", "🤟"),
    ("[拜拜]", "👋"),
    ("[摊手]", "🤷"),
    ("[跪了]", "🧎"),
    // Animals
    ("[doge]", "🐶"),
    ("[二哈]", "🐕"),
    ("[喵喵]", "🐱"),
    ("[熊猫]", "🐼"),
    ("[兔子]", "🐰"),
    ("[猪头]", "🐷"),
    ("[牛]", "🐂"),
    // Food and drinks
    ("[吃瓜]", "🍉"),
    ("[瓜]", "🍉"),
    ("[酸]", "🍋"),
    ("[啤酒]", "🍺"),
    ("[干杯]", "🍻"),
    ("[可乐]", "🥤"),
    ("[蛋糕]", "🎂"),
    ("[棒棒糖]", "🍭"),
    ("[炸鸡腿]", "🍗"),
    ("[五仁月饼]", "🥮"),
    // Nature and weather
    ("[太阳]", "☀️"),
    ("[月亮]", "🌙"),
    ("[圆月]", "🌕"),
    ("[星星]", "⭐"),
    ("[空星]", "★"),
    ("[彩虹]", "🌈"),
    ("[浮云]", "☁️"),
    ("[下雨]", "🌧️"),
    ("[微风]", "🍃"),
    ("[火]", "🔥"),
    ("[闪电]", "⚡"),
    // Objects
    ("[礼物]", "🎁"),
    ("[鲜花]", "💐"),
    ("[花]", "🌸"),
    ("[蜡烛]", "🕯️"),
    ("[钟]", "⏰"),
    ("[肥皂]", "🧼"),
    ("[绿丝带]", "🎗️"),
    ("[围脖]", "🧣"),
    ("[红包]", "🧧"),
    ("[钱]", "💰"),
    ("[发财]", "💰"),
    ("[100]", "💯"),
    ("[满分]", "💯"),
    ("[看涨]", "📈"),
    ("[看跌]", "📉"),
    // Music and film
    ("[音乐]", "🎵"),
    ("[话筒]", "🎤"),
    ("[相机]", "📷"),
    ("[照相机]", "📷"),
    ("[视频]", "📹"),
    // Transport
    ("[飞机]", "✈️"),
    ("[火车]", "🚄"),
    ("[汽车]", "🚗"),
    ("[自行车]", "🚲"),
    // Sports
    ("[足球]", "⚽"),
    ("[篮球]", "🏀"),
    ("[奖杯]", "🏆"),
    ("[金牌]", "🥇"),
    // Festivals and people
    ("[圣诞老人]", "🎅"),
    ("[全家福]", "👪"),
    ("[围观]", "👀"),
    ("[最右]", "→_→"),
    ("[文明遛狗]", "🐕"),
    ("[点亮平安灯]", "🏮"),
    ("[点亮橙色]", "🖐️"),
    ("[弗莱见钱眼开]", "🤑"),
    ("[骷髅]", "💀"),
    ("[嘘]", "🤫"),
    ("[闭嘴]", "🤐"),
];

/// Pre-compiled regex matching any known emoji marker `[...]` in one pass.
static RE_EMOJI_MARKER: LazyLock<regex::Regex> = LazyLock::new(|| {
    let alts: Vec<&str> = EMOJI_MAP.iter().map(|(k, _)| &k[1..k.len() - 1]).collect();
    let pattern = format!(r"\[(?:{})\]", alts.join("|"));
    regex::Regex::new(&pattern).unwrap()
});

/// O(1) lookup table: marker → emoji.
static EMOJI_LOOKUP: LazyLock<std::collections::HashMap<&'static str, &'static str>> =
    LazyLock::new(|| EMOJI_MAP.iter().copied().collect());

/// Map common platform emoji text markers (e.g. weibo [心]) to Unicode emojis.
/// Single-pass regex replacement; unmapped markers are left unchanged.
fn convert_emoji_markers(text: &str) -> String {
    RE_EMOJI_MARKER
        .replace_all(text, |caps: &regex::Captures| {
            let m = caps.get(0).unwrap().as_str();
            EMOJI_LOOKUP.get(m).copied().unwrap_or(m).to_string()
        })
        .into_owned()
}

/// Strip HTML tags for title extraction.
fn strip_tags(html: &str) -> String {
    crate::fetcher::html_to_plain(html)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_uid_valid() {
        assert_eq!(parse_uid("https://weibo.com/u/2803301701"), Some("2803301701".into()));
        assert_eq!(parse_uid("https://www.weibo.com/u/123"), Some("123".into()));
        assert_eq!(parse_uid("https://m.weibo.cn/u/456?tab=2"), Some("456".into()));
    }

    #[test]
    fn parse_uid_invalid() {
        assert_eq!(parse_uid("https://weibo.com/u/"), None);
        assert_eq!(parse_uid("https://weibo.com/n/某人"), None);
        assert_eq!(parse_uid("https://rsshub.example.com/weibo/user/123"), None);
        assert_eq!(parse_uid("https://weibo.com/7762107285/Rk6p8h0Pa"), None);
    }

    #[test]
    fn absolutize_hrefs_makes_relative_links_absolute() {
        // @user link (single quotes), hashtag search link, status link.
        let html = "回复<a href='/n/风不如斯'>@风不如斯</a>:<a href=\"/search?containerid=100103type%3D1%26q%3D%23话题%23\">#话题#</a><a href=\"/status/123\">全文</a>";
        let out = absolutize_hrefs(html);
        assert!(out.contains("href='https://m.weibo.cn/n/风不如斯'"), "out: {}", out);
        assert!(out.contains("href=\"https://m.weibo.cn/search?containerid=100103type%3D1%26q%3D%23话题%23\""), "out: {}", out);
        assert!(out.contains("href=\"https://m.weibo.cn/status/123\""), "out: {}", out);
        // Absolute URLs must be left untouched.
        let abs = "<a href=\"https://weibo.com/n/x\">@x</a>";
        assert_eq!(absolutize_hrefs(abs), abs);
    }

    #[test]
    fn parse_weibo_time_absolute() {
        // Normal absolute format (weekday consistent with the date). Assert down
        // to the exact instant: 2026-09-25 20:09:08 +0800 = 12:09:08 UTC.
        let t = parse_weibo_time("Fri Sep 25 20:09:08 +0800 2026");
        let expected = chrono::DateTime::parse_from_str(
            "2026-09-25 12:09:08 +0000",
            "%Y-%m-%d %H:%M:%S %z",
        )
        .unwrap()
        .with_timezone(&Utc);
        assert_eq!(t, Some(expected));
        // Key fault tolerance: weibo's weekday field is not guaranteed to
        // match the date. Here the weekday is deliberately wrong (Sep 25 2026
        // is a Friday but written as Mon); after stripping the weekday token
        // the same instant must still be parsed.
        let t2 = parse_weibo_time("Mon Sep 25 20:09:08 +0800 2026");
        assert_eq!(t2, Some(expected));
    }

    #[test]
    fn parse_weibo_time_relative() {
        let now = Utc::now();
        // "刚刚" (just now) ≈ now (±2s tolerance, guards against slow test machines).
        let t = parse_weibo_time("刚刚").expect("just-now form should parse");
        assert!(
            (t - now).num_seconds().abs() <= 2,
            "just-now form should be ~now, got delta {:?}",
            t - now
        );
        // "10分钟前" (10 minutes ago) ≈ now - 10min: assert the offset, not just is_some.
        let t = parse_weibo_time("10分钟前").expect("10-minutes-ago form should parse");
        let delta = (t - (now - chrono::Duration::minutes(10))).num_seconds().abs();
        assert!(delta <= 2, "10-minutes-ago offset wrong by {}s", delta);
        // "3小时前" (3 hours ago) ≈ now - 3h.
        let t = parse_weibo_time("3小时前").expect("3-hours-ago form should parse");
        let delta = (t - (now - chrono::Duration::hours(3))).num_seconds().abs();
        assert!(delta <= 2, "3-hours-ago offset wrong by {}s", delta);
    }

    #[test]
    fn parse_weibo_time_yesterday() {
        // "昨天 HH:MM" (yesterday HH:MM): take yesterday's date in the CST
        // timezone + the given time, then convert to UTC.
        let cst = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let now_cst = Utc::now().with_timezone(&cst);
        let yesterday = (now_cst - chrono::Duration::days(1)).date_naive();
        let t = parse_weibo_time("昨天 08:30").expect("yesterday form should parse");
        let expected = yesterday
            .and_hms_opt(8, 30, 0)
            .unwrap()
            .and_local_timezone(cst)
            .single()
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(t, expected);
    }

    #[test]
    fn parse_weibo_time_mm_dd_past() {
        // "MM-DD" parses to this year's 00:00 CST. Use a date that is
        // certainly in the past (01-01) so the cross-year correction does not
        // trigger; the result should be this year.
        let cst = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let year = Utc::now().with_timezone(&cst).year();
        let t = parse_weibo_time("01-01").expect("01-01 should parse");
        let expected = chrono::NaiveDate::from_ymd_opt(year, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_local_timezone(cst)
            .single()
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(t, expected);
    }

    #[test]
    fn parse_weibo_time_mm_dd_cross_year() {
        // Cross-year correction: weibo relative times carry no year; a parsed
        // "this year MM-DD" that is more than one day ahead of now is treated
        // as last year. Constructed dynamically from the month/day of now+30
        // days so the assertion holds on any run date:
        // - now+30 days does not cross the year boundary → this year's
        //   month/day is >1 day in the future → corrected to last year;
        // - now+30 days already crossed the year boundary → this year's
        //   month/day is already in the past → no correction, stays this year.
        let cst = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let now_cst = Utc::now().with_timezone(&cst);
        let target = (now_cst + chrono::Duration::days(30)).date_naive();
        let mmdd = format!("{:02}-{:02}", target.month(), target.day());
        let t = parse_weibo_time(&mmdd).unwrap_or_else(|| panic!("{} should parse", mmdd));
        let expected_year = if target.year() == now_cst.year() {
            now_cst.year() - 1 // >1 day in the future → cross-year correction to last year
        } else {
            now_cst.year() // this year's month/day is already past → no correction
        };
        let expected = chrono::NaiveDate::from_ymd_opt(expected_year, target.month(), target.day())
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_local_timezone(cst)
            .single()
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(t, expected, "mmdd={}", mmdd);
    }

    #[test]
    fn strip_tags_works() {
        assert_eq!(strip_tags("<a href=\"x\">hello</a><br/>world"), "helloworld");
    }

    #[test]
    fn strip_ui_icons_preserves_emoji_alt_and_removes_icons() {
        // Emoji icon with alt → converted to Unicode emoji.
        let html = r#"今天心情<img alt="[酷]" src="https://face.t.sinajs.cn/t4/cool.png">不错"#;
        let out = strip_ui_icons(html);
        assert_eq!(out, "今天心情😎不错");

        // Card thumbnail without alt → removed entirely.
        let html2 = r#"看视频<img src="https://h5.sinaimg.cn/upload/2015/09/25/3/timeline_card_small_video_default.png">结束"#;
        let out2 = strip_ui_icons(html2);
        assert_eq!(out2, "看视频结束");

        // Mixed: emoji converted, card icon removed.
        let html3 = r#"<img alt="[doge]" src="https://face.t.sinajs.cn/doge.png"><img src="https://h5.sinaimg.cn/upload/icon.png">"#;
        let out3 = strip_ui_icons(html3);
        assert_eq!(out3, "🐶");

        // Unmapped emoji stays as text.
        let html4 = r#"<img alt="[某平台专属表情]" src="https://face.t.sinajs.cn/x.png">"#;
        let out4 = strip_ui_icons(html4);
        assert_eq!(out4, "[某平台专属表情]");
    }

    #[test]
    fn extract_inline_images_html_plain_form() {
        // Simple form: <a href="sinaurl">label</a>
        let html = r#"前文<a href="https://weibo.cn/sinaurl?u=https%3A%2F%2Fwx2.sinaimg.cn%2Flarge%2Fabc.jpg">查看图片</a>后文"#;
        let (text, urls) = extract_inline_images(html);
        assert_eq!(urls, vec!["https://wx2.sinaimg.cn/large/abc.jpg"]);
        assert!(text.contains(r#"<a href="https://weibo.cn/sinaurl?u=https%3A%2F%2Fwx2.sinaimg.cn%2Flarge%2Fabc.jpg">查看图片</a>"#), "text: {}", text);
        assert!(text.starts_with("前文"));
        assert!(text.ends_with("后文"));
    }

    #[test]
    fn extract_inline_images_html_span_wrapped_form() {
        // Real API form (observed 2026-10-06): the anchor wraps a url-icon
        // span (its <img> is already stripped by strip_ui_icons, leaving an
        // empty span) plus a surl-text span holding the label.
        // The label is not fixed: "View image" (2026-10-06) / "Comment image" (2026-10-13).
        let html = r#"就是抖包袱！ <a  href="https://weibo.cn/sinaurl?u=https%3A%2F%2Fwx2.sinaimg.cn%2Flarge%2F53899d01ly1ihrm04xdknj219a19a1ky.jpg" data-hide=""><span class='url-icon'></span><span class="surl-text">查看图片</span></a>"#;
        let (text, urls) = extract_inline_images(html);
        assert_eq!(urls, vec!["https://wx2.sinaimg.cn/large/53899d01ly1ihrm04xdknj219a19a1ky.jpg"]);
        assert!(text.starts_with("就是抖包袱！ "), "text: {}", text);
        assert!(text.contains("查看图片</a>"), "text: {}", text);
        // The span shells must be gone from the preserved link.
        assert!(!text.contains("url-icon"), "text: {}", text);

        // "Comment image" label: same structure, different surl-text.
        let html2 = r#"转发下？ <a  href="https://weibo.cn/sinaurl?u=https%3A%2F%2Fwx4.sinaimg.cn%2Flarge%2Fbaf3936fgy1ii0h79kjtej20u02run9s.jpg" data-hide=""><span class='url-icon'></span><span class="surl-text">评论配图</span></a>"#;
        let (text2, urls2) = extract_inline_images(html2);
        assert_eq!(urls2, vec!["https://wx4.sinaimg.cn/large/baf3936fgy1ii0h79kjtej20u02run9s.jpg"]);
        assert!(text2.contains("评论配图</a>"), "text: {}", text2);
        assert!(!text2.contains("url-icon"), "text: {}", text2);
    }

    #[test]
    fn extract_inline_images_markdown_form() {
        let md = "看这里 [查看图片](https://weibo.cn/sinaurl?u=https%3A%2F%2Fwx1.sinaimg.cn%2Flarge%2Fdef.jpg) 完";
        let (text, urls) = extract_inline_images(md);
        assert_eq!(urls, vec!["https://wx1.sinaimg.cn/large/def.jpg"]);
        assert!(text.contains("查看图片</a>"), "text: {}", text);
    }

    #[test]
    fn extract_inline_images_non_http_u_ignored() {
        // u param that is not an http(s) URL must not be extracted.
        let html = r#"<a href="https://weibo.cn/sinaurl?u=javascript%3Aalert(1)">查看图片</a>"#;
        let (_text, urls) = extract_inline_images(html);
        assert!(urls.is_empty(), "urls: {:?}", urls);
    }

    #[test]
    fn extract_inline_images_non_image_url_filtered() {
        // v1.10.11: sinaurl decoding to a non-image URL (e.g. an HTML page
        // like peopleapp.com) must NOT be extracted as media.  The link is
        // still preserved as a clickable hyperlink.
        let html = r#"正文<a href="https://weibo.cn/sinaurl?u=https%3A%2F%2Fwww.peopleapp.com%2Farticle%2F12345">网页链接</a>结尾"#;
        let (text, urls) = extract_inline_images(html);
        assert!(urls.is_empty(), "non-image URL must be filtered, got: {:?}", urls);
        // The hyperlink is still preserved in the text.
        assert!(text.contains("网页链接</a>"), "text: {}", text);
    }

    #[test]
    fn extract_inline_images_sinaimg_no_extension_accepted() {
        // sinaimg.cn URLs without a file extension are still accepted
        // (weibo CDN sometimes serves extensionless image URLs).
        let html = r#"<a href="https://weibo.cn/sinaurl?u=https%3A%2F%2Fwx1.sinaimg.cn%2Flarge%2F006Xdasfly1abc">查看图片</a>"#;
        let (_text, urls) = extract_inline_images(html);
        assert_eq!(urls, vec!["https://wx1.sinaimg.cn/large/006Xdasfly1abc"]);
    }

    #[test]
    fn extract_inline_images_non_sinaimg_with_image_ext_accepted() {
        // Non-sinaimg hosts with a recognised image extension are accepted.
        let html = r#"<a href="https://weibo.cn/sinaurl?u=https%3A%2F%2Fexample.com%2Fphoto.png">查看图片</a>"#;
        let (_text, urls) = extract_inline_images(html);
        assert_eq!(urls, vec!["https://example.com/photo.png"]);
    }

    #[test]
    fn extract_inline_images_non_sinaimg_no_ext_filtered() {
        // Non-sinaimg hosts without an image extension are filtered.
        let html = r#"<a href="https://weibo.cn/sinaurl?u=https%3A%2F%2Fexample.com%2Fpage">网页链接</a>"#;
        let (_text, urls) = extract_inline_images(html);
        assert!(urls.is_empty(), "urls: {:?}", urls);
    }

    #[test]
    fn is_image_url_variants() {
        // sinaimg.cn with and without subdomain
        assert!(is_image_url("https://wx2.sinaimg.cn/large/abc.jpg"));
        assert!(is_image_url("https://sinaimg.cn/large/abc"));
        assert!(is_image_url("https://tvax1.sinaimg.cn/large/xyz"));
        // image extensions on other hosts
        assert!(is_image_url("https://example.com/a.png"));
        assert!(is_image_url("https://example.com/a.GIF"));
        assert!(is_image_url("https://example.com/a.webp"));
        assert!(is_image_url("https://example.com/a.jpeg?q=1"));
        // non-image
        assert!(!is_image_url("https://www.peopleapp.com/article/12345"));
        assert!(!is_image_url("https://example.com/page.html"));
        assert!(!is_image_url("https://example.com/"));
        // domain containing "sinaimg.cn" as substring but not actually sinaimg.cn,
        // and no image extension → rejected
        assert!(!is_image_url("https://notsinaimg.cn/abc"));
    }

    #[test]
    fn extract_inline_images_video_card_not_matched() {
        // video.weibo.com cards carry no sinaurl?u= and must not match.
        let html = r#"<a  href="https://video.weibo.com/show?fid=1034:123" data-hide=""><span class='url-icon'></span><span class="surl-text">某某的微博视频</span></a>"#;
        let (text, urls) = extract_inline_images(html);
        assert!(urls.is_empty());
        assert_eq!(text, html);
    }

    // ── Markdown rendering tests ──

    #[test]
    fn md_headings_become_bold() {
        let input = "# 大标题<br /><br />## 小节标题<br /><br />正文内容";
        let out = render_weibo_markdown(input);
        assert!(out.contains("<b>大标题</b>"), "out: {}", out);
        assert!(out.contains("<b>小节标题</b>"), "out: {}", out);
        assert!(out.contains("正文内容"), "out: {}", out);
        // No raw # left
        assert!(!out.contains("# "), "out: {}", out);
    }

    #[test]
    fn md_bold_italic_strike_code() {
        let input = "**加粗**和*斜体*和~~删除线~~和`代码`";
        let out = render_weibo_markdown(input);
        assert!(out.contains("<b>加粗</b>"), "out: {}", out);
        assert!(out.contains("<i>斜体</i>"), "out: {}", out);
        assert!(out.contains("<s>删除线</s>"), "out: {}", out);
        assert!(out.contains("<code>代码</code>"), "out: {}", out);
    }

    #[test]
    fn md_links_become_anchors() {
        let input = "看[这里](https://example.com)了解";
        let out = render_weibo_markdown(input);
        assert!(out.contains(r#"<a href="https://example.com">这里</a>"#), "out: {}", out);
    }

    #[test]
    fn md_existing_anchors_preserved() {
        // Output of absolutize_hrefs must be preserved verbatim, not escaped
        let input = "前文<a href=\"https://m.weibo.cn/n/某人\">@某人</a>后文";
        let out = render_weibo_markdown(input);
        assert!(out.contains(r#"<a href="https://m.weibo.cn/n/某人">@某人</a>"#), "out: {}", out);
    }

    #[test]
    fn md_unordered_list() {
        let input = "- 第一项<br />- 第二项";
        let out = render_weibo_markdown(input);
        assert!(out.contains("• 第一项"), "out: {}", out);
        assert!(out.contains("• 第二项"), "out: {}", out);
    }

    #[test]
    fn md_ordered_list_kept() {
        let input = "1. 第一步<br />2. 第二步";
        let out = render_weibo_markdown(input);
        assert!(out.contains("1. 第一步"), "out: {}", out);
        assert!(out.contains("2. 第二步"), "out: {}", out);
    }

    #[test]
    fn md_blockquote_wrapped() {
        // Note: the weibo API may escape > as &gt;, but unescaping is done
        // upstream; render_weibo_markdown only handles the raw > form.
        let input = "引用如下<br />> 被引用的话<br />引用结束";
        let out = render_weibo_markdown(input);
        assert!(out.contains("<blockquote>被引用的话</blockquote>"), "out: {}", out);
        assert!(out.contains("引用如下"), "out: {}", out);
        assert!(out.contains("引用结束"), "out: {}", out);
    }

    #[test]
    fn md_hr_becomes_line() {
        let input = "上面<br />---<br />下面";
        let out = render_weibo_markdown(input);
        assert!(out.contains("———————————"), "out: {}", out);
    }

    #[test]
    fn md_html_entities_escaped() {
        let input = "A & B < C > D";
        let out = render_weibo_markdown(input);
        assert!(out.contains("A &amp; B &lt; C &gt; D"), "out: {}", out);
    }

    #[test]
    fn md_real_post_structure() {
        // Simulate the structure of a real md_render_mark=1 post
        let input = "# 节后A股有望\u{201c}量价齐升\u{201d}<br /><br />来源：证券时报<br /><br />## 广发证券：节后A股市场有望\u{201c}量价齐升\u{201d}<br /><br />本轮科技产业周期大概率处于加速上行。";
        let out = render_weibo_markdown(input);
        assert!(out.contains("<b>节后A股有望\u{201c}量价齐升\u{201d}</b>"), "out: {}", out);
        assert!(out.contains("<b>广发证券：节后A股市场有望\u{201c}量价齐升\u{201d}</b>"), "out: {}", out);
        assert!(out.contains("来源：证券时报"), "out: {}", out);
        assert!(out.contains("本轮科技产业周期大概率处于加速上行。"), "out: {}", out);
        // Paragraphs are separated by blank lines
        assert!(out.contains("\n\n"), "out: {}", out);
    }

    #[test]
    fn md_sentinel_chars_stripped() {
        // Sentinel characters in the input do not cause placeholder conflicts
        let input = "\u{FFFC}hello\u{FFFE}world";
        let out = render_weibo_markdown(input);
        assert_eq!(out, "helloworld");
    }

    // ── Fenced code block tests ──

    #[test]
    fn md_fenced_code_block_basic() {
        let input = "前文<br />```python<br />def foo():<br />    return 1<br />```<br />后文";
        let out = render_weibo_markdown(input);
        let expected = "<pre><code class=\"language-python\">def foo():\n    return 1</code></pre>";
        assert!(out.contains(expected), "out: {}", out);
        assert!(out.contains("前文"), "out: {}", out);
        assert!(out.contains("后文"), "out: {}", out);
        // Fence markers must not leak
        assert!(!out.contains("```"), "out: {}", out);
    }

    #[test]
    fn md_fenced_code_no_lang_tag() {
        // No class attribute when there is no language tag
        let input = "```<br />x = 1<br />```";
        let out = render_weibo_markdown(input);
        assert!(out.contains("<pre><code>x = 1</code></pre>"), "out: {}", out);
        assert!(!out.contains("class="), "out: {}", out);
    }

    #[test]
    fn md_fenced_code_escapes_html_and_skips_formatting() {
        // Code content must escape HTML special characters and skip markdown
        // formatting
        let input = "```<br />a < b && c > d<br />**not bold**<br />```";
        let out = render_weibo_markdown(input);
        assert!(out.contains("a &lt; b &amp;&amp; c &gt; d"), "out: {}", out);
        assert!(out.contains("**not bold**"), "out: {}", out);
        assert!(!out.contains("<b>not bold</b>"), "out: {}", out);
    }

    #[test]
    fn md_fenced_code_preserves_indentation() {
        let input = "```<br />    indented line<br />no indent<br />```";
        let out = render_weibo_markdown(input);
        assert!(out.contains("    indented line"), "out: {}", out);
    }

    #[test]
    fn md_fenced_code_blank_lines_preserved() {
        // Blank lines inside the code block are preserved as-is (split_html
        // treats <pre> atomically, so they are not split into paragraphs)
        let input = "```<br />line1<br /><br /><br />line2<br />```";
        let out = render_weibo_markdown(input);
        assert!(out.contains("line1\n\n\nline2"), "out: {}", out);
    }

    #[test]
    fn md_fenced_code_unclosed_flushed() {
        // Unclosed fence: fault-tolerantly output the accumulated content
        let input = "```<br />orphan code";
        let out = render_weibo_markdown(input);
        assert!(out.contains("<pre><code>orphan code</code></pre>"), "out: {}", out);
    }

    #[test]
    fn md_fenced_code_hash_not_heading() {
        // A # inside a code block is not treated as a heading
        let input = "```<br /># comment<br />x = 1<br />```";
        let out = render_weibo_markdown(input);
        assert!(out.contains("# comment"), "out: {}", out);
        assert!(!out.contains("<b>comment</b>"), "out: {}", out);
    }
}
