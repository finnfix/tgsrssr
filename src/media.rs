use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use feed_rs::model::Entry;
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::config::Config;
use crate::fetcher;
use crate::state::AppState;
use crate::ssrf::is_private_url;

/// Static regexes for media URL extraction (extract_media_urls runs once per
/// entry).
static RE_VIDEO_BLOCK: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?is)<video\b[^>]*>.*?</video>").unwrap());
static RE_SOURCE_SRC: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"<source\b[^>]*?src\s*=\s*["']([^"']+)["']"#).unwrap());
static RE_VIDEO_SRC: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"(?is)<video\b[^>]*?\bsrc\s*=\s*["']([^"']+)["']"#).unwrap());
/// Extract poster attribute from <video> tags (cover image URL for fallback).
static RE_VIDEO_POSTER: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"<video\b[^>]*?\bposter\s*=\s*["']([^"']+)["']"#).unwrap());
static RE_ANCHOR_BLOCK: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"(?is)<a\b[^>]*?href\s*=\s*["']([^"']*)["'][^>]*>.*?</a>"#).unwrap());
static RE_URL_ICON_SPAN: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"(?is)<span\s[^>]*class\s*=\s*["']?url-icon["']?[^>]*>.*?</span>"#).unwrap());
static RE_IMG_SRC: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"<img\b[^>]*?(?:src|data-src)\s*=\s*["']([^"']+)["']"#).unwrap());
/// Full <img> tag matcher (used for extracting images inside the body).
static RE_IMG_TAG: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?is)<img\b[^>]*>").unwrap());

/// A downloaded media file ready to be sent.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MediaItem {
    pub path: PathBuf,
    pub kind: MediaKind,
    pub width: u32,
    pub height: u32,
    pub size: u64,
    /// Animated GIF: sent via sendAnimation instead of sendPhoto.
    pub is_gif: bool,
    /// Video-only: locally generated JPEG thumbnail and duration (seconds).
    /// Telegram does not auto-generate previews for larger videos; the
    /// thumbnail/duration must be provided explicitly in sendVideo for the
    /// message list to show a thumbnail and duration.
    /// Long/large images sent as documents also get a thumbnail
    /// (sendDocument supports it too).
    pub thumb_path: Option<PathBuf>,
    pub duration_secs: Option<u32>,
    /// SHA-256 of the file content (hex). Used as the file_id cache key:
    /// identical content (e.g. the same Weibo video referenced by multiple
    /// entries) hits the cache and reuses the file_id directly, avoiding
    /// duplicate uploads.
    pub content_hash: String,
    /// Inline images from the retweeted original post (decoded from Weibo
    /// [view image] text links, passed in via EntryMeta.retweet_images).
    /// At push time they are split out of the main media, merged into one
    /// standalone message that quotes the main message (v1.4.0).
    /// #[serde(default)] keeps compatibility with old outbox serialized data.
    #[serde(default)]
    pub is_retweet: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum MediaKind {
    Image,
    Video,
    Other,
}

/// Telegram sendPhoto hard limit; larger images are sent as documents.
pub const MAX_PHOTO_BYTES: u64 = 10 * 1024 * 1024;
/// The video push cap is now determined by the MAX_VIDEO_PUSH_MB config
/// (see config.rs, default 100MB): oversized videos are not pushed (pipeline
/// clogging + upload timeout risk); the entry degrades to plain text and
/// the admin gets a dedicated notification.
/// Download cap when uploading via memory (official Bot API). The official
/// API rejects uploads above 50MB, so we cap at 49MB to leave headroom.
pub const MAX_VIDEO_MEMORY_BYTES: u64 = 49 * 1024 * 1024;
/// Download cap when the local Bot API reads files directly (file:// path).
pub const MAX_VIDEO_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_MEDIA_PER_ENTRY: usize = 30; // At most 3 groups (≤10 each, Telegram mediaGroup cap)

/// How media reaches the Bot API server.
///
/// - `file` mode (local Bot API + shared mount): we hand the server a
///   `file://` path in *its* filesystem namespace — zero copy, no RAM usage,
///   2GB cap. The path is derived by translating our MEDIA_DIR prefix into
///   BOT_API_MEDIA_DIR, so no absolute path is hardcoded in code.
/// - `memory` mode (official API or empty BOT_API_MEDIA_DIR): read the file
///   and upload bytes; capped at 49MB (official API rejects uploads > 50MB).
#[derive(Debug, Clone)]
pub struct MediaTransport {
    local_prefix: Option<PathBuf>,
    bot_prefix: Option<PathBuf>,
}

impl MediaTransport {
    pub fn new(cfg: &Config) -> Self {
        let use_file = cfg.bot_api_url.is_some() && !cfg.bot_api_media_dir.is_empty();
        if use_file {
            Self {
                local_prefix: Some(cfg.media_dir.clone()),
                bot_prefix: Some(PathBuf::from(&cfg.bot_api_media_dir)),
            }
        } else {
            Self {
                local_prefix: None,
                bot_prefix: None,
            }
        }
    }

    fn use_file(&self) -> bool {
        self.bot_prefix.is_some()
    }

    /// Translate one of our local media paths into the Bot API server's
    /// namespace. Returns None when not translatable (caller falls back to
    /// memory upload).
    pub fn bot_path(&self, local: &Path) -> Option<String> {
        let lp = self.local_prefix.as_ref()?;
        let bp = self.bot_prefix.as_ref()?;
        let rel = local.strip_prefix(lp).ok()?;
        Some(bp.join(rel).to_string_lossy().to_string())
    }

    pub fn max_download_bytes(&self) -> u64 {
        if self.use_file() {
            MAX_VIDEO_FILE_BYTES
        } else {
            MAX_VIDEO_MEMORY_BYTES
        }
    }
}

impl MediaKind {
    fn from_mime_or_ext(mime: &str, url: &str) -> Self {
        if mime.starts_with("image/") {
            return Self::Image;
        }
        if mime.starts_with("video/") {
            return Self::Video;
        }
        let lower = url.to_lowercase();
        let path_part = lower.split('?').next().unwrap_or(&lower);
        const IMG: [&str; 8] = [".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp", ".svg", ".avif"];
        const VID: [&str; 6] = [".mp4", ".mov", ".avi", ".mkv", ".webm", ".m4v"];
        if IMG.iter().any(|e| path_part.ends_with(e)) {
            Self::Image
        } else if VID.iter().any(|e| path_part.ends_with(e)) {
            Self::Video
        } else {
            Self::Other
        }
    }

    fn ext(&self, mime: &str, url: &str) -> String {
        // Prefer extension from URL path
        let lower = url.to_lowercase();
        let path_part = lower.split('?').next().unwrap_or(&lower);
        if let Some(dot) = path_part.rfind('.') {
            let ext: String = path_part[dot + 1..].chars().take(5).collect();
            if ext.chars().all(|c| c.is_ascii_alphanumeric()) && !ext.is_empty() {
                return ext;
            }
        }
        match mime {
            "image/jpeg" | "image/jpg" => "jpg",
            "image/png" => "png",
            "image/gif" => "gif",
            "image/webp" => "webp",
            "image/bmp" => "bmp",
            "video/mp4" => "mp4",
            "video/webm" => "webm",
            "video/quicktime" => "mov",
            _ => match self {
                MediaKind::Image => "jpg",
                MediaKind::Video => "mp4",
                MediaKind::Other => "bin",
            },
        }
        .to_string()
    }
}

