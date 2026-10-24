//! Platform-native fetchers and login-cookie management.
//!
//! A "platform" is a social network fetched natively (bypassing RSS), e.g.
//! weibo. Each platform registers itself in [`PLATFORMS`] with URL matching,
//! canonicalization, and fetch function pointers. Adding a new platform
//! requires only changes in this directory:
//! 1. a fetcher module here (see `weibo.rs`),
//! 2. one entry in [`PLATFORMS`] with match/canonical/fetch pointers.
//!
//! Routing (fetcher.rs, scheduler.rs) iterates the registry — no per-platform
//! if-else chains outside this module.

pub mod telegram;
pub mod weibo;
pub mod x;

use std::future::Future;
use std::pin::Pin;

use crate::i18n::Lang;
use crate::state::AppState;
use feed_rs::model::{Content, Entry, Link, Person, Text};

/// Build a unified feed-rs [`Entry`] from platform-native fields (M1 fix:
/// the weibo/x/telegram fetchers each repeated ~40 lines of isomorphic
/// literals differing only in content_type; collapsed into a single
/// constructor to prevent field drift).
/// `content_type` is the body MIME (after group C it is always text/html;
/// weibo markdown pre-rendering is now marked explicitly via
/// EntryMeta.prerendered_html).
pub fn make_entry(
    id: String,
    title: String,
    author: String,
    body: String,
    content_type: &str,
    link: String,
    published: Option<chrono::DateTime<chrono::Utc>>,
) -> Entry {
    Entry {
        id,
        title: Some(Text {
            content_type: "text/plain".parse().unwrap(),
            src: None,
            content: title,
        }),
        updated: published,
        published,
        authors: vec![Person {
            name: Some(author),
            uri: None,
            email: None,
            role: None,
        }],
        contributors: Vec::new(),
        content: Some(Content {
            body: Some(body),
            // Defensive parsing: all current call sites pass "text/html", so
            // this cannot fail; if an invalid MIME string is passed in the
            // future, fall back to text/html instead of panicking.
            content_type: content_type.parse().unwrap_or_else(|_| "text/html".parse().unwrap()),
            length: None,
            src: None,
        }),
        links: vec![Link {
            href: link,
            target: None,
            rel: None,
            media_type: None,
            href_lang: None,
            title: None,
            length: None,
        }],
        summary: None,
        categories: Vec::new(),
        media: Vec::new(),
        rights: None,
        source: None,
        language: None,
        base: None,
    }
}

/// Typed platform failures. Authentication expiry is the only class that must
/// set the cookie-expired flag and notify the admin; before this type the
/// scheduler keyed that decision on the substring "cookie", so an x.com
/// 429/5xx/Cloudflare response without ct0 was misclassified as an expired
/// cookie and counted toward auto-disable.
#[derive(Debug)]
pub enum PlatformError {
    AuthExpired {
        platform: &'static str,
        detail: String,
    },
    RateLimited {
        platform: &'static str,
        status: u16,
    },
}

impl std::fmt::Display for PlatformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlatformError::AuthExpired { platform, detail } => {
                write!(f, "{} cookie expired: {}; please update via /cookies", platform, detail)
            }
            PlatformError::RateLimited { platform, status } => {
                write!(f, "{} rate limited (HTTP {}); will retry later", platform, status)
            }
        }
    }
}

impl std::error::Error for PlatformError {}

/// Build an `AuthExpired` error (keeps the "cookie" wording for user-facing
/// compatibility, but the scheduler detects it by type first).
pub fn auth_expired(platform: &'static str, detail: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(PlatformError::AuthExpired {
        platform,
        detail: detail.into(),
    })
}

/// Build a `RateLimited` error: transient, must not be treated as auth expiry.
pub fn rate_limited(platform: &'static str, status: u16) -> anyhow::Error {
    anyhow::Error::new(PlatformError::RateLimited { platform, status })
}

/// Async fetch function registered per platform. Receives the full feed URL
/// (owned, moved into the future); returns entries newest-first.
/// Higher-ranked over the `AppState` borrow so the returned future may borrow
/// from it.
pub type PlatformFetchFn = for<'a> fn(
    &'a AppState,
    String,
    Option<&'a std::collections::HashSet<String>>,
    Lang,
) -> Pin<Box<dyn Future<Output = anyhow::Result<crate::fetcher::FetchResult>> + Send + 'a>>;

/// Platform descriptor: identity (cookie panel) + routing (URL match,
/// canonicalization, fetch). Single source of truth for platform knowledge —
/// fetcher.rs and scheduler.rs route via [`platform_for_url`] instead of
/// maintaining their own if-else chains.
pub struct PlatformDef {
    /// Stable id used in settings keys (`cookie:<id>`) and callback data.
    pub id: &'static str,
    /// Display name shown in the bot UI.
    pub label: &'static str,
    /// Whether the platform needs a login cookie. Cookie-less platforms
    /// (e.g. Telegram web preview) are listed in the /cookies panel as
    /// read-only, without a configuration button.
    pub needs_cookie: bool,
    /// True when the URL belongs to this platform (host/path shape check).
    pub match_url: fn(&str) -> bool,
    /// Rewrite the URL to its canonical form for subscribe-time dedup
    /// (host/path variants collapse). Returns None on unparseable input.
    pub canonical_url: fn(&str) -> Option<String>,
    /// Native fetch entry point.
    pub fetch: PlatformFetchFn,
}

/// All platforms. Extend here — routing picks new entries up automatically.
pub const PLATFORMS: &[PlatformDef] = &[
    PlatformDef {
        id: "weibo",
        label: "Weibo",
        needs_cookie: true,
        match_url: weibo::is_weibo_url,
        canonical_url: |url| weibo::parse_uid(url).map(|uid| format!("https://weibo.com/u/{}", uid)),
        fetch: |state, url, known, lang| Box::pin(weibo::fetch_by_url(state, url, known, lang)),
    },
    PlatformDef {
        id: "x",
        label: "X",
        needs_cookie: true,
        match_url: x::is_x_url,
        // Lowercase the screen name for canonicalization: X usernames are
        // case-insensitive, so without this x.com/ElonMusk and x.com/elonmusk
        // canonicalize to different strings and bypass the UNIQUE(chat_id,
        // feed_url) dedup — the same account gets subscribed twice and pushed
        // twice. The API lookup and links are case-insensitive; the display
        // name comes from the API response, not this URL.
        canonical_url: |url| {
            x::parse_screen_name(url)
                .map(|name| format!("https://x.com/{}", name.to_ascii_lowercase()))
        },
        fetch: |state, url, known, lang| Box::pin(x::fetch_by_url(state, url, known, lang)),
    },
    PlatformDef {
        id: "telegram",
        label: "Telegram",
        needs_cookie: false,
        match_url: telegram::is_telegram_url,
        // Lowercase the channel name for canonicalization: t.me channel
        // usernames are case-insensitive, so t.me/Foo and t.me/foo must
        // canonicalize identically to prevent duplicate subscriptions.
        canonical_url: |url| {
            telegram::parse_channel_name(url)
                .map(|ch| format!("https://t.me/{}", ch.to_ascii_lowercase()))
        },
        fetch: |state, url, known, lang| Box::pin(telegram::fetch_by_url(state, url, known, lang)),
    },
];

pub fn find_platform(id: &str) -> Option<&'static PlatformDef> {
    PLATFORMS.iter().find(|p| p.id == id)
}

/// Route a feed URL to its platform (registry-driven; non-platform URLs
/// return None and fall through to the generic RSS pipeline).
pub fn platform_for_url(url: &str) -> Option<&'static PlatformDef> {
    PLATFORMS.iter().find(|p| (p.match_url)(url))
}

/// Sort entries newest-first by published (fallback updated) timestamp.
/// Shared by all platform fetchers and the RSS fetcher (#15 fix).
pub fn sort_entries_newest_first(entries: &mut [feed_rs::model::Entry]) {
    entries.sort_by(|a, b| {
        let ta = a.published.or(a.updated);
        let tb = b.published.or(b.updated);
        tb.cmp(&ta)
    });
}

/// Build a display title from plain text: first 50 chars + "…" if truncated.
/// Returns `fallback` when the result is empty/whitespace-only (#15 fix).
pub fn make_title(plain: &str, fallback: &str) -> String {
    let title: String = plain.chars().take(50).collect();
    let title = if plain.chars().count() > 50 {
        format!("{}…", title)
    } else {
        title
    };
    if title.trim().is_empty() {
        fallback.to_string()
    } else {
        title
    }
}

/// Settings key for the global platform cookie.
pub fn cookie_setting_key(platform: &str) -> String {
    crate::db::setting_key::cookie(platform)
}

/// Load all persisted cookies from the DB into the in-memory store at startup.
/// Scans settings keys matching "cookie:<platform>" (global, no user_id).
pub async fn load_persisted_cookies(state: &AppState) {
    let rows = match crate::db::get_settings_by_prefix(&state.db, "cookie:").await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "load_persisted_cookies: prefix query failed, cookies not loaded");
            return;
        }
    };
    for (key, value) in rows {
        // key format: "cookie:<platform>" (2 segments)
        let parts: Vec<&str> = key.splitn(2, ':').collect();
        if parts.len() == 2 {
            let platform = parts[1];
            // Skip legacy per-user keys (cookie:<user_id>:<platform>) — they
            // are cleaned up by the migration in main.rs.
            if platform.contains(':') {
                continue;
            }
            if !value.trim().is_empty() {
                state.set_cookie(platform, &value).await;
            }
        }
    }
}