/// True for platform placeholder images that are not real content.
/// These are default cover images inserted by the platform when an article
/// has no actual cover photo.
fn is_placeholder_image(url: &str) -> bool {
    let lower = url.to_lowercase();
    // Huxiu default article placeholder
    lower.contains("article_default_picpath")
        // Generic placeholder patterns
        || lower.contains("placeholder")
        || lower.contains("default_pic")
        || lower.contains("default_cover")
}

/// True for Weibo/Sina image hosts that enforce Referer-based hotlink protection.
fn is_weibo_host(host: &str) -> bool {
    host.contains("sinaimg.cn")
        || host.contains("weibocdn.com")
        || host.contains("weibo.com")
        || host.contains("weibo.cn")
}

/// Referer header value that satisfies Weibo/Sina hotlink checks.
fn referer_for(url: &str) -> Option<&'static str> {
    let host = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
        .unwrap_or_default();
    if is_weibo_host(&host) {
        Some("https://weibo.com/")
    } else {
        None
    }
}

/// Weibo/Sina thumbnails embed a size segment in the path
/// (thumbnail/square/bmiddle/mw690/bmw690/orj360/...). Upgrade to `large`
/// so we always fetch the original instead of a cropped preview.
fn upgrade_weibo_size(url: &str) -> String {
    let Ok(mut u) = url::Url::parse(url) else {
        return url.to_string();
    };
    let Some(host) = u.host_str().map(|h| h.to_lowercase()) else {
        return url.to_string();
    };
    if !is_weibo_host(&host) {
        return url.to_string();
    }
    let segs: Vec<String> = u.path_segments().map(|s| s.map(str::to_string).collect()).unwrap_or_default();
    if segs.is_empty() {
        return url.to_string();
    }
    if !is_weibo_size_seg(&segs[0]) {
        return url.to_string();
    }
    let mut new_segs = segs.clone();
    new_segs[0] = "large".to_string();
    let new_path = format!("/{}", new_segs.join("/"));
    u.set_path(&new_path);
    u.to_string()
}