/// Migrate legacy per-user cookies (cookie:<user_id>:<platform>) to the new
/// global format (cookie:<platform>). Prefers the admin's cookie when multiple
/// users had configured one. Deletes all legacy keys after migration.
/// Also cleans up legacy per-user health/streak/expired keys.
pub async fn migrate_legacy_cookies(state: &AppState) {
    let rows = match crate::db::get_settings_by_prefix(&state.db, "cookie:").await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "migrate_legacy_cookies: prefix query failed");
            return;
        }
    };

    // Collect legacy per-user cookies: (user_id, platform, value)
    let mut legacy: Vec<(i64, String, String)> = Vec::new();
    for (key, value) in &rows {
        let parts: Vec<&str> = key.splitn(3, ':').collect();
        if parts.len() == 3 {
            if let Ok(uid) = parts[1].parse::<i64>() {
                legacy.push((uid, parts[2].to_string(), value.clone()));
            }
        }
    }

    if legacy.is_empty() {
        // No legacy per-user cookies to migrate, but a DB could still hold
        // legacy per-user health/streak/expired keys (three-segment form). Fall
        // through to the unconditional cleanup below so those orphans are
        // removed too — do NOT early-return here.
        tracing::debug!("migrate_legacy_cookies: no legacy cookies, running orphan-key cleanup only");
    } else {
        tracing::info!(count = legacy.len(), "migrating legacy per-user cookies to global format");

        // Group by platform; prefer admin's cookie
        let mut best: std::collections::HashMap<String, (i64, String)> = std::collections::HashMap::new();
        for (uid, platform, value) in &legacy {
            let is_admin = state.config.is_admin(*uid);
            let entry = best.entry(platform.clone());
            match entry {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert((*uid, value.clone()));
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    // Replace if current best is non-admin and this one is admin
                    let cur_uid = e.get().0;
                    if is_admin && !state.config.is_admin(cur_uid) {
                        e.insert((*uid, value.clone()));
                    }
                }
            }
        }

        // Write global keys (only if not already set)
        for (platform, (_uid, value)) in &best {
            let global_key = cookie_setting_key(platform);
            let existing = crate::db::get_setting(&state.db, &global_key).await.ok().flatten();
            if existing.is_none() && !value.trim().is_empty() {
                crate::db::set_setting(&state.db, &global_key, value).await.ok();
                state.set_cookie(platform, value).await;
                tracing::info!(platform = %platform, "migrated cookie to global format");
            }
        }

        // Delete all legacy per-user cookie keys
        for (key, _) in &rows {
            let parts: Vec<&str> = key.splitn(3, ':').collect();
            if parts.len() == 3 {
                crate::db::delete_setting(&state.db, key).await.ok();
            }
        }
    }

    // Clean up legacy per-user health/streak/expired keys. Runs unconditionally
    // (even when there were no legacy cookies), so a DB that only has per-user
    // health keys still gets them removed instead of leaking forever.
    for prefix in ["platform_health:", "platform_streak:", "cookie_expired_notified:"] {
        if let Ok(hrows) = crate::db::get_settings_by_prefix(&state.db, prefix).await {
            for (key, _) in &hrows {
                let rest = &key[prefix.len()..];
                // Old format has user_id segment: "prefix:<user_id>:<platform>"
                if rest.contains(':') {
                    crate::db::delete_setting(&state.db, key).await.ok();
                }
            }
        }
    }
}

/// Update the global platform cookie: DB persistence first, then in-memory store.
/// (DB-first so a failed write doesn't leave the cookie live in memory while
/// the user is told "save failed".) Also clears the "cookie expired, user
/// notified" flag so the next expiry can trigger a fresh notification.
pub async fn update_cookie(state: &AppState, platform: &str, value: &str) -> anyhow::Result<()> {
    // Uniform trim: DB and memory share the same value, preventing the
    // inconsistency where a pasted cookie with trailing newline/spaces fails
    // requests with the untrimmed value in the current session, yet works
    // fine after restart when the trimmed value is loaded from the DB.
    let value = value.trim();
    crate::db::set_setting(&state.db, &cookie_setting_key(platform), value).await?;
    state.set_cookie(platform, value).await;
    // Clear the expiry-notification flag (set by the scheduler on failure).
    crate::db::set_setting(&state.db, &crate::db::setting_key::cookie_expired_notified(platform), "0").await.ok();
    Ok(())
}