/// True for Weibo/Sina path size segments: thumbnail/square/bmiddle/small/
/// orj360/orj480/large, plus numeric forms mw2000/bmw690/orj960 etc.
fn is_weibo_size_seg(seg: &str) -> bool {
    const NAMED: [&str; 7] = ["thumbnail", "square", "bmiddle", "small", "orj360", "orj480", "large"];
    if NAMED.contains(&seg) {
        return true;
    }
    let digits = seg
        .strip_prefix("bmw")
        .or_else(|| seg.strip_prefix("mw"))
        .or_else(|| seg.strip_prefix("orj"));
    matches!(digits, Some(d) if !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
}

/// Weibo HD video upgrade: page-embedded <video src> is the low-bitrate
/// stream; the HD stream lives in the m.weibo.cn status API under
/// page_info.urls (hd / ultra / high keys). Best effort: query the API and
/// replace the video URL; on any failure keep the original.
async fn upgrade_weibo_video_hd(
    state: &AppState,
    entry: &Entry,
    url: &str,
) -> String {
    // Only for weibo video hosts / weibo status links.
    let host = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
        .unwrap_or_default();
    if !is_weibo_host(&host) {
        return url.to_string();
    }

    // Status id: entry.id is the numeric ID (mblog["id"]); the
    // statuses/show API requires the numeric ID.
    // It cannot be parsed from the entry link — the link's last segment is
    // the bid (base62-encoded like "RlfOxksyS"), and passing it to
    // statuses/show?id= returns an error, silently disabling the HD upgrade.
    let mid = fetcher::entry_id(entry);
    if mid.is_empty() || !mid.chars().all(|c| c.is_ascii_digit()) {
        return url.to_string();
    }

    let user_cookies = state.get_cookie("weibo").await.unwrap_or_default();

    // The desktop API (weibo.com/ajax/statuses/show) playback_list contains
    // 1080p+ (the mobile API caps at 720p). Three-tier attempts:
    //   1) visitor cookie (genvisitor self-issued guest identity, no login
    //      required, preferred);
    //   2) user-configured cookie (fallback when the visitor fails);
    //   3) mobile API for the highest available quality (fallback when the
    //      desktop path fails entirely).
    // Always pick the highest quality; if it exceeds MAX_VIDEO_PUSH_MB, the
    // download stage's over-limit detection skips the push and notifies the
    // owner (no downgrade at the selection stage — keep the "highest quality
    // we can get" semantics).
    if let Some(visitor) = weibo_visitor_cookie(state).await {
        if let Some(hd) = weibo_hd_from_desktop_api(state, &mid, &visitor).await {
            return hd;
        }
    }
    if !user_cookies.is_empty() {
        if let Some(hd) = weibo_hd_from_desktop_api(state, &mid, &user_cookies).await {
            return hd;
        }
    }

    // Fall back to the mobile API (m.weibo.cn/statuses/show), at most 720p.
    let api = format!("https://m.weibo.cn/statuses/show?id={}", mid);
    let client = state.client_for_url(&api);
    let mut req = client
        .get(&api)
        .header("Referer", "https://m.weibo.cn/")
        .header("User-Agent", crate::state::MOBILE_BROWSER_UA)
        .header("Accept", "application/json, text/plain, */*");
    if !user_cookies.is_empty() {
        req = req.header("Cookie", user_cookies);
    }
    let Ok(resp) = req.send().await else {
        return url.to_string();
    };
    let Ok(json) = resp.json::<serde_json::Value>().await else {
        return url.to_string();
    };

    let urls = json.pointer("/data/page_info/urls").and_then(|v| v.as_object());
    let Some(urls) = urls else {
        return url.to_string();
    };
    // Mobile key names (confirmed via RSSHub): mp4_720p_mp4 / hevc_mp4_hd /
    // mp4_hd_mp4 / mp4_ld_mp4, plus the legacy ultra/hd/high/medium. Try
    // from highest to lowest quality.
    for key in [
        "ultra", "hd", "high", "medium",
        "mp4_4k_mp4", "mp4_1080p_mp4", "mp4_720p_mp4", "hevc_mp4_hd", "mp4_hd_mp4", "mp4_ld_mp4",
    ] {
        if let Some(v) = urls.get(key).and_then(|v| v.as_str()) {
            if !v.is_empty() {
                debug!(mid = %mid, key = key, "weibo HD video resolved (mobile api)");
                return v.to_string();
            }
        }
    }
    url.to_string()
}

/// Weibo visitor cookie (genvisitor/incarnate self-issued guest identity),
/// used to fetch playback_list from the desktop video API without login.
/// Same mechanism as yt-dlp WeiboBaseIE._update_visitor_cookies:
///   1) POST passport.weibo.com/visitor/genvisitor with a browser fingerprint
///      → get a tid;
///   2) GET passport.weibo.com/visitor/visitor?a=incarnate&t=<tid>... →
///      Set-Cookie plants SUB/SUBP.
///
/// In-memory cache with 1h TTL (the visitor identity's validity far exceeds
/// 1h, but refresh conservatively to avoid risk control).
/// Returns None on failure (silent; does not affect the main flow).
async fn weibo_visitor_cookie(state: &AppState) -> Option<String> {
    const TTL: std::time::Duration = std::time::Duration::from_secs(3600);

    // Cache hit
    {
        let guard = state.weibo_visitor_cookie.lock().await;
        if let Some((cookie, ts)) = guard.as_ref() {
            if ts.elapsed() < TTL {
                return Some(cookie.clone());
            }
        }
    }

    // Step 1: genvisitor — submit a fabricated browser fingerprint, get a tid
    let gen_url = "https://passport.weibo.com/visitor/genvisitor";
    let client = state.client_for_url(gen_url);
    let fp = r#"{"os":"1","browser":"Chrome130,0,0,0","fonts":"undefined","screenInfo":"1920*1080*24","plugins":""}"#;
    let body = format!("cb=gen_callback&fp={}", urlencoding(fp));
    let resp = client
        .post(gen_url)
        .header("Referer", "https://weibo.com/")
        .header("User-Agent", crate::state::BROWSER_UA)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .ok()?;
    let text = resp.text().await.ok()?;

    // The response is JSONP: gen_callback({...}); strip the wrapper to get
    // the JSON
    let json_str = text
        .strip_prefix("gen_callback(")
        .and_then(|s| s.strip_suffix(");"))
        .or_else(|| text.strip_prefix("gen_callback(").and_then(|s| s.strip_suffix(")")))?;
    let data: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let tid = data.pointer("/data/tid").and_then(|v| v.as_str())?;
    let new_tid = data.pointer("/data/new_tid").and_then(|v| v.as_bool()).unwrap_or(false);
    let confidence = data.pointer("/data/confidence").and_then(|v| v.as_i64()).unwrap_or(100);

    // Step 2: incarnate — exchange the tid for guest SUB/SUBP cookies
    let w = if new_tid { 3 } else { 2 };
    let incarnate_url = format!(
        "https://passport.weibo.com/visitor/visitor?a=incarnate&t={}&w={}&c={:03}&gc=&cb=cross_domain&from=weibo&_rand=0.{}",
        urlencoding(tid), w, confidence,
        // Simple pseudo-random: use the nanosecond part of the timestamp
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(123456789)
    );
    let resp2 = client
        .get(&incarnate_url)
        .header("Referer", "https://weibo.com/")
        .header("User-Agent", crate::state::BROWSER_UA)
        .send()
        .await
        .ok()?;

    // Extract SUB and SUBP from the Set-Cookie headers
    let mut sub = String::new();
    let mut subp = String::new();
    for value in resp2.headers().get_all("set-cookie") {
        let Ok(s) = value.to_str() else { continue };
        if let Some(v) = extract_cookie_value(s, "SUB") {
            sub = v;
        } else if let Some(v) = extract_cookie_value(s, "SUBP") {
            subp = v;
        }
    }
    if sub.is_empty() {
        debug!("weibo visitor cookie: incarnate returned no SUB");
        return None;
    }
    let cookie = if subp.is_empty() {
        format!("SUB={}", sub)
    } else {
        format!("SUB={}; SUBP={}", sub, subp)
    };
    debug!("weibo visitor cookie obtained (cached 1h)");

    // Write to cache
    {
        let mut guard = state.weibo_visitor_cookie.lock().await;
        *guard = Some((cookie.clone(), std::time::Instant::now()));
    }
    Some(cookie)
}

/// Extract the value of a named cookie from a Set-Cookie header value.
/// Example: "SUB=xxx; Path=/; Domain=.weibo.com" → Some("xxx")
fn extract_cookie_value(set_cookie: &str, name: &str) -> Option<String> {
    for part in set_cookie.split(';') {
        let part = part.trim();
        if let Some(val) = part.strip_prefix(&format!("{}=", name)) {
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

/// Simple URL encoding (encodes everything except ASCII alphanumerics and
/// the safe characters).
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

/// Get the highest-quality video stream from the desktop API:
/// page_info.media_info.playback_list in weibo.com/ajax/statuses/show
/// contains 1080p+ (the mobile API only goes up to 720p).
/// Each playback_list item's play_info has url/label/width/height/bitrate;
/// pick the mp4 stream with the largest width*height (excluding scrubber
/// thumbnail entries).
/// Always pick the highest quality; over-limit detection happens at the
/// download stage and notifies the owner.
async fn weibo_hd_from_desktop_api(state: &AppState, mid: &str, cookies: &str) -> Option<String> {
    let api = format!("https://weibo.com/ajax/statuses/show?id={}", mid);
    let client = state.client_for_url(&api);
    let mut req = client
        .get(&api)
        .header("Referer", "https://weibo.com/")
        .header("User-Agent", crate::state::BROWSER_UA)
        .header("Accept", "application/json, text/plain, */*")
        .header("X-Requested-With", "XMLHttpRequest");
    if !cookies.is_empty() {
        req = req.header("Cookie", cookies);
    }
    let resp = req.send().await.ok()?;
    let json: serde_json::Value = resp.json().await.ok()?;

    let list = json
        .pointer("/page_info/media_info/playback_list")
        .and_then(|v| v.as_array())?;

    // Pick the entry with the highest resolution; at equal resolution prefer
    // the higher bitrate. Exclude entries without a url or non-mp4 labels.
    let mut best: Option<(i64, i64, &str)> = None; // (pixels, bitrate, url)
    for item in list {
        // An item missing play_info (e.g. ad/preview item) is just skipped;
        // don't give up on the whole list
        let Some(pi) = item.get("play_info") else { continue };
        let vurl = pi.get("url").and_then(|v| v.as_str()).unwrap_or("");
        if vurl.is_empty() {
            continue;
        }
        let label = pi.get("label").and_then(|v| v.as_str()).unwrap_or("");
        // scrubber_* are thumbnail sprite sheets, not video streams
        if label.starts_with("scrubber") {
            continue;
        }
        let w = pi.get("width").and_then(|v| v.as_i64()).unwrap_or(0);
        let h = pi.get("height").and_then(|v| v.as_i64()).unwrap_or(0);
        let bitrate = pi.get("bitrate").and_then(|v| v.as_i64()).unwrap_or(0);
        let pixels = w * h;
        if pixels == 0 {
            continue;
        }
        let better = match &best {
            None => true,
            Some((bp, bb, _)) => pixels > *bp || (pixels == *bp && bitrate > *bb),
        };
        if better {
            best = Some((pixels, bitrate, vurl));
        }
    }

    if let Some((pixels, bitrate, vurl)) = best {
        debug!(mid = %mid, pixels = pixels, bitrate = bitrate, "weibo HD video resolved (desktop api)");
        Some(vurl.to_string())
    } else {
        None
    }
}

/// A video too large to push; reported to the chat owner with entry context.
#[derive(Debug, Clone)]
pub struct OversizedVideo {
    /// Total size in bytes when known (advertised by Content-Length);
    /// None when only discovered mid-stream.
    pub size: Option<u64>,
}

/// Result of preparing media for one entry.
#[derive(Debug, Default)]
pub struct PreparedMedia {
    pub items: Vec<MediaItem>,
    pub warnings: Vec<String>,
    /// Videos skipped for exceeding MAX_VIDEO_PUSH_MB (dedicated owner
    /// notice; the entry itself is pushed as text).
    pub oversized_videos: Vec<OversizedVideo>,
}

impl PreparedMedia {
    /// Empty result (skip_media modes: title+link / title-only do not fetch
    /// media).
    /// Shared by the scheduler and /test to avoid drift between two
    /// hand-written struct literals.
    pub fn empty() -> Self {
        Self { items: Vec::new(), warnings: Vec::new(), oversized_videos: Vec::new() }
    }
}

/// Outcome of downloading one media item.
enum DownloadOutcome {
    Item(MediaItem),
    /// Exceeded the size cap (video cap or global cap).
    Oversized { size: Option<u64> },
    /// Zero-byte response.
    Empty,
}

/// Raw result of one download attempt (before file rename / probing).
/// Streaming to disk: bytes are written directly to a temp file instead of
/// accumulating in memory (prevents OOM on large files).
enum RawOutcome {
    /// Download complete: temp file path + byte count + content hash
    /// (computed incrementally).
    File { path: PathBuf, size: u64, content_hash: String },
    Oversized { size: Option<u64> },
    Empty,
}

/// Media type label (used in warning text).
fn kind_label(lang: crate::i18n::Lang, kind: &MediaKind) -> &'static str {
    use crate::i18n;
    match kind {
        MediaKind::Image => i18n::t(lang, "media_image"),
        MediaKind::Video => i18n::t(lang, "media_video"),
        MediaKind::Other => i18n::t(lang, "media_file"),
    }
}

/// Extract candidate media URLs from an entry: media objects/enclosures first,
/// then <img>/<video>/<source> tags inside the body HTML.
/// Returns (url, kind, mime, is_retweet) — is_retweet is true for images
/// listed in EntryMeta.retweet_images (weibo [view image] links, group-C
/// sidecar).
pub fn extract_media_urls(entry: &Entry, meta: Option<&crate::entrymeta::EntryMeta>) -> Vec<(String, MediaKind, String, bool)> {
    let mut out: Vec<(String, MediaKind, String, bool)> = Vec::new();
    let mut push = |url: String, mime: String, is_rt: bool| {
        if url.is_empty() || out.iter().any(|(u, _, _, _)| *u == url) || out.len() >= MAX_MEDIA_PER_ENTRY
        {
            return;
        }
        // Skip platform placeholder images (not real content).
        if is_placeholder_image(&url) {
            return;
        }
        let kind = MediaKind::from_mime_or_ext(&mime, &url);
        out.push((url, kind, mime, is_rt));
    };

    for media in &entry.media {
        for content in &media.content {
            if let Some(u) = &content.url {
                let mime = content
                    .content_type
                    .as_ref()
                    .map(|t| t.to_string())
                    .unwrap_or_default();
                push(u.to_string(), mime, false);
            }
        }
    }

    // Body HTML: extract media from <video> blocks and <img> tags.
    let body = entry
        .content
        .as_ref()
        .and_then(|c| c.body.clone())
        .or_else(|| entry.summary.as_ref().map(|s| s.content.clone()))
        .unwrap_or_default();
    if !body.is_empty() {
        // 1) <video> blocks: each block contains multiple <source> children
        //    (alternative qualities of the SAME video). Extract only the FIRST
        //    <source> per block to avoid sending duplicates. Some producers
        //    (e.g. the native weibo fetcher) put the URL directly on the
        //    <video src="..."> tag instead of using <source> children.
        for caps in RE_VIDEO_BLOCK.captures_iter(&body) {
            let block = caps.get(0).unwrap().as_str();
            if let Some(src_caps) = RE_SOURCE_SRC.captures(block) {
                let url = crate::htmltext::unescape_html(&src_caps[1]);
                push(url, "video/unknown".to_string(), false);
            } else if let Some(tag_caps) = RE_VIDEO_SRC.captures(block) {
                let url = crate::htmltext::unescape_html(&tag_caps[1]);
                push(url, "video/unknown".to_string(), false);
            }
        }
        // Remove <video> blocks so <img> regex doesn't pick up poster images.
        let mut remaining = RE_VIDEO_BLOCK.replace_all(&body, "").to_string();

        // 2) Remove <a> blocks whose href is a media/video link (video cards).
        //    These contain icon <img> tags that must NOT be sent as images.
        remaining = RE_ANCHOR_BLOCK.replace_all(&remaining, |caps: &regex::Captures| {
            let href = crate::htmltext::unescape_html(&caps[1]);
            if fetcher::is_media_href(&href) {
                String::new()
            } else {
                caps.get(0).unwrap().as_str().to_string()
            }
        }).to_string();

        // 2.5) Remove all <span class="url-icon">…</span> blocks. These are
        //      small icon images (article cards, live cards, link previews,
        //      etc.) that must never be sent as standalone photos.
        remaining = RE_URL_ICON_SPAN.replace_all(&remaining, "").to_string();

        // 3) <img> tags: extract src/data-src as images.
        //    Group C: imgs inside the body are always is_rt=false; retweet
        //    inline images are explicitly injected via meta.retweet_images
        //    below (the former data-rt detection has been removed).
        for caps in RE_IMG_TAG.captures_iter(&remaining) {
            let tag = caps.get(0).unwrap().as_str();
            let Some(src_caps) = RE_IMG_SRC.captures(tag) else { continue };
            let url = crate::htmltext::unescape_html(&src_caps[1]);
            push(url, "image/unknown".to_string(), false);
        }
    }

    // Group-C sidecar: meta.retweet_images explicitly injects retweet inline
    // images (decoded from Weibo [view image] links), flagged with
    // is_retweet=true so they are split into a standalone message at push
    // time.
    if let Some(m) = meta {
        for url in &m.retweet_images {
            push(url.clone(), "image/unknown".to_string(), true);
        }
        // Truncation detection: the push closure silently drops URLs once
        // out reaches MAX_MEDIA_PER_ENTRY.
        // Retweet images may be the entry's only content (the "view image"
        // scenario), so any loss must leave a trace.
        // Dedup does not count as loss (a URL already in out is sent
        // normally); only count those neither injected nor already listed.
        let dropped = m
            .retweet_images
            .iter()
            .filter(|u| !out.iter().any(|(o, _, _, _)| o == *u))
            .count();
        if dropped > 0 {
            warn!(
                entry_id = %fetcher::entry_id(entry),
                dropped = dropped,
                limit = MAX_MEDIA_PER_ENTRY,
                "retweet_images truncated by MAX_MEDIA_PER_ENTRY, images dropped"
            );
        }
    }

    out
}

/// Extract video→poster URL mappings from the entry body.
/// Returns (video_src_url, poster_url) pairs for <video> tags with poster attributes.
/// Used to provide cover image fallback when a video is too large or fails to send.
pub fn extract_video_posters(entry: &Entry) -> Vec<(String, String)> {
    let body = entry
        .content
        .as_ref()
        .and_then(|c| c.body.clone())
        .or_else(|| entry.summary.as_ref().map(|s| s.content.clone()))
        .unwrap_or_default();
    if body.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for caps in RE_VIDEO_BLOCK.captures_iter(&body) {
        let block = caps.get(0).unwrap().as_str();
        let poster = match RE_VIDEO_POSTER.captures(block) {
            Some(pc) => crate::htmltext::unescape_html(&pc[1]),
            None => continue,
        };
        let src = if let Some(sc) = RE_SOURCE_SRC.captures(block) {
            crate::htmltext::unescape_html(&sc[1])
        } else if let Some(vc) = RE_VIDEO_SRC.captures(block) {
            crate::htmltext::unescape_html(&vc[1])
        } else {
            continue;
        };
        if !poster.is_empty() && !src.is_empty() {
            out.push((src, poster));
        }
    }
    out
}

/// Video cover fallback: download the video's cover image and send it as a
/// regular image, avoiding a plain-text entry (#5 fix).
/// Used when a video exceeds the limit or fails to download. On failure it
/// returns silently (the entry stays plain text).
async fn download_cover_fallback(
    state: &AppState,
    poster_map: &std::collections::HashMap<String, String>,
    raw_url: &str,
    outbound: &std::path::Path,
    transport: &MediaTransport,
    out: &mut PreparedMedia,
) {
    let Some(cover) = poster_map.get(raw_url).cloned() else { return };
    let cover_resolved = fetcher::resolve_url(state, &upgrade_weibo_size(&cover));
    match download_one(state, &cover_resolved, &MediaKind::Image, "image/unknown", outbound, transport).await {
        Ok(DownloadOutcome::Item(item)) => {
            out.items.push(item);
        }
        _ => {
            debug!(cover_url = %cover, "cover image download failed, entry stays text-only");
        }
    }
}

/// Download an entry's media into media/outbound/<date>/ and probe image sizes.
/// Returns prepared items plus human-readable warnings (skips/downgrades) and
/// whether any media host is Weibo/Sina (drives the native caption layout).
/// Never fails the push pipeline: on any problem returns whatever was prepared.
pub async fn prepare_entry_media(
    state: &AppState,
    entry: &Entry,
    transport: &MediaTransport,
    meta: Option<&crate::entrymeta::EntryMeta>,
    lang: crate::i18n::Lang,
) -> PreparedMedia {
    let mut out = PreparedMedia::default();
    let candidates = extract_media_urls(entry, meta);
    if candidates.is_empty() {
        return out;
    }

    // Build video→poster mapping for cover image fallback on oversized/failed videos.
    let poster_map: std::collections::HashMap<String, String> = extract_video_posters(entry)
        .into_iter()
        .collect();

    // Directories are named by Local date, consistent with the Local cutoff
    // used in cleanup.rs (previously Utc was used; under UTC+8, directories
    // created in the early morning were attributed to the previous day and
    // cleaned up about 1 day early).
    let date_dir = chrono::Local::now().format("%Y-%m-%d").to_string();
    let outbound = state.config.media_dir.join("outbound").join(&date_dir);
    if let Err(e) = std::fs::create_dir_all(&outbound) {
        warn!(dir = %outbound.display(), error = %e, "cannot create outbound dir");
        return out;
    }

    for (raw_url, kind, mime, is_retweet) in candidates {
        // Weibo-specific: upgrade thumbnail size segment to `large`.
        let mut url = upgrade_weibo_size(&raw_url);
        // Weibo-specific: resolve HD video stream via status API.
        if kind == MediaKind::Video {
            url = upgrade_weibo_video_hd(state, entry, &url).await;
        }
        // Keep the direct URL from before the relay rewrite, for a fallback
        // retry when the relay fails.
        let direct_url = url.clone();
        // Optional img-relay (anti-hotlink bypass service) when configured.
        let url = fetcher::resolve_url(state, &url);
        let via_relay = url != direct_url;

        let _permit = match state.download_semaphore.acquire().await {
            Ok(p) => p,
            Err(_) => break,
        };

        // Download: when the relay fails, retry once with the original
        // direct URL as a fallback (the relay can 502 on large files /
        // region-locked CDNs, while a direct connection often succeeds).
        // Converged to a single final result handled uniformly below
        // (previously the relay/direct result handling was duplicated
        // verbatim and the cover-fallback call was copied in 4 places —
        // high risk of fixing one and missing another).
        let (result, relay_err) = match download_one(state, &url, &kind, &mime, &outbound, transport).await {
            Err(e) if via_relay => {
                debug!(relay_url = %url, direct_url = %direct_url, error = %e, "relay download failed, retrying direct");
                let r = download_one(state, &direct_url, &kind, &mime, &outbound, transport).await;
                (r, Some(e.to_string()))
            }
            other => (other, None),
        };

        // Videos go through the cover fallback uniformly under the three
        // failure shapes — over limit / empty response / download failure
        // (#5 fix) — converged to a single call site after the match, so a
        // newly added failure branch cannot miss the fallback.
        let mut video_needs_cover = false;
        match result {
            Ok(DownloadOutcome::Item(mut item)) => {
                item.is_retweet = is_retweet;
                out.items.push(item);
            }
            Ok(DownloadOutcome::Oversized { size }) => {
                if kind == MediaKind::Video {
                    // Large videos are not pushed: after being recorded, the
                    // scheduler sends a dedicated notification to the admin
                    // in one batch, and the entry itself is degraded to a
                    // plain-text send. The cover image URL is attached for
                    // the fallback send.
                    warn!(entry_id = %fetcher::entry_id(entry), url = %url, size = ?size, "video too large, skipping push");
                    out.oversized_videos.push(OversizedVideo { size });
                    video_needs_cover = true;
                } else {
                    out.warnings.push(crate::i18n::t(lang, "media_oversized_skip").replacen("{}", kind_label(lang, &kind), 1));
                    debug!(url = %url, "media skipped (size limit)");
                }
            }
            Ok(DownloadOutcome::Empty) => {
                let msg = crate::i18n::t(lang, "media_dl_empty").replacen("{}", kind_label(lang, &kind), 1);
                debug!(url = %url, "media skipped (empty response)");
                out.warnings.push(msg);
                if kind == MediaKind::Video {
                    video_needs_cover = true;
                }
            }
            Err(e) => {
                let msg = crate::i18n::t(lang, "media_dl_failed").replacen("{}", kind_label(lang, &kind), 1);
                if let Some(re) = &relay_err {
                    warn!(entry_id = %fetcher::entry_id(entry), url = %direct_url, relay_error = %re, direct_error = %e, "media download failed (relay + direct)");
                } else {
                    warn!(entry_id = %fetcher::entry_id(entry), url = %url, error = %e, "media download failed");
                }
                out.warnings.push(msg);
                if kind == MediaKind::Video {
                    video_needs_cover = true;
                }
            }
        }
        if video_needs_cover {
            download_cover_fallback(state, &poster_map, &raw_url, &outbound, transport, &mut out).await;
        }
        if out.items.len() >= MAX_MEDIA_PER_ENTRY {
            break;
        }
    }

    // #21: ffmpeg/ffprobe probing runs after the semaphore is released (does
    // not occupy a download concurrency slot).
    // Videos: probe the duration + generate a thumbnail; long/large images
    // (sent as documents): generate a thumbnail.
    for item in &mut out.items {
        if item.kind == MediaKind::Video {
            let (thumb, dur) = probe_video_meta(&item.path).await;
            item.thumb_path = thumb;
            item.duration_secs = dur;
        } else if item.kind == MediaKind::Image
            && !item.is_gif
            && image_goes_to_doc(item.size, item.width, item.height, state.config.long_image_aspect_ratio)
        {
            item.thumb_path = make_image_thumb(&item.path).await;
        }
    }

    out
}

async fn download_one(
    state: &AppState,
    url: &str,
    kind: &MediaKind,
    mime: &str,
    outbound: &std::path::Path,
    transport: &MediaTransport,
) -> anyhow::Result<DownloadOutcome> {
    // SSRF protection: reject media URLs pointing at private/loopback
    // addresses (a malicious feed could use them to probe the internal
    // network).
    // Requests routed via a proxy have their connection established on the
    // proxy side, so the local DNS resolution result does not affect the
    // actual connection target — skip the check (otherwise local DNS
    // resolving a CDN domain to a private address would be wrongly blocked,
    // v1.10.14).
    if !state.is_routed_via_proxy(url) && is_private_url(url).await {
        anyhow::bail!("blocked: private/loopback address");
    }

    // Per-host concurrency limit: prevents opening too many connections to a
    // single CDN at once, which would trigger rate limiting / risk control.
    // Stacks with the global download_semaphore (global controls the total,
    // per-host controls a single endpoint).
    let host_sem = state.host_download_semaphore(url).await;
    let _host_permit = match host_sem.acquire().await {
        Ok(p) => p,
        Err(_) => anyhow::bail!("host download semaphore closed"),
    };

    // Media download client: automatic redirects disabled (SSRF protection;
    // redirects are followed manually hop-by-hop below).
    let client = state.media_client_for_url(url);
    // All media types (images/videos/files) are uniformly bounded by
    // MAX_VIDEO_PUSH_MB (default 100MB) and the transport cap (49MB via
    // official API, 2GB via local Bot API): prevents oversized uploads.
    let cap = transport.max_download_bytes().min(state.config.max_video_push_mb * 1024 * 1024);

    // Occasional jitter on the proxy link (connection cut off, response body
    // transfer interrupted, etc.) can cause a single download to fail; here
    // transient network errors get up to 3 attempts, so an image is not lost
    // due to a sporadic fault.
    const ATTEMPTS: u32 = 3;
    let mut result: anyhow::Result<RawOutcome> =
        Err(anyhow::anyhow!("download not attempted"));
    for attempt in 0..ATTEMPTS {
        // The temp file path is created outside the async block so it can be
        // cleaned up on failure.
        let tmp_path = outbound.join(format!(".tmp_{}", uuid::Uuid::new_v4()));
        let tmp = tmp_path.clone();
        result = async {
            // SSRF redirect protection: automatic following disabled,
            // validated manually hop-by-hop (≤5 hops).
            // reqwest follows up to 10 redirects by default without
            // re-running is_private_url; a malicious feed could 302 → an
            // internal address to bypass the entry check.
            const MAX_REDIRECTS: u32 = 5;
            let mut cur_url = url.to_string();
            let mut resp;
            let mut hops = 0u32;
            loop {
                // The client has automatic redirects disabled
                // (media_client_for_url); follow them manually here.
                let mut req = client.get(&cur_url)
                    .timeout(std::time::Duration::from_secs(300));
                if let Some(referer) = referer_for(url) {
                    req = req.header("Referer", referer);
                    req = req.header("User-Agent", crate::state::BROWSER_UA);
                }
                let r = req.send().await?;
                if r.status().is_redirection() {
                    hops += 1;
                    if hops > MAX_REDIRECTS {
                        anyhow::bail!("too many redirects (>{}): {}", MAX_REDIRECTS, url);
                    }
                    let loc = r.headers()
                        .get(reqwest::header::LOCATION)
                        .and_then(|v| v.to_str().ok())
                        .ok_or_else(|| anyhow::anyhow!("redirect without Location header"))?;
                    let next = url::Url::parse(&cur_url)
                        .and_then(|base| base.join(loc))
                        .map_err(|e| anyhow::anyhow!("bad redirect Location: {}", e))?;
                    let next_str = next.to_string();
                    // When routed via a proxy, redirects are also followed on
                // the proxy side, so the local DNS check is meaningless
                // (same as the entry check).
                    if !state.is_routed_via_proxy(&next_str) && is_private_url(&next_str).await {
                        anyhow::bail!("blocked: redirect to private/loopback address");
                    }
                    cur_url = next_str;
                    continue;
                }
                resp = r;
                break;
            }
            if !resp.status().is_success() {
                anyhow::bail!("HTTP {}", resp.status());
            }
            // Pre-check content-length when advertised
            if let Some(len) = resp.content_length() {
                if len > cap {
                    return Ok(RawOutcome::Oversized { size: Some(len) });
                }
            }
            // Streaming to disk: write to the temp file while downloading +
            // compute SHA256 incrementally; do not accumulate bytes in memory
            // (prevents OOM on large files). If the limit is exceeded
            // mid-stream, truncate and clean up the temp file.
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::File::create(&tmp).await?;
            let mut hasher = Sha256::new();
            let mut written: u64 = 0;
            while let Some(chunk) = resp.chunk().await? {
                hasher.update(&chunk);
                file.write_all(&chunk).await?;
                written += chunk.len() as u64;
                if written > cap {
                    drop(file);
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return Ok(RawOutcome::Oversized { size: None });
                }
            }
            if written == 0 {
                drop(file);
                let _ = tokio::fs::remove_file(&tmp).await;
                return Ok(RawOutcome::Empty);
            }
            file.flush().await?;
            drop(file);
            let content_hash = hasher
                .finalize()
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect::<String>();
            Ok(RawOutcome::File { path: tmp, size: written, content_hash })
        }
        .await;

        match &result {
            // Success (got the file, or skipped due to over-limit): no more
            // retries
            Ok(_) => break,
            Err(e) => {
                // Clean up any leftover temp file
                let _ = tokio::fs::remove_file(&tmp_path).await;
                let es = e.to_string();
                // 5xx (502/503/504…) are transient upstream failures (especially
                // img-relay Worker origin-fetch jitter) — retryable; 4xx
                // (403 hotlink protection / 404 resource missing) are
                // deterministic failures — no retry.
                let is_http_5xx = es.starts_with("HTTP 5");
                let transient = is_http_5xx
                    || (!es.starts_with("HTTP ")
                        && (es.contains("error sending request")
                            || es.contains("error decoding response body")
                            // reqwest renders a timeout as "operation timed out"
                            // (no "timeout" substring), so match both spellings;
                            // a mid-body stall that only says "timed out" would
                            // otherwise be classified non-transient and the media
                            // dropped without retry.
                            || es.contains("timed out")
                            || es.contains("timeout")
                            || es.contains("connection")
                            || es.contains("incomplete")));
                if !transient || attempt + 1 == ATTEMPTS {
                    break;
                }
                tracing::debug!(url = %url, attempt = attempt, error = %es, "media download transient error, retrying");
                // 5xx gets a longer backoff (img-relay Worker jitter can last
                // several seconds); network errors get a short backoff.
                let delay = if is_http_5xx { 5 } else { 2 };
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            }
        }
    }
    let (tmp_path, size, content_hash) = match result? {
        RawOutcome::File { path, size, content_hash } => (path, size, content_hash),
        RawOutcome::Oversized { size } => return Ok(DownloadOutcome::Oversized { size }),
        RawOutcome::Empty => return Ok(DownloadOutcome::Empty),
    };

    let ext = kind.ext(mime, url);
    let name = format!("{}_{}.{}", chrono::Utc::now().format("%H%M%S%f"), uuid::Uuid::new_v4(), ext);
    let path = outbound.join(&name);
    std::fs::rename(&tmp_path, &path)?;

    let (width, height) = if *kind == MediaKind::Image {
        match imagesize::size(&path) {
            Ok(dim) => (dim.width as u32, dim.height as u32),
            Err(_) => (0, 0),
        }
    } else {
        (0, 0)
    };

    // GIF detection: only sendAnimation gives correct animated display
    let is_gif = *kind == MediaKind::Image
        && (mime == "image/gif"
            || url
                .to_lowercase()
                .split('?')
                .next()
                .map(|p| p.ends_with(".gif"))
                .unwrap_or(false));

    // #21: ffmpeg/ffprobe probing moved to prepare_entry_media, running
    // after the semaphore is released, to avoid occupying a download
    // concurrency slot during probing. Only basic info is returned here.
    Ok(DownloadOutcome::Item(MediaItem {
        path,
        kind: *kind,
        width,
        height,
        size,
        is_gif,
        thumb_path: None,
        duration_secs: None,
        content_hash,
        is_retweet: false,
    }))
}

/// Determine whether an image will be sent as a document (>10MB / long image
/// / w+h>10000 / compression risk).
/// The classification stage in push.rs reuses this function (single source of
/// truth, prevents rule drift).
///
/// Telegram sendPhoto compression rules: when the longest side > 1280px, the
/// server scales it down to 1280 and re-encodes as JPEG (lossy). For long
/// images with aspect ratio > 4:1, the short side becomes tiny after scaling
/// (e.g. 1080×5000 → 276×1280), severely blurring text/details. Such images
/// are converted to sendDocument to preserve the original. Large images with
/// normal aspect ratios (16:9 etc.) are usually acceptable after compression
/// and still go through sendPhoto.
pub fn image_goes_to_doc(size: u64, width: u32, height: u32, aspect_threshold: f64) -> bool {
    if size > MAX_PHOTO_BYTES {
        return true;
    }
    if width + height > 10000 {
        return true;
    }
    if width > 0 && height > 0 {
        let ratio = (height as f64) / (width as f64);
        // Original rule: extreme long images (aspect ratio above the
        // configured threshold, default 8.0)
        if ratio > aspect_threshold {
            return true;
        }
        // New: longest side > 1280 (Telegram's compression threshold) and
        // aspect ratio > 4:1 → the short side becomes too small after
        // compression; convert to document to preserve the original
        let longest = width.max(height);
        if longest > 1280 && (ratio > 4.0 || ratio < 0.25) {
            return true;
        }
    }
    false
}

/// Run ffmpeg/ffprobe with a timeout: malformed files can hang the process
/// and block the push pipeline indefinitely.
/// kill_on_drop guarantees the child process is killed when the future is
/// dropped on timeout; timeout/failure returns None, and the caller already
/// has silent-degradation logic (a missing thumbnail does not affect media
/// sending).
/// Failure/timeout leaves a warn — if ffmpeg inside the container is broken
/// or missing, all videos would silently have no preview, so the logs must
/// not be trace-free.
async fn run_tool_with_timeout(tool: &str, mut cmd: tokio::process::Command) -> Option<std::process::Output> {
    cmd.kill_on_drop(true);
    match tokio::time::timeout(std::time::Duration::from_secs(30), cmd.output()).await {
        Ok(Ok(o)) => Some(o),
        Ok(Err(e)) => {
            tracing::warn!(tool = tool, error = %e, "media tool spawn failed (missing binary?)");
            None
        }
        Err(_) => {
            tracing::warn!(tool = tool, "media tool timed out after 30s");
            None
        }
    }
}

/// Shared ffmpeg frame-grab thumbnail implementation: grab 1 frame from
/// `path`, scale it to width 320, and write a JPEG to `thumb`.
/// `ss` is the start second (None = no seek). Returns true on success when
/// the output is non-empty.
async fn grab_thumb(path: &std::path::Path, ss: Option<&str>, thumb: &std::path::Path) -> bool {
    let mut cmd = tokio::process::Command::new("ffmpeg");
    cmd.arg("-y");
    if let Some(s) = ss {
        cmd.args(["-ss", s]);
    }
    cmd.arg("-i")
        .arg(path)
        .args(["-frames:v", "1", "-q:v", "4", "-vf", "scale=320:-2"])
        .arg(thumb);
    let ok = run_tool_with_timeout("ffmpeg", cmd)
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);
    let good = ok && thumb.exists() && std::fs::metadata(thumb).map(|m| m.len()).unwrap_or(0) > 0;
    if !good {
        // ffmpeg may have been killed mid-write on timeout (kill_on_drop), or
        // failed after creating a partial/empty file. Remove the leftover so it
        // does not accumulate as an orphan: cleanup.rs only reclaims thumbnails
        // registered on an outbox MediaItem.thumb_path, and a failed grab_thumb
        // never registers one.
        let _ = tokio::fs::remove_file(thumb).await;
    }
    good
}

/// Grab one frame from an image and scale it to a JPEG thumbnail (for the
/// document-send preview). Silently returns None on failure.
async fn make_image_thumb(path: &std::path::Path) -> Option<PathBuf> {
    let thumb = path.with_extension("thumb.jpg");
    if grab_thumb(path, None, &thumb).await {
        Some(thumb)
    } else {
        None
    }
}

/// Get the video duration with ffprobe and grab one frame with ffmpeg to
/// make a JPEG thumbnail (width ≤320; Telegram requires thumbnails ≤200KB).
/// If either step fails, silently degrade to None — the video is sent as
/// usual, just without a preview (same behavior as before this change).
async fn probe_video_meta(path: &std::path::Path) -> (Option<PathBuf>, Option<u32>) {
    // Duration
    let mut probe_cmd = tokio::process::Command::new("ffprobe");
    probe_cmd
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(path);
    let duration_secs = run_tool_with_timeout("ffprobe", probe_cmd)
        .await
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<f64>()
                .ok()
        })
        .map(|d| d.round() as u32);

    // Thumbnail: prefer the frame at the 1st second; if unavailable (short
    // video), take the first frame
    let thumb = path.with_extension("thumb.jpg");
    let mut made = false;
    for ss in ["1", "0"] {
        if grab_thumb(path, Some(ss), &thumb).await {
            made = true;
            break;
        }
    }

    (if made { Some(thumb) } else { None }, duration_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal RSS entry with the given body HTML for testing.
    fn entry_with_body(body_html: &str) -> Entry {
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"><channel><title>t</title>
<item><title>t</title><link>https://weibo.com/1/abc</link>
<description><![CDATA[{}]]></description>
</item></channel></rss>"#,
            body_html
        );
        let feed = feed_rs::parser::parse(xml.as_bytes()).unwrap();
        feed.entries.into_iter().next().unwrap()
    }

    #[test]
    fn url_icon_img_not_extracted_as_media() {
        // Article card: <a> with non-media href containing a url-icon <img>.
        // The icon must NOT be extracted; only the real photo should be.
        let body = r#"正文<img src="https://tvax1.sinaimg.cn/large/abc.jpg" referrerpolicy="no-referrer"><a href="https://card.weibo.com/article/m/show/id/12345"><span class="url-icon"><img src="https://h5.sinaimg.cn/upload/article_icon.png"></span><span class="surl-text">文章标题</span></a>"#;
        let entry = entry_with_body(body);
        let urls = extract_media_urls(&entry, None);
        assert_eq!(urls.len(), 1, "expected only the real photo, got: {:?}", urls);
        assert!(urls[0].0.contains("tvax1.sinaimg.cn"));
    }

    #[test]
    fn video_card_icon_not_extracted() {
        // Video card <a> with media href → entire block removed in step 2.
        let body = r#"文字<a href="https://video.weibo.com/show?fid=1034:1"><span class="url-icon"><img src="https://h5.sinaimg.cn/video_icon.png"></span><span class="surl-text">微博视频</span></a><img src="https://tvax2.sinaimg.cn/large/real.jpg">"#;
        let entry = entry_with_body(body);
        let urls = extract_media_urls(&entry, None);
        assert_eq!(urls.len(), 1, "expected only the real photo, got: {:?}", urls);
        assert!(urls[0].0.contains("tvax2.sinaimg.cn"));
    }

    #[test]
    fn normal_images_still_extracted() {
        // Multiple normal images should all be extracted.
        let body = r#"<img src="https://tvax1.sinaimg.cn/large/a.jpg"><img src="https://tvax2.sinaimg.cn/large/b.jpg">"#;
        let entry = entry_with_body(body);
        let urls = extract_media_urls(&entry, None);
        assert_eq!(urls.len(), 2);
    }

    #[test]
    fn video_src_attribute_extracted() {
        // Native weibo fetcher emits <video src="..."></video> (URL on the
        // video tag, no <source> children). Must still be extracted.
        let body = r#"文字<video src="https://f.video.weibocdn.com/bs/abc.mp4"></video>"#;
        let entry = entry_with_body(body);
        let urls = extract_media_urls(&entry, None);
        assert_eq!(urls.len(), 1, "expected the video, got: {:?}", urls);
        assert!(urls[0].0.contains("weibocdn.com"), "got: {:?}", urls);
    }

    #[test]
    fn retweet_inline_image_flagged() {
        // Group C: retweet inline images are passed in via
        // EntryMeta.retweet_images and flagged with is_retweet.
        let body = r#"<img src="https://tvax1.sinaimg.cn/large/normal.jpg">"#;
        let entry = entry_with_body(body);
        let meta = crate::entrymeta::EntryMeta {
            prerendered_html: false,
            poll: None,
            retweet_images: vec!["https://wx2.sinaimg.cn/large/abc.jpg".to_string()],
        };
        let urls = extract_media_urls(&entry, Some(&meta));
        assert_eq!(urls.len(), 2);
        // Normal images inside the body are not flagged
        assert!(!urls.iter().any(|(u, _, _, rt)| u.contains("tvax1") && *rt), "normal image should not be flagged");
        // Retweet images from meta are flagged
        assert!(urls.iter().any(|(u, _, _, rt)| u.contains("wx2") && *rt), "retweet image should be flagged");
    }

    #[test]
    fn retweet_image_dedup_with_body() {
        // When the body and meta.retweet_images contain the same URL, it is
        // not extracted twice (the dedup closure works).
        let body = r#"<img src="https://wx2.sinaimg.cn/large/abc.jpg">"#;
        let entry = entry_with_body(body);
        let meta = crate::entrymeta::EntryMeta {
            prerendered_html: false,
            poll: None,
            retweet_images: vec!["https://wx2.sinaimg.cn/large/abc.jpg".to_string()],
        };
        let urls = extract_media_urls(&entry, Some(&meta));
        assert_eq!(urls.len(), 1, "same URL in body and meta should dedup, got: {:?}", urls);
    }

    // ── image_goes_to_doc classification tests ──

    #[test]
    fn goes_to_doc_oversized_file() {
        // Anything over 10MB is always converted to a document, regardless of
        // dimensions.
        assert!(image_goes_to_doc(MAX_PHOTO_BYTES + 1, 800, 600, 8.0));
    }

    #[test]
    fn goes_to_doc_dimension_sum() {
        assert!(image_goes_to_doc(1024, 6000, 5000, 8.0));
    }

    #[test]
    fn goes_to_doc_extreme_aspect() {
        // Aspect ratio above the threshold (default 8.0): converted to a
        // document even when the longest side is ≤1280 (original rule).
        assert!(image_goes_to_doc(1024, 100, 900, 8.0));
    }

    #[test]
    fn goes_to_doc_tall_image_over_1280() {
        // Portrait long image 1080×5000 (ratio 4.6:1, longest side >1280) →
        // Telegram would compress it to 276×1280, blurring text → convert to
        // document.
        assert!(image_goes_to_doc(1024 * 1024, 1080, 5000, 8.0));
        // Same for landscape long images (ratio <0.25).
        assert!(image_goes_to_doc(1024 * 1024, 5000, 1080, 8.0));
    }

    #[test]
    fn stays_photo_normal_large_image() {
        // Normal-aspect large image (16:9, 2000×1125): compression to
        // 1280×720 is acceptable, still sent as an image.
        assert!(!image_goes_to_doc(1024 * 1024, 2000, 1125, 8.0));
        // 4:3 portrait image 1600×1200 (ratio 0.75).
        assert!(!image_goes_to_doc(1024 * 1024, 1600, 1200, 8.0));
    }

    #[test]
    fn stays_photo_small_or_borderline() {
        // Longest side ≤1280 does not trigger the new rule (Telegram does
        // not compress).
        assert!(!image_goes_to_doc(1024, 1280, 400, 8.0));
        // Ratio exactly 4.0 with longest side >1280: not triggered (must be
        // >4.0).
        assert!(!image_goes_to_doc(1024 * 1024, 1000, 4000, 8.0));
        // Ratio 3:1 (the old threshold 2.0 would convert to a document; the
        // new threshold 4.0 does not).
        assert!(!image_goes_to_doc(1024 * 1024, 1000, 3000, 8.0));
        // Zero-dimension defense.
        assert!(!image_goes_to_doc(1024, 0, 0, 8.0));
    }
}
