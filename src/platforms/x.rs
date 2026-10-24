//! X (Twitter) native fetcher: calls x.com's internal GraphQL API using a
//! login cookie (`auth_token`). Mirrors RSSHub's web-api approach.
//!
//! Cookie resolution: global runtime store (`state.cookies["x"]`)
//! — set via the /cookies command by admins, persisted in the `settings`
//! table under `cookie:x`. (Since v1.0.0 there is no env seed; storage is
//! purely DB-backed.)
//!
//! Subscription URL format: `https://x.com/<screen_name>` (twitter.com
//! variants also accepted).
//!
//! The fetched tweets are converted into feed-rs `Entry` objects with media
//! embedded as `<img>`/`<video>` tags in the content body, so the existing
//! media extraction / rendering pipeline works unchanged.

use std::collections::HashMap;
use std::sync::{LazyLock, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::{DateTime, Utc};
use feed_rs::model::Entry;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::i18n::{self, Lang};
use crate::state::AppState;
// Set-Cookie value extraction is a pure text helper; it lives in htmltext
// and is shared with media.rs (two drifting private copies previously).
use crate::htmltext::extract_cookie_value;

/// Public Bearer token used by x.com's web client (same for all users).
const BEARER: &str = "Bearer AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs%3D1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA";

const BASE_API: &str = "https://x.com/i/api";

/// GraphQL query IDs — hardcoded FALLBACK only. At runtime the fresh IDs
/// are auto-refreshed from the community-maintained TwitterInternalAPIDocument
/// (see `refresh_query_ids`); these constants are the last-resort fallback
/// when the document is unreachable.
/// Last synced from RSSHub / TwitterInternalAPIDocument 2026-09-28.
const QID_USER_BY_SCREEN_NAME: &str = "Gb-d6r0vxPOADdG62OEBpQ";
const QID_USER_TWEETS: &str = "eoJ5zbv51Z_KVl81v9PmLQ";

/// Features payload for UserByScreenName.
const FEATURES_USER: &str = r#"{"hidden_profile_subscriptions_enabled":true,"rweb_tipjar_consumption_enabled":true,"responsive_web_graphql_exclude_directive_enabled":true,"verified_phone_label_enabled":false,"subscriptions_verification_info_is_identity_verified_enabled":true,"subscriptions_verification_info_verified_since_enabled":true,"highlights_tweets_tab_ui_enabled":true,"responsive_web_twitter_article_notes_tab_enabled":true,"subscriptions_feature_can_gift_premium":true,"creator_subscriptions_tweet_preview_api_enabled":true,"responsive_web_graphql_skip_user_profile_image_extensions_enabled":false,"responsive_web_graphql_timeline_navigation_enabled":true}"#;

/// Features payload for UserTweets (timeline).
const FEATURES_FEED: &str = r#"{"rweb_tipjar_consumption_enabled":true,"responsive_web_graphql_exclude_directive_enabled":true,"verified_phone_label_enabled":false,"creator_subscriptions_tweet_preview_api_enabled":true,"responsive_web_graphql_timeline_navigation_enabled":true,"responsive_web_graphql_skip_user_profile_image_extensions_enabled":false,"communities_web_enable_tweet_community_results_fetch":true,"c9s_tweet_anatomy_moderator_badge_enabled":true,"articles_preview_enabled":true,"responsive_web_edit_tweet_api_enabled":true,"graphql_is_translatable_rweb_tweet_is_translatable_enabled":true,"view_counts_everywhere_api_enabled":true,"longform_notetweets_consumption_enabled":true,"responsive_web_twitter_article_tweet_consumption_enabled":true,"tweet_awards_web_tipping_enabled":false,"creator_subscriptions_quote_tweet_preview_enabled":false,"freedom_of_speech_not_reach_fetch_enabled":true,"standardized_nudges_misinfo":true,"tweet_with_visibility_results_prefer_gql_limited_actions_policy_enabled":true,"rweb_video_timestamps_enabled":true,"longform_notetweets_rich_text_read_enabled":true,"longform_notetweets_inline_media_enabled":true,"responsive_web_enhance_cards_enabled":false}"#;

/// Hard cap on timeline pagination pages per fetch round. Protects against
/// high-volume accounts (and against any cursor anomaly) blowing up one
/// round: 3 pages x count=20 ≈ up to 60 tweets per cycle. Already-sent
/// entries from older pages are still filtered downstream by the
/// scheduler's sent-entry dedup, so widening the window never re-pushes.
const MAX_TIMELINE_PAGES: usize = 3;

/// Pause between timeline pages: extra requests inside one round raise
/// rate-limit / risk-control exposure; the cadence matches the send-side
/// pacer (1100 ms).
const TIMELINE_PAGE_PAUSE: Duration = Duration::from_millis(1100);

// ---------------------------------------------------------------------------
// URL detection
// ---------------------------------------------------------------------------

/// True when the feed URL is an X/Twitter user profile that should be
/// fetched natively.
pub fn is_x_url(url: &str) -> bool {
    parse_screen_name(url).is_some()
}

/// Parse screen_name from an X user profile URL:
/// https://x.com/<name>, https://twitter.com/<name>, https://www.x.com/<name>
/// Excludes paths like /i/, /home, /search, /settings, etc.
pub fn parse_screen_name(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?.to_lowercase();
    let is_x_host = matches!(
        host.as_str(),
        "x.com" | "www.x.com" | "twitter.com" | "www.twitter.com" | "mobile.twitter.com"
    );
    if !is_x_host {
        return None;
    }
    let segs: Vec<&str> = u.path_segments()?.filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return None;
    }
    let name = segs[0];
    // Reject reserved paths.
    const RESERVED: &[&str] = &[
        "i", "home", "search", "explore", "notifications", "messages",
        "settings", "compose", "login", "signup", "logout", "hashtag",
    ];
    if RESERVED.contains(&name) {
        return None;
    }
    // Screen names: 1-15 alphanumeric/underscore chars.
    if name.is_empty() || name.len() > 15 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(name.to_string())
}

// ---------------------------------------------------------------------------
// Main fetch entry point
// ---------------------------------------------------------------------------

/// Registry fetch entry: parse the screen name from the full feed URL, then
/// delegate to [`fetch_x_user`]. Called via `PlatformDef.fetch` (URL passed
/// owned so the returned future borrows only from `state`).
pub async fn fetch_by_url(
    state: &AppState,
    url: String,
    known_sent: Option<&std::collections::HashSet<String>>,
    lang: Lang,
) -> anyhow::Result<crate::fetcher::FetchResult> {
    let screen_name = parse_screen_name(&url)
        .ok_or_else(|| anyhow::anyhow!("Invalid X user profile URL: {}", url))?;
    fetch_x_user(state, &screen_name, known_sent, lang).await
}

/// Fetch an X user's timeline and convert to FetchResult.
pub async fn fetch_x_user(
    state: &AppState,
    screen_name: &str,
    known_sent: Option<&std::collections::HashSet<String>>,
    lang: Lang,
) -> anyhow::Result<crate::fetcher::FetchResult> {
    let auth_token = match state.get_cookie("x").await {
        Some(c) if !c.trim().is_empty() => c,
        _ => anyhow::bail!("X feeds require an admin-configured login cookie (use the /cookies command)"),
    };

    // ct0 is cached (1h TTL): avoids every X feed hitting x.com once per fetch round.
    let mut ct0 = obtain_ct0_cached(state, &auth_token).await?;

    // The cached ct0 may have expired (403): clear the cache, re-fetch ct0 and
    // retry the whole flow once. Only a second 403 means the auth_token cookie
    // itself is dead.
    let result = fetch_x_user_with_ct0(state, &auth_token, &ct0, screen_name, known_sent, lang).await;
    let result = match result {
        Err(e) if e.downcast_ref::<StaleCt0Error>().is_some() => {
            tracing::info!(screen_name = screen_name, "X ct0 looks stale; refreshing and retrying once");
            clear_ct0_cache(&auth_token).await;
            ct0 = obtain_ct0(state, &auth_token).await?;
            {
                let mut cache = ct0_cache().lock().await;
                cache.insert(auth_token.clone(), Ct0Entry {
                    ct0: ct0.clone(),
                    fetched_at: Instant::now(),
                });
            }
            fetch_x_user_with_ct0(state, &auth_token, &ct0, screen_name, known_sent, lang).await
        }
        other => other,
    };
    result.map_err(|e| {
        if e.downcast_ref::<StaleCt0Error>().is_some() {
            crate::platforms::auth_expired("X", "HTTP 403 after a ct0 refresh")
        } else {
            e
        }
    })
}

async fn fetch_x_user_with_ct0(
    state: &AppState,
    auth_token: &str,
    ct0: &str,
    screen_name: &str,
    known_sent: Option<&std::collections::HashSet<String>>,
    lang: Lang,
) -> anyhow::Result<crate::fetcher::FetchResult> {
    // Step 2: resolve screen_name → rest_id + display name.
    let (rest_id, display_name) = resolve_user(state, auth_token, ct0, screen_name).await?;

    // Step 3: fetch user timeline.
    let tweets = fetch_user_tweets(state, auth_token, ct0, &rest_id, screen_name, known_sent).await?;

    // Step 4: convert to entries (dedup by entry id within the batch, matching
    // the Weibo behavior; fix for #8).
    let mut entries: Vec<Entry> = Vec::new();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Unexpected-conversion-failure counter (summarized in a warn at batch
    // end; ensures an API redesign that fails the whole batch doesn't leave
    // zero traces, aligned with the None-warn convention of Weibo's
    // mblog_to_entry). Replies are an EXPECTED skip and are counted
    // separately, so a normal day full of replies stops polluting the dropped
    // signal that is meant to catch genuine structural breakage.
    let mut dropped = 0usize;
    let mut skipped_replies = 0usize;
    for tweet in &tweets {
        match tweet_to_entry(tweet, screen_name, &display_name, lang) {
            ConvertOutcome::Ok(e) => {
                if !e.id.is_empty() && seen_ids.insert(e.id.clone()) {
                    entries.push(*e);
                }
            }
            ConvertOutcome::SkippedReply => skipped_replies += 1,
            ConvertOutcome::Failed => dropped += 1,
        }
    }
    if skipped_replies > 0 {
        tracing::debug!(
            screen_name = screen_name,
            skipped = skipped_replies,
            total = tweets.len(),
            "X replies skipped during conversion (expected)"
        );
    }
    if dropped > 0 {
        tracing::warn!(
            screen_name = screen_name,
            dropped = dropped,
            total = tweets.len(),
            "X tweets dropped during conversion (legacy missing?)"
        );
    }

    // Sort newest first.
    crate::platforms::sort_entries_newest_first(&mut entries);

    Ok(crate::fetcher::FetchResult::platform(Some(display_name), entries))
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

/// Visit x.com with the auth_token cookie to obtain ct0 (CSRF token).
async fn obtain_ct0(state: &AppState, auth_token: &str) -> anyhow::Result<String> {
    // Use a client with auto-redirect disabled: when the cookie is invalid
    // x.com 302-redirects to the login page, and a redirect-following client
    // ends up with the final page so is_redirection() never fires
    // (same lesson as Weibo's getIndex; see the comment on the
    // fetch_client_no_redirect_for_url call inside fetch_weibo_user in weibo.rs).
    let client = state.media_client_for_url("https://x.com");
    let resp = client
        .get("https://x.com")
        .header("Cookie", format!("auth_token={}", auth_token))
        .send()
        .await
        .context("x.com request failed")?;

    // Extract ct0 from Set-Cookie headers.
    let mut ct0 = String::new();
    for value in resp.headers().get_all("set-cookie") {
        if let Ok(s) = value.to_str() {
            if let Some(token) = extract_cookie_value(s, "ct0") {
                ct0 = token;
                break;
            }
        }
    }
    if ct0.is_empty() {
        // No ct0 in the response. Only redirects/401/login pages mean the
        // cookie is dead; 429/5xx/Cloudflare risk-control pages are transient
        // and must NOT be reported as an expired cookie (the scheduler used
        // to key cookie-expiry detection on the substring "cookie", so every
        // such response wrongly notified the admin and counted toward
        // auto-disable).
        let status = resp.status();
        if status.is_redirection()
            || status == reqwest::StatusCode::UNAUTHORIZED
            || resp.url().path().contains("/i/flow/login")
        {
            return Err(crate::platforms::auth_expired(
                "X",
                format!("ct0 missing (HTTP {})", status),
            ));
        }
        if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(crate::platforms::rate_limited("X", status.as_u16()));
        }
        anyhow::bail!("X ct0 unavailable (HTTP {}); response shape may have changed", status);
    }
    Ok(ct0)
}

/// The ct0 lifetime is measured in hours; cache for 1 hour to avoid every X
/// feed hitting x.com once per fetch round (on a serial channel each extra
/// request adds extra risk-control exposure). After a cookie update the
/// auth_token changes and the cache key no longer matches, so the entry
/// expires automatically.
const CT0_TTL: Duration = Duration::from_secs(3600);

struct Ct0Entry {
    ct0: String,
    fetched_at: Instant,
}

static CT0_CACHE: OnceLock<Mutex<std::collections::HashMap<String, Ct0Entry>>> = OnceLock::new();

fn ct0_cache() -> &'static Mutex<std::collections::HashMap<String, Ct0Entry>> {
    CT0_CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Cached ct0 lookup: returns the cached token when it matches the current
/// auth_token and is within TTL; otherwise fetches fresh and caches it.
/// The HashMap is keyed by auth_token, so multiple users stay independent
/// and never evict each other's entries.
async fn obtain_ct0_cached(state: &AppState, auth_token: &str) -> anyhow::Result<String> {
    {
        let cache = ct0_cache().lock().await;
        if let Some(entry) = cache.get(auth_token) {
            if entry.fetched_at.elapsed() < CT0_TTL {
                return Ok(entry.ct0.clone());
            }
        }
    }
    let ct0 = obtain_ct0(state, auth_token).await?;
    let mut cache = ct0_cache().lock().await;
    // Opportunistically prune expired entries: after a user switches cookies,
    // old auth_token keys would linger forever and grow unboundedly with each
    // cookie change (~100 bytes per entry — slow growth, but worth preventing).
    cache.retain(|_, entry| entry.fetched_at.elapsed() < CT0_TTL);
    cache.insert(auth_token.to_string(), Ct0Entry {
        ct0: ct0.clone(),
        fetched_at: Instant::now(),
    });
    Ok(ct0)
}

/// Invalidate the cached ct0 for a specific auth_token (called when a request signals a stale token).
async fn clear_ct0_cache(auth_token: &str) {
    let mut cache = ct0_cache().lock().await;
    cache.remove(auth_token);
}

// ---------------------------------------------------------------------------
// GraphQL query-ID auto-refresh
// ---------------------------------------------------------------------------

/// Community-maintained document tracking the current x.com GraphQL query IDs.
/// Refreshed whenever X ships a new frontend; RSSHub uses the same source.
const QUERY_ID_DOC_URL: &str =
    "https://cdn.jsdelivr.net/gh/fa0311/TwitterInternalAPIDocument@master/docs/json/API.json";

/// Cached query IDs stay fresh this long before we re-fetch the document.
const QUERY_ID_TTL: Duration = Duration::from_secs(6 * 3600);
/// After a failed refresh, keep using cached/fallback IDs for this long before
/// retrying the document. Without it, a jsdelivr outage made every GraphQL
/// call (up to 4 per feed per round) retry the document request and burn its
/// 30s client timeout on the serial platform channel.
const QUERY_ID_NEGATIVE_TTL: Duration = Duration::from_secs(300);

const OP_USER_BY_SCREEN_NAME: &str = "UserByScreenName";
const OP_USER_TWEETS: &str = "UserTweets";

struct QueryIdCache {
    ids: HashMap<String, String>,
    fetched_at: Option<Instant>,
    last_failure: Option<Instant>,
}

static QUERY_IDS: OnceLock<Mutex<QueryIdCache>> = OnceLock::new();

fn query_id_cache() -> &'static Mutex<QueryIdCache> {
    QUERY_IDS.get_or_init(|| {
        Mutex::new(QueryIdCache {
            ids: HashMap::new(),
            fetched_at: None,
            last_failure: None,
        })
    })
}

/// Hardcoded fallback queryId for an operation (used when the document is
/// unreachable and nothing has been cached yet).
fn fallback_query_id(op: &str) -> &'static str {
    match op {
        OP_USER_BY_SCREEN_NAME => QID_USER_BY_SCREEN_NAME,
        OP_USER_TWEETS => QID_USER_TWEETS,
        _ => "",
    }
}

/// Resolve the queryId for an operation. Prefers freshly-fetched IDs (cached
/// for [`QUERY_ID_TTL`]); on a cache miss / expiry fetches the document; on
/// fetch failure falls back to any stale cached ID, then to the constant.
/// `force` bypasses the TTL (used after a suspected stale-ID 404).
async fn get_query_id(state: &AppState, op: &str, force: bool) -> String {
    // Fast path: a fresh cached ID, or a recent refresh failure (negative
    // cache) — retrying the document on every GraphQL call would amplify an
    // outage into multi-second stalls on the serial platform channel.
    {
        let cache = query_id_cache().lock().await;
        if !force {
            if cache.last_failure.is_some_and(|t| t.elapsed() < QUERY_ID_NEGATIVE_TTL) {
                return cache
                    .ids
                    .get(op)
                    .cloned()
                    .unwrap_or_else(|| fallback_query_id(op).to_string());
            }
            if let Some(at) = cache.fetched_at {
                if at.elapsed() < QUERY_ID_TTL {
                    if let Some(id) = cache.ids.get(op) {
                        return id.clone();
                    }
                }
            }
        }
    }

    // Slow path: refresh from the document.
    match fetch_query_ids(state).await {
        Ok(ids) => {
            let mut cache = query_id_cache().lock().await;
            let resolved = ids.get(op).cloned();
            cache.ids = ids;
            cache.fetched_at = Some(Instant::now());
            cache.last_failure = None;
            resolved.unwrap_or_else(|| fallback_query_id(op).to_string())
        }
        Err(e) => {
            // `{:#}` renders the full anyhow source chain (reqwest root cause
            // under the "query-ID document request failed" context), so a
            // proxy/TLS failure here is distinguishable from a parse failure.
            tracing::warn!(error = format!("{:#}", e), op = op, "X query-ID refresh failed; using cached/fallback ID");
            let mut cache = query_id_cache().lock().await;
            cache.last_failure = Some(Instant::now());
            cache
                .ids
                .get(op)
                .cloned()
                .unwrap_or_else(|| fallback_query_id(op).to_string())
        }
    }
}

/// Fetch and parse the current query IDs from the community document.
async fn fetch_query_ids(state: &AppState) -> anyhow::Result<HashMap<String, String>> {
    let client = state.client_for_url(QUERY_ID_DOC_URL);
    let resp = client
        .get(QUERY_ID_DOC_URL)
        .header("Accept", "application/json")
        .send()
        .await
        .context("query-ID document request failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("query-ID document HTTP {}", resp.status());
    }
    let json: Value = crate::fetcher::read_json_capped(resp, "query-ID document").await?;

    let mut ids = HashMap::new();
    for op in [OP_USER_BY_SCREEN_NAME, OP_USER_TWEETS] {
        if let Some(qid) = json["graphql"][op]["queryId"].as_str() {
            // Supply-chain defense: the queryId is spliced into the URL path,
            // so validate its character set to prevent injection.
            let valid = !qid.is_empty()
                && qid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if valid {
                ids.insert(op.to_string(), qid.to_string());
            }
        }
    }
    if ids.is_empty() {
        anyhow::bail!("query-ID document contained no usable queryIds");
    }
    Ok(ids)
}

/// Marker error: the endpoint rejected the request in a way that suggests the
/// queryId is stale (X shipped a new frontend). Triggers one forced refresh.
#[derive(Debug)]
struct StaleQueryIdError;

impl std::fmt::Display for StaleQueryIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "X GraphQL queryId appears stale")
    }
}
impl std::error::Error for StaleQueryIdError {}

/// Marker: a GraphQL request got HTTP 403, most likely the cached ct0 (CSRF
/// token) expired. The caller clears the ct0 cache, re-fetches and retries
/// once; a second 403 means the auth_token cookie itself is dead.
#[derive(Debug)]
struct StaleCt0Error;

impl std::fmt::Display for StaleCt0Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "X GraphQL request forbidden (ct0 likely stale)")
    }
}
impl std::error::Error for StaleCt0Error {}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

/// Perform a GraphQL API call, resolving the queryId automatically. If the
/// first attempt signals a stale queryId (HTTP 404), force-refresh the IDs
/// and retry exactly once.
async fn gql_request(
    state: &AppState,
    auth_token: &str,
    ct0: &str,
    op: &str,
    params: &[(&str, String)],
) -> anyhow::Result<Value> {
    match gql_request_once(state, auth_token, ct0, op, params, false).await {
        Ok(v) => Ok(v),
        Err(e) if e.downcast_ref::<StaleQueryIdError>().is_some() => {
            tracing::info!(op = op, "X queryId looks stale; force-refreshing and retrying once");
            gql_request_once(state, auth_token, ct0, op, params, true).await
        }
        Err(e) => Err(e),
    }
}

async fn gql_request_once(
    state: &AppState,
    auth_token: &str,
    ct0: &str,
    op: &str,
    params: &[(&str, String)],
    force_refresh: bool,
) -> anyhow::Result<Value> {
    let qid = get_query_id(state, op, force_refresh).await;
    if qid.is_empty() {
        anyhow::bail!("no queryId available for X operation {}", op);
    }
    let path = format!("/graphql/{}/{}", qid, op);

    // Build URL with query params manually (reqwest `query` feature not enabled).
    let mut url = format!("{}{}", BASE_API, path);
    if !params.is_empty() {
        url.push('?');
        let pairs: Vec<String> = params
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    k,
                    url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>()
                )
            })
            .collect();
        url.push_str(&pairs.join("&"));
    }

    let client = state.client_for_url(&url);
    let resp = client
        .get(&url)
        .header("Cookie", format!("auth_token={}; ct0={}", auth_token, ct0))
        .header("Authorization", BEARER)
        .header("x-csrf-token", ct0)
        .header("x-twitter-auth-type", "OAuth2Session")
        .header("x-twitter-active-user", "yes")
        .header("x-twitter-client-language", "en")
        .header("Content-Type", "application/json")
        .header("Referer", "https://x.com/")
        .send()
        .await
        .with_context(|| format!("X GraphQL request failed: {}", path))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        // 401 = the auth_token itself is invalid; no point retrying.
        return Err(crate::platforms::auth_expired("X", "HTTP 401"));
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        // 403 may be an expired cached ct0 (rather than a dead cookie): throw
        // the marker so the caller clears the cache, re-fetches ct0 and retries
        // once; only a second 403 means the cookie is dead.
        return Err(anyhow::Error::new(StaleCt0Error));
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(crate::platforms::rate_limited("X", status.as_u16()));
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        // A 404 on a GraphQL path almost always means the queryId no longer
        // exists (X rotated it). Surface a marker so the caller retries once
        // after a forced refresh.
        return Err(anyhow::Error::new(StaleQueryIdError));
    }
    if !status.is_success() {
        anyhow::bail!("X API HTTP {}", status);
    }

    let json: Value = crate::fetcher::read_json_capped(resp, "X API").await?;

    // GraphQL errors can come with HTTP 200.
    if let Some(errors) = json.get("errors").filter(|e| e.is_array()) {
        if let Some(msg) = errors.as_array().and_then(|a| a.first()) {
            let message = msg["message"].as_str().unwrap_or("unknown");
            anyhow::bail!("X GraphQL error: {}", message);
        }
    }
    Ok(json)
}

// ---------------------------------------------------------------------------
// GraphQL operations
// ---------------------------------------------------------------------------

/// Resolve screen_name → (rest_id, display_name).
async fn resolve_user(
    state: &AppState,
    auth_token: &str,
    ct0: &str,
    screen_name: &str,
) -> anyhow::Result<(String, String)> {
    let variables = serde_json::json!({
        "screen_name": screen_name,
        "withSafetyModeUserFields": true
    });
    let field_toggles = serde_json::json!({"withAuxiliaryUserLabels": false});

    let json = gql_request(state, auth_token, ct0, OP_USER_BY_SCREEN_NAME, &[
        ("variables", variables.to_string()),
        ("features", FEATURES_USER.to_string()),
        ("fieldToggles", field_toggles.to_string()),
    ]).await?;

    let result = &json["data"]["user"]["result"];
    if result.is_null() {
        anyhow::bail!("X user does not exist: @{}", screen_name);
    }
    if result["__typename"].as_str() == Some("UserUnavailable") {
        anyhow::bail!("X user unavailable: @{}", screen_name);
    }
    let rest_id = result["rest_id"].as_str()
        .ok_or_else(|| anyhow::anyhow!("X API did not return rest_id"))?
        .to_string();
    let display_name = result["legacy"]["name"].as_str()
        .or_else(|| result["core"]["name"].as_str())
        .unwrap_or(screen_name)
        .to_string();

    Ok((rest_id, display_name))
}

/// Fetch the user's tweet timeline.
/// `screen_name` is only used to pinpoint the zero-tweet warning (to tell
/// which account it is when multiple X feeds are subscribed).
async fn fetch_user_tweets(
    state: &AppState,
    auth_token: &str,
    ct0: &str,
    rest_id: &str,
    screen_name: &str,
    known_sent: Option<&std::collections::HashSet<String>>,
) -> anyhow::Result<Vec<Value>> {
    let mut tweets: Vec<Value> = Vec::new();
    // Accumulated across pages: distinguishes a "truly empty timeline" from
    // "no tweets extracted due to a structural redesign" (see the warn below).
    let mut total_non_cursor_entries = 0usize;
    // Instruction types from the first page, kept for the shape-change warn.
    let mut first_page_instr_types: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    // Pages actually fetched this round (drives the inter-page pause so the
    // first page — and a fully cached short timeline — stays request-cheap).
    let mut pages_fetched = 0usize;

    for _page in 0..MAX_TIMELINE_PAGES {
        if pages_fetched > 0 {
            tokio::time::sleep(TIMELINE_PAGE_PAUSE).await;
        }
        let mut variables = serde_json::json!({
            "userId": rest_id,
            "count": 20,
            "includePromotedContent": false,
            "withQuickPromoteEligibilityTweetFields": true,
            "withVoice": true,
            "withV2Timeline": true
        });
        if let Some(c) = &cursor {
            variables["cursor"] = Value::String(c.clone());
        }

        let json = gql_request(state, auth_token, ct0, OP_USER_TWEETS, &[
            ("variables", variables.to_string()),
            ("features", FEATURES_FEED.to_string()),
        ]).await?;
        pages_fetched += 1;

        // Navigate: data.user.result.timeline.timeline.instructions
        let mut instructions = &json["data"]["user"]["result"]["timeline"]["timeline"]["instructions"];
        if !instructions.is_array() {
            // Try alternate path (timeline_v2)
            let alt = &json["data"]["user"]["result"]["timeline_v2"]["timeline"]["instructions"];
            if !alt.is_array() {
                // Neither path yielded instructions: the API shape changed or the
                // account is abnormal. Must NOT return Ok as an empty timeline —
                // otherwise the scheduler treats it as success, resets the error
                // count, and the X feed silently stops updating with no warning at
                // all. Error out so the feed-level error count / retirement notice
                // kicks in. Bail mid-pagination too: error propagation is
                // unchanged (cookie / stale-ct0 marker / rate-limit texts kept).
                anyhow::bail!(
                    "X UserTweets response missing timeline instructions (API shape may have changed)"
                );
            }
            instructions = alt;
        }
        if pages_fetched == 1 {
            first_page_instr_types = instructions
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|i| i["type"].as_str().map(|t| t.to_string()))
                        .collect()
                })
                .unwrap_or_default();
        }

        let page = extract_timeline_page(instructions);
        total_non_cursor_entries += page.non_cursor_entries;
        let page_tweet_count = page.tweets.len();
        // If every tweet on this page is already sent, older pages cannot
        // contain unseen newer tweets; stop paginating to save requests
        // (2 extra pages × 1.1s per X feed per round in the steady state).
        // Pinned tweets are included in the check, so a pinned old tweet with
        // unseen newer tweets on the same page still continues.
        let page_all_known = known_sent.is_some_and(|known| {
            !page.tweets.is_empty()
                && page.tweets.iter().all(|t| {
                    tweet_id_of(t).is_some_and(|id| known.contains(id))
                })
        });
        tweets.extend(page.tweets);

        // Stop conditions (none can loop forever): no bottom cursor, a cursor
        // identical to the one just used (X sometimes repeats it on an
        // exhausted timeline), or a page that yielded no tweets at all
        // (further pages cannot recover from an empty/redesigned page).
        // Pages with older already-sent tweets are still safe: the scheduler
        // filters them via the sent-entry table (widening the window never
        // re-pushes).
        let Some(next) = page.bottom_cursor else { break };
        if page_tweet_count == 0 || Some(&next) == cursor.as_ref() {
            break;
        }
        if page_all_known {
            break;
        }
        cursor = Some(next);
    }

    // instructions existed and non-cursor entries exist, yet not a single
    // tweet was extracted across all pages: most likely an entry-structure
    // redesign (e.g. TweetTombstone wrapping). Must not silently return
    // empty — the scheduler would treat it as success, reset the error
    // count, and the X feed would silently stop updating with zero logs
    // (the failure mode v1.4.3 set out to eliminate, recurring one layer
    // deeper). Leave a warn to aid debugging; a truly empty timeline
    // (cursors only) does not warn.
    if tweets.is_empty() && total_non_cursor_entries > 0 {
        tracing::warn!(
            screen_name = screen_name,
            non_cursor_entries = total_non_cursor_entries,
            instruction_types = ?first_page_instr_types,
            "X timeline has entries but zero tweets extracted (API shape may have changed)"
        );
    }
    Ok(tweets)
}

/// One parsed timeline page: tweet result objects, the count of non-cursor
/// entries, and the bottom pagination cursor (if any).
/// `tweets` / `non_cursor_entries` / `bottom_cursor` semantics are identical
/// to the pre-pagination single-page extractor.
struct TimelinePage {
    tweets: Vec<Value>,
    non_cursor_entries: usize,
    bottom_cursor: Option<String>,
}

/// Extract tweet objects and the pagination cursor from GraphQL timeline
/// instructions (one page).
fn extract_timeline_page(instructions: &Value) -> TimelinePage {
    let mut tweets = Vec::new();
    let mut non_cursor_entries = 0usize;
    let mut bottom_cursor: Option<String> = None;
    let empty_instructions = Vec::new();
    for instr in instructions.as_array().unwrap_or(&empty_instructions) {
        let instr_type = instr["type"].as_str().unwrap_or("");
        // Dedicated pagination instruction: carries only a cursor entry
        // (entryId `cursor-bottom-…`), never tweets. Read the bottom token
        // from it, contributing neither tweets nor the non-cursor count.
        if instr_type == "TimelineSuggestCursor" {
            if let Some(entry) = instr.get("entry") {
                let entry_id = entry["entryId"].as_str().unwrap_or("");
                if bottom_cursor.is_none() {
                    if let Some(v) = cursor_entry_value(entry_id, entry) {
                        bottom_cursor = Some(v);
                    }
                }
            }
            continue;
        }
        let entries: Option<Vec<&Value>> = if instr_type == "TimelineAddEntries" {
            instr["entries"].as_array().map(|a| a.iter().collect())
        } else if instr_type == "TimelineAddEntry" {
            instr.get("entry").map(|e| vec![e])
        } else {
            None
        };
        if let Some(entries) = entries {
            for entry in entries {
                let entry_id = entry["entryId"].as_str().unwrap_or("");
                if entry_id.starts_with("cursor-") {
                    // Cursor entry: not a tweet, but the bottom one carries
                    // the token needed to request the next page.
                    if bottom_cursor.is_none() {
                        if let Some(v) = cursor_entry_value(entry_id, entry) {
                            bottom_cursor = Some(v);
                        }
                    }
                } else {
                    non_cursor_entries += 1;
                    collect_tweet_results(entry, &mut tweets);
                }
            }
        }
    }
    TimelinePage {
        tweets,
        non_cursor_entries,
        bottom_cursor,
    }
}

/// Bottom pagination cursor token from a timeline cursor entry.
/// Two shapes seen in the wild: the real opaque token in `content.value`
/// (with `content.cursorType == "Bottom"`), or the token embedded in the
/// entryId itself (`cursor-bottom-<value>`, e.g. TimelineSuggestCursor).
/// Neither present → None (fall back to single-page mode rather than
/// sending a bogus cursor).
fn cursor_entry_value(entry_id: &str, entry: &Value) -> Option<String> {
    let is_bottom = entry["content"]["cursorType"].as_str() == Some("Bottom")
        || entry_id.starts_with("cursor-bottom-");
    if !is_bottom {
        return None;
    }
    if let Some(v) = entry["content"]["value"].as_str() {
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    // entryId shape: cursor-bottom-<value>
    let suffix = entry_id.strip_prefix("cursor-bottom-")?;
    if suffix.is_empty() {
        return None;
    }
    Some(suffix.to_string())
}

/// Unwrap a `{__typename:"TweetWithVisibilityResults", tweet:{...}}` wrapper to
/// the inner tweet object; return the input unchanged when not wrapped.
/// X returns this wrapper for some tweets (e.g. visibility-restricted results);
/// its top level carries no `rest_id`/`legacy` — those live on the inner `tweet`.
fn unwrap_tweet_result(result: &Value) -> &Value {
    if result["__typename"].as_str() == Some("TweetWithVisibilityResults") {
        &result["tweet"]
    } else {
        result
    }
}

/// Recursively collect tweet result objects from timeline entries.
fn collect_tweet_results(entry: &Value, out: &mut Vec<Value>) {
    let entry_id = entry["entryId"].as_str().unwrap_or("");
    // Skip cursor entries (pagination markers).
    if entry_id.starts_with("cursor-") {
        return;
    }
    // Direct tweet entry: content.itemContent.tweet_results.result. The result
    // may be a TweetWithVisibilityResults wrapper whose rest_id is on the inner
    // tweet — unwrap before the rest_id check, otherwise such tweets are
    // silently dropped at the collection stage (never reaching tweet_to_entry,
    // so no warn and no dropped-count).
    let tweet_result = unwrap_tweet_result(&entry["content"]["itemContent"]["tweet_results"]["result"]);
    if !tweet_result.is_null() && tweet_result.get("rest_id").is_some() {
        out.push(tweet_result.clone());
        return;
    }
    // Nested items (e.g. conversation threads in profile-conversation modules)
    if let Some(items) = entry["content"]["items"].as_array() {
        for item in items {
            let tr = unwrap_tweet_result(&item["item"]["itemContent"]["tweet_results"]["result"]);
            if !tr.is_null() && tr.get("rest_id").is_some() {
                out.push(tr.clone());
            }
        }
    }
}

/// X sizes media through the `name` query parameter. Video preview frames are
/// re-encoded JPEGs with no `orig` variant (requesting it 404s), so ask for
/// `large` — the biggest one that always exists — instead of the `small`
/// default the timeline returns.
fn upgrade_x_cover(url: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, q),
        None => (url, ""),
    };
    let mut parts: Vec<String> = Vec::new();
    let mut has_name = false;
    for p in query.split('&').filter(|p| !p.is_empty()) {
        match p.strip_prefix("name=") {
            Some(_v) => {
                // Replace whatever size the response requested with large
                // (small = timeline default, orig = photo-only and 404s for
                // re-encoded video covers).
                has_name = true;
                parts.push("name=large".to_string());
            }
            None => parts.push(p.to_string()),
        }
    }
    if !has_name {
        parts.push("name=large".to_string());
    }
    format!("{base}?{}", parts.join("&"))
}

/// Append `<img>`/`<video>` tags for a tweet's media to the HTML body.
/// Photos get ?name=orig (highest resolution); videos pick the highest-bitrate
/// mp4 variant and carry their cover frame as the poster attribute. Reads
/// extended_entities.media, falling back to entities.media.
fn append_media_tags(body: &mut String, legacy: &Value) {
    let Some(media_arr) = legacy["extended_entities"]["media"]
        .as_array()
        .or_else(|| legacy["entities"]["media"].as_array())
    else {
        return;
    };
    for m in media_arr {
        match m["type"].as_str().unwrap_or("") {
            "photo" => {
                if let Some(url) = m["media_url_https"].as_str() {
                    // Append ?name=orig to get the highest resolution version.
                    let orig = if url.contains('?') {
                        format!("{}&name=orig", url)
                    } else {
                        format!("{}?name=orig", url)
                    };
                    body.push_str(&format!("<img src=\"{}\">", crate::push::escape_attr(&orig)));
                }
            }
            "video" | "animated_gif" => {
                // Pick highest bitrate variant.
                let variants = m["video_info"]["variants"].as_array();
                let best = variants.and_then(|vs| {
                    vs.iter()
                        .filter(|v| v["content_type"].as_str() == Some("video/mp4"))
                        .max_by_key(|v| v["bitrate"].as_i64().unwrap_or(0))
                });
                if let Some(v) = best {
                    if let Some(url) = v["url"].as_str() {
                        // preview_image_url is the video cover: emit it as the
                        // poster attribute so the oversized/failed-video cover
                        // fallback in media.rs has a URL to send. The field is
                        // an object with a url in some response shapes and a
                        // bare string in others, so accept both (same
                        // dual-shape handling as weibo page_pic). Timeline
                        // responses can omit it entirely (observed for videos
                        // inside retweets); media_url_https then carries the
                        // same preview frame, so fall back to it.
                        let poster = m["preview_image_url"]["url"]
                            .as_str()
                            .or_else(|| m["preview_image_url"].as_str())
                            .or_else(|| m["media_url_https"].as_str())
                            .filter(|s| !s.is_empty())
                            .map(upgrade_x_cover);
                        match poster {
                            Some(p) => body.push_str(&format!(
                                "<video src=\"{}\" poster=\"{}\"></video>",
                                crate::push::escape_attr(url),
                                crate::push::escape_attr(&p)
                            )),
                            None => body.push_str(&format!(
                                "<video src=\"{}\"></video>",
                                crate::push::escape_attr(url)
                            )),
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Tweet → Entry conversion
// ---------------------------------------------------------------------------

/// Extract @mention screen names from a tweet's entities.
/// Checks both legacy.entities.user_mentions and note_tweet entity_set.mentions.
fn extract_mention_names(result: &Value) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    // legacy.entities.user_mentions
    if let Some(arr) = result["legacy"]["entities"]["user_mentions"].as_array() {
        for m in arr {
            if let Some(s) = m["screen_name"].as_str() {
                if !s.is_empty() {
                    names.push(s.to_string());
                }
            }
        }
    }
    // note_tweet entity_set.mentions (may have additional mentions)
    if let Some(arr) = result["note_tweet"]["note_tweet_results"]["result"]["entity_set"]["mentions"].as_array() {
        for m in arr {
            if let Some(s) = m["screen_name"].as_str() {
                if !s.is_empty() && !names.iter().any(|n| n.eq_ignore_ascii_case(s)) {
                    names.push(s.to_string());
                }
            }
        }
    }
    names
}

/// Wrap `@username` occurrences in HTML with links to their X profile.
/// Only links usernames present in the `names` list (from API entities).
/// Skips content already inside `<a>` tags (e.g. from URL auto-linking).
fn linkify_mentions(html: &str, names: &[String]) -> String {
    if names.is_empty() {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len() + names.len() * 60);
    let mut rest = html;
    loop {
        match rest.find("<a ") {
            Some(ap) => {
                // Linkify the plain-text segment before this anchor.
                out.push_str(&linkify_text_segment(&rest[..ap], names));
                // Copy the entire anchor (up to and including </a>) verbatim.
                match rest[ap..].find("</a>") {
                    Some(cp) => {
                        let anchor_end = ap + cp + 4;
                        out.push_str(&rest[ap..anchor_end]);
                        rest = &rest[anchor_end..];
                    }
                    None => {
                        // Malformed anchor; treat the rest as plain text.
                        out.push_str(&linkify_text_segment(&rest[ap..], names));
                        break;
                    }
                }
            }
            None => {
                out.push_str(&linkify_text_segment(rest, names));
                break;
            }
        }
    }
    out
}

/// Linkify @mentions within a plain (anchor-free) HTML text segment.
/// Collects all valid mention matches in one pass to avoid re-scanning
/// already-inserted anchors.
fn linkify_text_segment(text: &str, names: &[String]) -> String {
    let lower = text.to_ascii_lowercase();
    let mut matches: Vec<(usize, usize, &str)> = Vec::new();
    for name in names {
        let needle = format!("@{}", name).to_ascii_lowercase();
        let mut search_from = 0usize;
        while let Some(idx) = lower[search_from..].find(&needle) {
            let abs = search_from + idx;
            let end = abs + needle.len();
            // Boundary: char before @ must not be alnum/underscore.
            let ok_before = abs == 0 || {
                let prev = text.as_bytes()[abs - 1];
                !(prev.is_ascii_alphanumeric() || prev == b'_')
            };
            // Boundary: char after the name must not be alnum/underscore.
            let ok_after = end >= text.len() || {
                let next = text.as_bytes()[end];
                !(next.is_ascii_alphanumeric() || next == b'_')
            };
            if ok_before && ok_after {
                matches.push((abs, end, name.as_str()));
            }
            search_from = abs + 1;
        }
    }
    if matches.is_empty() {
        return text.to_string();
    }
    // Sort by position; skip overlaps (keep earliest).
    matches.sort_by_key(|&(s, _, _)| s);
    let mut out = String::with_capacity(text.len() + matches.len() * 60);
    let mut last_end = 0usize;
    for (start, end, name) in &matches {
        if *start < last_end {
            continue;
        }
        out.push_str(&text[last_end..*start]);
        out.push_str(&format!(
            "<a href=\"https://x.com/{}\">{}</a>",
            crate::push::escape_attr(name),
            crate::push::escape_html(&text[*start..*end])
        ));
        last_end = *end;
    }
    out.push_str(&text[last_end..]);
    out
}

/// Linkify #hashtags and $cashtags in Telegram HTML, skipping existing anchors.
/// #tag → https://x.com/hashtag/tag ; $TICKER → https://x.com/search?q=%24TICKER
fn linkify_hashtags(html: &str) -> String {
    let mut out = String::with_capacity(html.len() + 128);
    let mut rest = html;
    loop {
        match rest.find("<a ") {
            Some(ap) => {
                out.push_str(&linkify_hashtags_in_text(&rest[..ap]));
                match rest[ap..].find("</a>") {
                    Some(cp) => {
                        let anchor_end = ap + cp + 4;
                        out.push_str(&rest[ap..anchor_end]);
                        rest = &rest[anchor_end..];
                    }
                    None => {
                        out.push_str(&linkify_hashtags_in_text(&rest[ap..]));
                        break;
                    }
                }
            }
            None => {
                out.push_str(&linkify_hashtags_in_text(rest));
                break;
            }
        }
    }
    out
}

/// Find #hashtag and $cashtag patterns in a plain-text segment and wrap them
/// as X platform hyperlinks. Uses the same positional-scan approach as
/// linkify_text_segment to avoid re-scanning inserted anchors.
fn linkify_hashtags_in_text(text: &str) -> String {
    // Collect matches: (start, end, is_cashtag)
    let mut matches: Vec<(usize, usize, bool)> = Vec::new();
    let bytes = text.as_bytes();

    for (i, &b) in bytes.iter().enumerate() {
        if b != b'#' && b != b'$' {
            continue;
        }
        // Boundary: char before must not be alnum/underscore.
        if i > 0 {
            let prev = bytes[i - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                continue;
            }
        }
        let is_cashtag = b == b'$';
        // Scan the tag body: alphanumeric + underscore (ASCII sufficient for
        // X hashtags/cashtags in practice; unicode chars terminate the tag).
        let mut end = i + 1;
        while end < bytes.len() {
            let c = bytes[end];
            if c.is_ascii_alphanumeric() || c == b'_' {
                end += 1;
            } else {
                break;
            }
        }
        // Must have at least 1 char after the sigil.
        if end <= i + 1 {
            continue;
        }
        // Cashtags: only pure alpha tickers (avoid $5, $100 etc).
        if is_cashtag {
            let tag_body = &text[i + 1..end];
            if !tag_body.chars().all(|c| c.is_ascii_alphabetic()) {
                continue;
            }
        }
        matches.push((i, end, is_cashtag));
    }

    if matches.is_empty() {
        return text.to_string();
    }

    // Sort and deduplicate overlaps (keep earliest).
    matches.sort_by_key(|&(s, _, _)| s);
    let mut out = String::with_capacity(text.len() + matches.len() * 80);
    let mut last_end = 0usize;
    for &(start, end, is_cashtag) in &matches {
        if start < last_end {
            continue;
        }
        out.push_str(&text[last_end..start]);
        let tag_text = &text[start..end];
        let tag_body = &text[start + 1..end]; // without sigil
        let href = if is_cashtag {
            format!("https://x.com/search?q=%24{}", crate::push::escape_attr(tag_body))
        } else {
            format!("https://x.com/hashtag/{}", crate::push::escape_attr(tag_body))
        };
        out.push_str(&format!(
            "<a href=\"{}\">{}</a>",
            href,
            crate::push::escape_html(tag_text)
        ));
        last_end = end;
    }
    out.push_str(&text[last_end..]);
    out
}

/// Extract a tweet's full text (note_tweet takes priority, falling back to
/// legacy.full_text — the convention fixed in #7) and clean t.co links,
/// returning readable text. M7 fix: the main tweet/RT/QT three call sites
/// previously repeated the same extraction chain verbatim; collapsed into a
/// single function.
fn tweet_full_text(result: &Value) -> String {
    let legacy = &result["legacy"];
    let raw = result["note_tweet"]["note_tweet_results"]["result"]["text"]
        .as_str()
        .or_else(|| legacy["full_text"].as_str())
        .unwrap_or_default();
    let note_urls = result["note_tweet"]["note_tweet_results"]["result"]["entity_set"]["urls"]
        .as_array();
    clean_tweet_text(raw, legacy, note_urls)
}

/// Append a RT/QT blockquote to the body. `prefix` = "RT" or "QT".
/// Quoted media is always included (QT matches RT, v1.10.16): an image-only
/// quote tweet must not degrade to plain text when the main tweet itself has
/// no media. M7 fix: the former RT and QT blocks were verbatim duplicates;
/// collapsed into a single function.
fn append_quote_block(body: &mut String, result: &Value, prefix: &str) {
    // Handle TweetWithVisibilityResults wrapper (shared with tweet_to_entry).
    let result = unwrap_tweet_result(result);
    // Deleted/protected quotes arrive as TweetTombstone/TweetUnavailable with
    // no legacy payload. Rendering them produced an empty quote block and a
    // bogus link to x.com/unknown (extract_screen_name falls back to
    // "unknown").
    if !quote_renderable(result) {
        return;
    }
    let legacy = &result["legacy"];
    let text = tweet_full_text(result);
    let screen = extract_screen_name(result);
    let mut mentions = extract_mention_names(result);
    // Same as tweet_to_entry: add the author's screen_name so self-mentions
    // also get hyperlinked (v1.10.14).
    if !mentions.iter().any(|n| n.eq_ignore_ascii_case(screen)) {
        mentions.push(screen.to_string());
    }
    let mut inner = linkify_hashtags(&linkify_mentions(&tweet_text_to_html(&text), &mentions));
    // Quoted media is always included (QT matches RT): an image-only quote
    // tweet must not degrade to plain text when the main tweet has no media.
    append_media_tags(&mut inner, legacy);
    // Link to the specific tweet if id is available, else user profile.
    let link = match legacy["id_str"].as_str() {
        Some(tid) => format!("https://x.com/{}/status/{}", screen, tid),
        None => format!("https://x.com/{}", screen),
    };
    // Consistent with weibo.rs: when body is non-empty, separate the original
    // text from the quote block with \n — otherwise </a> sits flush against
    // <blockquote>, Telegram fails to recognize the blockquote boundary, and
    // after the content degrades to plain text the @mention merges with the
    // QT/RT prefix into a wrong username (e.g. @BotQT).
    if !body.is_empty() {
        body.push('\n');
    }
    body.push_str(&format!(
        "<blockquote>{} <a href=\"{}\">@{}</a>: {}</blockquote>",
        prefix,
        crate::push::escape_attr(&link),
        crate::push::escape_html(screen),
        inner
    ));
}

/// Result of converting one collected tweet to an Entry, distinguishing an
/// expected skip from a genuine parse failure so the batch-end dropped-warn
/// only reflects structural breakage (replies were previously counted as
/// dropped and fired the "legacy missing?" warn on any reply-heavy day).
enum ConvertOutcome {
    /// Converted successfully. Entry is boxed: feed-rs Entry is large and the
    /// skip/fail variants carry no data, so an inline Entry would make every
    /// outcome the size of Entry (clippy::large_enum_variant).
    Ok(Box<Entry>),
    /// Expected skip: a reply (the timeline only pushes root tweets).
    SkippedReply,
    /// Unexpected failure: legacy missing or no usable id (API shape change).
    Failed,
}

/// Convert a GraphQL tweet result object into a feed-rs Entry.
fn tweet_to_entry(tweet: &Value, screen_name: &str, display_name: &str, lang: Lang) -> ConvertOutcome {
    // Handle TweetWithVisibilityResults wrapper (shared with collect_tweet_results).
    let tweet = unwrap_tweet_result(tweet);

    let legacy = &tweet["legacy"];
    if legacy.is_null() {
        return ConvertOutcome::Failed;
    }
    let Some(id_str) = legacy["id_str"].as_str().or_else(|| tweet["rest_id"].as_str()) else {
        return ConvertOutcome::Failed;
    };
    let id = id_str.to_string();

    // Skip replies (only root tweets in the timeline). Expected, not a
    // conversion failure: counted separately, never in the dropped warn.
    if legacy["in_reply_to_status_id_str"].is_string() {
        return ConvertOutcome::SkippedReply;
    }

    // Full text: note_tweet takes priority, falling back to legacy.full_text
    // (unified convention via tweet_full_text).
    let text = tweet_full_text(tweet);

    // Pure-retweet detection: the outer full_text is X's auto-generated
    // "RT @xxx: original text"; rendering it would duplicate the retweet
    // content in the blockquote below → skip the outer body and keep only
    // the blockquote.
    let rt = &legacy["retweeted_status_result"]["result"];
    let is_pure_rt = !rt.is_null() && text.starts_with("RT @");

    // Build HTML body (escape + auto-link URLs + newlines to <br>).
    // For pure retweets the body stays empty; only the RT blockquote is emitted.
    let mut body = String::new();
    if !is_pure_rt {
        let mut mentions = extract_mention_names(tweet);
        // The X API's user_mentions does not include the author's self-mention
        // (e.g. Elon Musk's body text @elonmusk); add the author's screen_name
        // so self-mentions also get hyperlinked (v1.10.14).
        if !mentions.iter().any(|n| n.eq_ignore_ascii_case(screen_name)) {
            mentions.push(screen_name.to_string());
        }
        body = linkify_hashtags(&linkify_mentions(&tweet_text_to_html(&text), &mentions));
        // Media: images and videos from legacy.entities.media / extended_entities.
        append_media_tags(&mut body, legacy);
    }

    // Retweet → blockquote (including the RT's media tags).
    if !rt.is_null() {
        append_quote_block(&mut body, rt, "RT");
    }

    // Quote tweet → also blockquote (including the quoted tweet's media tags).
    // v1.10.16: previously include_media=false meant that when the main tweet
    // had no media and all images were in the quoted tweet, the whole push
    // degraded to plain text (e.g. an elonmusk QT of a tweet with images).
    // Changed to true to align with RT.
    let quote = &tweet["quoted_status_result"]["result"];
    if !quote.is_null() {
        append_quote_block(&mut body, quote, "QT");
    }

    // Parse created_at: "Wed Oct 10 20:19:24 +0000 2018"
    let published = parse_twitter_time(legacy["created_at"].as_str().unwrap_or(""));

    // Title: first 50 chars of the CLEANED text (no t.co noise).
    let title = crate::platforms::make_title(&text, i18n::t(lang, "x_tweet_title"));

    let link = format!("https://x.com/{}/status/{}", screen_name, id);

    ConvertOutcome::Ok(Box::new(crate::platforms::make_entry(
        id, title, display_name.to_string(), body, "text/html", link, published,
    )))
}

/// Stable tweet id of a timeline item (legacy id_str, then rest_id).
fn tweet_id_of(value: &Value) -> Option<&str> {
    let tr = unwrap_tweet_result(value);
    tr["legacy"]["id_str"]
        .as_str()
        .or_else(|| tr["rest_id"].as_str())
}

/// Whether a quoted/retweeted payload is a real tweet (not a tombstone or
/// an unavailable/protected placeholder) and carries something renderable.
fn quote_renderable(tr: &Value) -> bool {
    let legacy = &tr["legacy"];
    if !legacy.is_object() {
        return false;
    }
    let has_id = legacy["id_str"].as_str().is_some_and(|s| !s.is_empty())
        || tr["rest_id"].as_str().is_some_and(|s| !s.is_empty());
    let has_text = !tweet_full_text(tr).trim().is_empty();
    let has_media = legacy["extended_entities"]["media"].is_array()
        || legacy["entities"]["media"].is_array();
    has_id && (has_text || has_media)
}

/// Extract screen_name from a tweet result object, trying multiple API paths.
/// Handles TweetWithVisibilityResults wrapper transparently.
fn extract_screen_name(tweet_result: &Value) -> &str {
    // Unwrap TweetWithVisibilityResults if present (shared helper).
    let tr = unwrap_tweet_result(tweet_result);
    // Path 1: core.screen_name (older API shape)
    if let Some(s) = tr["core"]["screen_name"].as_str() {
        return s;
    }
    // Path 2: core.user_results.result.legacy.screen_name
    if let Some(s) = tr["core"]["user_results"]["result"]["legacy"]["screen_name"].as_str() {
        return s;
    }
    // Path 3: core.user_results.result.core.screen_name
    if let Some(s) = tr["core"]["user_results"]["result"]["core"]["screen_name"].as_str() {
        return s;
    }
    // Path 4: user_results.result.legacy.screen_name (some responses nest user at top level)
    if let Some(s) = tr["user_results"]["result"]["legacy"]["screen_name"].as_str() {
        return s;
    }
    // Path 5: legacy.screen_name (rare fallback)
    if let Some(s) = tr["legacy"]["screen_name"].as_str() {
        return s;
    }
    // Log the actual structure so we can fix the path next time.
    let keys: Vec<&str> = tr.as_object().map(|m| m.keys().map(|k| k.as_str()).collect()).unwrap_or_default();
    let core_keys: Vec<&str> = tr["core"].as_object().map(|m| m.keys().map(|k| k.as_str()).collect()).unwrap_or_default();
    tracing::warn!(
        rest_id = ?tr["rest_id"].as_str(),
        top_keys = ?keys,
        core_keys = ?core_keys,
        "extract_screen_name: no path matched, falling back to unknown"
    );
    "unknown"
}

/// Clean tweet text by processing t.co URL entities:
/// 1) Remove media placeholder t.co URLs (images/videos are sent as attachments).
/// 2) Replace regular t.co URLs with their expanded_url (real target).
///
/// `legacy` is the tweet's legacy object; `note_urls` is the optional
/// note_tweet entity_set urls array (takes precedence over legacy entities).
fn clean_tweet_text(full_text: &str, legacy: &Value, note_urls: Option<&Vec<Value>>) -> String {
    let mut text = full_text.to_string();

    // Remove media placeholder t.co URLs.
    let media_entities = legacy["extended_entities"]["media"]
        .as_array()
        .or_else(|| legacy["entities"]["media"].as_array());
    if let Some(arr) = media_entities {
        for m in arr {
            if let Some(tco) = m["url"].as_str() {
                text = text.replace(tco, "");
            }
        }
    }

    // Replace regular t.co URLs with expanded_url.
    let url_entities = note_urls.or_else(|| legacy["entities"]["urls"].as_array());
    if let Some(urls) = url_entities {
        for u in urls {
            let tco = u["url"].as_str().unwrap_or_default();
            let expanded = u["expanded_url"].as_str().unwrap_or_default();
            if !tco.is_empty() && !expanded.is_empty() {
                text = text.replace(tco, expanded);
            }
        }
    }

    // Trim trailing whitespace left after removing media placeholders.
    text.trim_end().to_string()
}

/// Convert cleaned tweet text to Telegram-HTML: escape special chars,
/// auto-link bare URLs (so expanded_url targets are clickable), and
/// convert newlines to <br>. Media tags are appended separately.
fn tweet_text_to_html(text: &str) -> String {
    static RE_URL: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#"https?://[^\s<>"]+"#).unwrap());
    let mut out = String::new();
    let mut last = 0usize;
    for caps in RE_URL.captures_iter(text) {
        let m = caps.get(0).unwrap();
        // Escape and append the text before this URL.
        out.push_str(&crate::push::escape_html(&text[last..m.start()]));
        // Strip trailing sentence punctuation and unbalanced closing brackets
        // that aren't part of the URL. The URL regex `[^\s<>"]+` happily swallows
        // a `)`/`]`/`}` that actually belongs to surrounding prose (e.g.
        // "(see https://x.com/a)"), producing a broken link. Brackets are only
        // stripped while they are unbalanced (closers exceed openers), so URLs
        // with legitimately balanced parens (e.g. Wikipedia links) are kept.
        let mut url = m.as_str();
        loop {
            let before = url;
            while url.ends_with(['.', ',', ';', '!', '?']) {
                url = &url[..url.len() - 1];
            }
            for (open, close) in [('(', ')'), ('[', ']'), ('{', '}')] {
                while url.ends_with(close)
                    && url.matches(close).count() > url.matches(open).count()
                {
                    url = &url[..url.len() - close.len_utf8()];
                }
            }
            if url.len() == before.len() {
                break;
            }
        }
        out.push_str(&format!(
            "<a href=\"{}\">{}</a>",
            crate::push::escape_attr(url),
            crate::push::escape_html(url)
        ));
        // Emit the stripped trailing punctuation after the link (otherwise the
        // body would lose periods etc.).
        let stripped = &m.as_str()[url.len()..];
        out.push_str(&crate::push::escape_html(stripped));
        last = m.end();
    }
    out.push_str(&crate::push::escape_html(&text[last..]));
    // Convert newlines to <br>
    out.replace('\n', "<br>")
}

/// Parse Twitter's created_at format: "Wed Oct 10 20:19:24 +0000 2018".
fn parse_twitter_time(s: &str) -> Option<DateTime<Utc>> {
    // Format: "%a %b %d %H:%M:%S %z %Y"
    chrono::DateTime::parse_from_str(s, "%a %b %d %H:%M:%S %z %Y")
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_screen_name_valid() {
        assert_eq!(parse_screen_name("https://x.com/elonmusk"), Some("elonmusk".into()));
        assert_eq!(parse_screen_name("https://twitter.com/NASA"), Some("NASA".into()));
        assert_eq!(parse_screen_name("https://www.x.com/some_user"), Some("some_user".into()));
        assert_eq!(parse_screen_name("https://x.com/a"), Some("a".into()));
    }

    #[test]
    fn parse_screen_name_invalid() {
        assert_eq!(parse_screen_name("https://x.com/"), None);
        assert_eq!(parse_screen_name("https://x.com/home"), None);
        assert_eq!(parse_screen_name("https://x.com/search?q=test"), None);
        assert_eq!(parse_screen_name("https://x.com/i/flow/login"), None);
        assert_eq!(parse_screen_name("https://x.com/settings"), None);
        assert_eq!(parse_screen_name("https://example.com/elonmusk"), None);
        // Too long (>15 chars)
        assert_eq!(parse_screen_name("https://x.com/abcdefghijklmnop"), None);
    }

    #[test]
    fn parse_twitter_time_works() {
        let t = parse_twitter_time("Wed Oct 10 20:19:24 +0000 2018");
        assert!(t.is_some());
        let dt = t.unwrap();
        assert_eq!(dt.format("%Y-%m-%d").to_string(), "2018-10-10");
    }

    #[test]
    fn extract_cookie_value_works() {
        let header = "ct0=abc123xyz; Max-Age=14400; Domain=.x.com; Path=/; Secure";
        assert_eq!(extract_cookie_value(header, "ct0"), Some("abc123xyz".into()));
        assert_eq!(extract_cookie_value(header, "other"), None);
    }

    #[test]
    fn extract_cookie_value_scans_all_segments() {
        // Regression: the target key may not be the first `;` segment
        // (some Set-Cookie headers carry attributes or extra cookies first);
        // aligned with media.rs semantics.
        let header = "guest_id=v1%3A123; ct0=abc123xyz; Path=/; Secure";
        assert_eq!(extract_cookie_value(header, "ct0"), Some("abc123xyz".into()));
        assert_eq!(extract_cookie_value(header, "guest_id"), Some("v1%3A123".into()));
    }

    #[test]
    fn extract_timeline_page_reads_bottom_cursor_from_content_value() {
        let instructions = serde_json::json!([
            {
                "type": "TimelineAddEntries",
                "entries": [
                    {"entryId": "cursor-top-1", "content": {"cursorType": "Top", "value": "TOPCURSOR"}},
                    {"entryId": "tweet-123", "content": {"itemContent": {"tweet_results": {"result":
                        {"rest_id": "123", "legacy": {"id_str": "123"}}}}}},
                    {"entryId": "cursor-bottom-1", "content": {"cursorType": "Bottom", "value": "BOTTOMCURSOR"}}
                ]
            }
        ]);
        let page = extract_timeline_page(&instructions);
        assert_eq!(page.tweets.len(), 1);
        assert_eq!(page.non_cursor_entries, 1);
        assert_eq!(page.bottom_cursor.as_deref(), Some("BOTTOMCURSOR"));
    }

    #[test]
    fn extract_timeline_page_reads_cursor_from_suggest_cursor_and_entry_id() {
        // TimelineSuggestCursor instruction, token embedded in the entryId.
        let instructions = serde_json::json!([
            {"type": "TimelineAddEntries", "entries": [
                {"entryId": "tweet-1", "content": {"itemContent": {"tweet_results": {"result":
                    {"rest_id": "1", "legacy": {"id_str": "1"}}}}}}
            ]},
            {"type": "TimelineSuggestCursor", "entry":
                {"entryId": "cursor-bottom-ABC123", "content": {"cursorType": "Bottom"}}}
        ]);
        let page = extract_timeline_page(&instructions);
        assert_eq!(page.tweets.len(), 1);
        assert_eq!(page.bottom_cursor.as_deref(), Some("ABC123"));
    }

    #[test]
    fn extract_timeline_page_no_cursor_when_bottom_missing() {
        let instructions = serde_json::json!([
            {"type": "TimelineAddEntries", "entries": [
                {"entryId": "cursor-top-1", "content": {"cursorType": "Top", "value": "TOP"}},
                {"entryId": "tweet-1", "content": {"itemContent": {"tweet_results": {"result":
                    {"rest_id": "1", "legacy": {"id_str": "1"}}}}}}
            ]}
        ]);
        let page = extract_timeline_page(&instructions);
        assert_eq!(page.bottom_cursor, None);
    }

    #[test]
    fn tweet_to_entry_classifies_reply_as_expected_skip() {
        // Replies must NOT be reported as conversion failures (dropped):
        // they are an expected skip counted separately.
        let reply = serde_json::json!({
            "rest_id": "5",
            "legacy": {
                "id_str": "5",
                "full_text": "hi",
                "in_reply_to_status_id_str": "4"
            }
        });
        assert!(matches!(
            tweet_to_entry(&reply, "user", "User", Lang::Zh),
            ConvertOutcome::SkippedReply
        ));
        // Missing legacy is a genuine structural failure.
        let broken = serde_json::json!({"rest_id": "6"});
        assert!(matches!(
            tweet_to_entry(&broken, "user", "User", Lang::Zh),
            ConvertOutcome::Failed
        ));
        let ok = serde_json::json!({
            "rest_id": "7",
            "legacy": {"id_str": "7", "full_text": "hello"}
        });
        assert!(matches!(
            tweet_to_entry(&ok, "user", "User", Lang::Zh),
            ConvertOutcome::Ok(_)
        ));
        let empty = serde_json::json!({
            "rest_id": "8",
            "legacy": {"id_str": "8", "full_text": ""}
        });
        let ConvertOutcome::Ok(entry) = tweet_to_entry(&empty, "user", "User", Lang::En) else {
            panic!("empty tweet should still convert");
        };
        assert_eq!(entry.title.as_ref().unwrap().content, "(Tweet)");
    }

    #[test]
    fn linkify_mentions_basic() {
        let names = vec!["elonmusk".to_string()];
        let html = "Hello @elonmusk how are you";
        let out = linkify_mentions(html, &names);
        assert_eq!(
            out,
            "Hello <a href=\"https://x.com/elonmusk\">@elonmusk</a> how are you"
        );
    }

    #[test]
    fn linkify_mentions_case_insensitive_match_preserves_original_case() {
        let names = vec!["elonmusk".to_string()];
        let html = "cc @ElonMusk";
        let out = linkify_mentions(html, &names);
        assert_eq!(
            out,
            "cc <a href=\"https://x.com/elonmusk\">@ElonMusk</a>"
        );
    }

    #[test]
    fn linkify_mentions_skips_inside_existing_anchor() {
        let names = vec!["elonmusk".to_string()];
        let html = "<a href=\"https://x.com/elonmusk\">@elonmusk</a> and @elonmusk";
        let out = linkify_mentions(html, &names);
        // First occurrence is inside an anchor and must stay verbatim;
        // the second one gets linked.
        assert_eq!(
            out,
            "<a href=\"https://x.com/elonmusk\">@elonmusk</a> and <a href=\"https://x.com/elonmusk\">@elonmusk</a>"
        );
    }

    #[test]
    fn linkify_mentions_boundary_checks() {
        let names = vec!["elonmusk".to_string()];
        // Preceded by alnum → not a mention.
        assert_eq!(linkify_mentions("foo@elonmusk", &names), "foo@elonmusk");
        // Followed by alnum → not a mention (e.g. @elonmusk2x).
        assert_eq!(linkify_mentions("@elonmusk2x", &names), "@elonmusk2x");
        // Followed by underscore → not a mention.
        assert_eq!(linkify_mentions("@elonmusk_", &names), "@elonmusk_");
        // At start of string → mention.
        assert!(linkify_mentions("@elonmusk", &names).contains("<a href"));
        // After punctuation → mention.
        assert!(linkify_mentions("(@elonmusk)", &names).contains("<a href"));
    }

    #[test]
    fn linkify_mentions_no_names_noop() {
        let html = "Hello @elonmusk";
        assert_eq!(linkify_mentions(html, &[]), html);
    }

    #[test]
    fn linkify_mentions_multiple_mentions() {
        let names = vec!["alice".to_string(), "bob".to_string()];
        let html = "@alice meets @bob";
        let out = linkify_mentions(html, &names);
        assert_eq!(
            out,
            "<a href=\"https://x.com/alice\">@alice</a> meets <a href=\"https://x.com/bob\">@bob</a>"
        );
    }

    #[test]
    fn linkify_mentions_self_mention_by_author() {
        // v1.10.14: author's own screen_name is appended to the names list
        // by tweet_to_entry/append_quote_block, so self-mentions get linked.
        let names = vec!["SpaceXSI".to_string(), "elonmusk".to_string()];
        let html = "Great work @SpaceXSI — @elonmusk";
        let out = linkify_mentions(html, &names);
        assert!(out.contains(r#"<a href="https://x.com/SpaceXSI">@SpaceXSI</a>"#), "out: {}", out);
        assert!(out.contains(r#"<a href="https://x.com/elonmusk">@elonmusk</a>"#), "out: {}", out);
    }

    #[test]
    fn extract_mention_names_from_legacy_entities() {
        let json: Value = serde_json::json!({
            "legacy": {
                "entities": {
                    "user_mentions": [
                        {"screen_name": "alice"},
                        {"screen_name": "bob"}
                    ]
                }
            }
        });
        let names = extract_mention_names(&json);
        assert_eq!(names, vec!["alice".to_string(), "bob".to_string()]);
    }

    #[test]
    fn extract_mention_names_dedupes_note_tweet_mentions() {
        let json: Value = serde_json::json!({
            "legacy": {
                "entities": {
                    "user_mentions": [{"screen_name": "alice"}]
                }
            },
            "note_tweet": {
                "note_tweet_results": {
                    "result": {
                        "entity_set": {
                            "mentions": [
                                {"screen_name": "Alice"},
                                {"screen_name": "carol"}
                            ]
                        }
                    }
                }
            }
        });
        let names = extract_mention_names(&json);
        // "Alice" is a case-insensitive duplicate of "alice" and must not be added.
        assert_eq!(names, vec!["alice".to_string(), "carol".to_string()]);
    }

    #[test]
    fn append_quote_block_newline_separator() {
        // Regression test: when body is non-empty there must be a \n
        // separator before the quote block — otherwise </a> sits flush
        // against <blockquote> and Telegram fails to recognize the
        // blockquote boundary (observed in practice: @Bot + QT merged into
        // @BotQT and auto-linked as t.me/BotQT).
        let tweet: Value = serde_json::json!({
            "legacy": {
                "full_text": "quoted text",
                "id_str": "123",
                "entities": {
                    "user_mentions": [],
                    "media": []
                }
            },
            "core": {
                "user_results": {
                    "result": {
                        "legacy": { "screen_name": "someuser" }
                    }
                }
            }
        });
        // body non-empty: must have a \n separator
        let mut body = String::from("Grok <a href=\"https://x.com/Bot\">@Bot</a>");
        append_quote_block(&mut body, &tweet, "QT");
        assert!(body.contains("@Bot</a>\n<blockquote>"), "missing \\n separator: {}", body);
        assert!(!body.contains("@Bot</a><blockquote>"), "blockquote glued to anchor: {}", body);

        // body empty (pure RT): no leading \n
        let mut empty_body = String::new();
        append_quote_block(&mut empty_body, &tweet, "RT");
        assert!(empty_body.starts_with("<blockquote>"), "empty body should not have leading \\n: {}", empty_body);
    }

    #[test]
    fn linkify_hashtags_basic() {
        let out = linkify_hashtags("Hello #Tesla world");
        assert!(out.contains("<a href=\"https://x.com/hashtag/Tesla\">#Tesla</a>"), "got: {}", out);
        assert!(out.starts_with("Hello "), "prefix lost: {}", out);
        assert!(out.ends_with(" world"), "suffix lost: {}", out);
    }

    #[test]
    fn linkify_hashtags_cashtag() {
        let out = linkify_hashtags("Buy $TSLA now");
        assert!(out.contains("<a href=\"https://x.com/search?q=%24TSLA\">$TSLA</a>"), "got: {}", out);
    }

    #[test]
    fn linkify_hashtags_skips_numeric_cashtag() {
        // $5, $100 are not cashtags (pure digits)
        let out = linkify_hashtags("Price is $5 and $100");
        assert!(!out.contains("<a href"), "numeric should not linkify: {}", out);
    }

    #[test]
    fn linkify_hashtags_skips_inside_anchor() {
        let out = linkify_hashtags("<a href=\"https://x.com/foo\">#Tesla</a> plain #SpaceX");
        // #Tesla inside anchor should NOT be double-wrapped
        assert_eq!(out.matches("<a href").count(), 2, "got: {}", out);
        assert!(out.contains("#SpaceX</a>"), "plain hashtag should be linkified: {}", out);
    }

    #[test]
    fn linkify_hashtags_boundary_checks() {
        // No match when preceded by an alphanumeric char
        assert_eq!(linkify_hashtags("foo#Tesla"), "foo#Tesla");
        // No match with an underscore prefix
        assert_eq!(linkify_hashtags("foo_#Tesla"), "foo_#Tesla");
        // Matches at line start
        assert!(linkify_hashtags("#Tesla is great").contains("<a href"));
        // Matches after an opening parenthesis
        assert!(linkify_hashtags("(#Tesla)").contains("<a href"));
    }

    #[test]
    fn linkify_hashtags_multiple() {
        let out = linkify_hashtags("#DavidLetterman #ElonMusk #Tesla");
        assert_eq!(out.matches("<a href").count(), 3, "got: {}", out);
        assert!(out.contains("hashtag/DavidLetterman"), "got: {}", out);
        assert!(out.contains("hashtag/ElonMusk"), "got: {}", out);
        assert!(out.contains("hashtag/Tesla"), "got: {}", out);
    }

    #[test]
    fn linkify_hashtags_no_tags_noop() {
        let html = "Hello world, no tags here.";
        assert_eq!(linkify_hashtags(html), html);
    }

    #[test]
    fn linkify_hashtags_with_br() {
        // In tweet_text_to_html output newlines have already become <br>;
        // a tag right after <br> must still match
        let out = linkify_hashtags("text<br>#Tesla<br>more");
        assert!(out.contains("<a href=\"https://x.com/hashtag/Tesla\">#Tesla</a>"), "got: {}", out);
    }

    #[test]
    fn tweet_text_to_html_strips_trailing_unbalanced_paren() {
        // A URL wrapped in prose parentheses must not swallow the closing ')'.
        let out = tweet_text_to_html("see (https://x.com/a)");
        assert!(
            out.contains("<a href=\"https://x.com/a\">https://x.com/a</a>)"),
            "got: {}",
            out
        );
    }

    #[test]
    fn tweet_text_to_html_keeps_balanced_parens_in_url() {
        // URLs with legitimately balanced parens (e.g. Wikipedia) are kept whole.
        let out = tweet_text_to_html("https://en.wikipedia.org/wiki/Foo_(bar)");
        assert!(
            out.contains("<a href=\"https://en.wikipedia.org/wiki/Foo_(bar)\">"),
            "got: {}",
            out
        );
    }

    #[test]
    fn tweet_text_to_html_strips_trailing_punct_then_paren() {
        // "(url)." — period stripped first, then the now-unbalanced ')'.
        let out = tweet_text_to_html("(https://x.com/a).");
        assert!(
            out.contains("<a href=\"https://x.com/a\">https://x.com/a</a>)."),
            "got: {}",
            out
        );
    }

    #[test]
    fn collect_tweet_results_unwraps_visibility_wrapper() {
        // A TweetWithVisibilityResults wrapper carries rest_id only on the
        // inner tweet; collect must unwrap it (previously such tweets were
        // silently dropped before reaching tweet_to_entry).
        let entry = serde_json::json!({
            "entryId": "tweet-1",
            "content": {
                "itemContent": {
                    "tweet_results": {
                        "result": {
                            "__typename": "TweetWithVisibilityResults",
                            "tweet": { "rest_id": "123", "legacy": { "id_str": "123" } }
                        }
                    }
                }
            }
        });
        let mut out = Vec::new();
        collect_tweet_results(&entry, &mut out);
        assert_eq!(out.len(), 1, "wrapper tweet should be collected");
        assert_eq!(out[0]["rest_id"].as_str(), Some("123"));
    }

    #[test]
    fn unwrap_tweet_result_plain_passthrough() {
        let plain = serde_json::json!({ "rest_id": "999", "legacy": {} });
        assert_eq!(unwrap_tweet_result(&plain)["rest_id"].as_str(), Some("999"));
    }

    #[test]
    fn append_media_tags_video_poster_object_shape() {
        // preview_image_url is an object with a url field in the standard
        // GraphQL shape; the poster attribute must be emitted so the
        // oversized-video cover fallback has a URL to send. The cover is
        // upgraded to name=large (no orig exists for video previews).
        let legacy: Value = serde_json::json!({
            "extended_entities": { "media": [{
                "type": "video",
                "preview_image_url": { "url": "https://pbs.twimg.com/media/COVER.jpg" },
                "video_info": { "variants": [
                    { "content_type": "video/mp4", "bitrate": 832000, "url": "https://video.twimg.com/x/low.mp4" },
                    { "content_type": "video/mp4", "bitrate": 2560000, "url": "https://video.twimg.com/x/hd.mp4" }
                ]}
            }]}
        });
        let mut body = String::new();
        append_media_tags(&mut body, &legacy);
        assert_eq!(
            body,
            "<video src=\"https://video.twimg.com/x/hd.mp4\" poster=\"https://pbs.twimg.com/media/COVER.jpg?name=large\"></video>"
        );
    }

    #[test]
    fn append_media_tags_video_poster_bare_string_shape() {
        // Some response shapes carry preview_image_url as a bare string;
        // both forms must be accepted (same dual-shape handling as weibo
        // page_pic). The timeline default is name=small and must be
        // replaced with large.
        let legacy: Value = serde_json::json!({
            "extended_entities": { "media": [{
                "type": "video",
                "preview_image_url": "https://pbs.twimg.com/media/COVER2.jpg?name=small",
                "video_info": { "variants": [
                    { "content_type": "video/mp4", "bitrate": 100, "url": "https://video.twimg.com/y/a.mp4" }
                ]}
            }]}
        });
        let mut body = String::new();
        append_media_tags(&mut body, &legacy);
        assert!(body.contains("poster=\"https://pbs.twimg.com/media/COVER2.jpg?name=large\""));
        assert!(!body.contains("name=small"));
    }

    #[test]
    fn upgrade_x_cover_modes() {
        // No query: append name=large.
        assert_eq!(
            upgrade_x_cover("https://pbs.twimg.com/media/A.jpg"),
            "https://pbs.twimg.com/media/A.jpg?name=large"
        );
        // name=small: replace in place, keep other params.
        assert_eq!(
            upgrade_x_cover("https://pbs.twimg.com/media/B.jpg?format=jpg&name=small"),
            "https://pbs.twimg.com/media/B.jpg?format=jpg&name=large"
        );
        // name=orig (photos never reach here, but be safe): orig 404s for
        // re-encoded video covers, downgrade to large.
        assert_eq!(
            upgrade_x_cover("https://pbs.twimg.com/media/C.jpg?name=orig"),
            "https://pbs.twimg.com/media/C.jpg?name=large"
        );
        // Already large: unchanged.
        assert_eq!(
            upgrade_x_cover("https://pbs.twimg.com/media/D.jpg?name=large"),
            "https://pbs.twimg.com/media/D.jpg?name=large"
        );
    }

    #[test]
    fn append_media_tags_video_poster_falls_back_to_media_url() {
        // Timeline responses can omit preview_image_url for video entities
        // (observed for videos inside retweets); media_url_https carries the
        // same preview frame and must become the poster.
        let legacy: Value = serde_json::json!({
            "extended_entities": { "media": [{
                "type": "video",
                "media_url_https": "https://pbs.twimg.com/amplify_video_thumb/123/img/cover.jpg",
                "video_info": { "variants": [
                    { "content_type": "video/mp4", "bitrate": 100, "url": "https://video.twimg.com/x/a.mp4?tag=29" }
                ]}
            }]}
        });
        let mut body = String::new();
        append_media_tags(&mut body, &legacy);
        assert_eq!(
            body,
            "<video src=\"https://video.twimg.com/x/a.mp4?tag=29\" poster=\"https://pbs.twimg.com/amplify_video_thumb/123/img/cover.jpg?name=large\"></video>"
        );
    }

    #[test]
    fn append_media_tags_video_without_preview_keeps_plain_tag() {
        // No usable preview_image_url: the tag stays without a poster
        // attribute (unchanged legacy behavior).
        let legacy: Value = serde_json::json!({
            "extended_entities": { "media": [{
                "type": "animated_gif",
                "video_info": { "variants": [
                    { "content_type": "video/mp4", "bitrate": 0, "url": "https://video.twimg.com/z/g.mp4" }
                ]}
            }]}
        });
        let mut body = String::new();
        append_media_tags(&mut body, &legacy);
        assert_eq!(
            body,
            "<video src=\"https://video.twimg.com/z/g.mp4\"></video>"
        );
    }

    #[test]
    fn tombstoned_quote_is_not_renderable() {
        let tombstone: Value = serde_json::json!({
            "__typename": "TweetTombstone",
            "tombstone": { "text": { "text": "This Tweet is unavailable" } }
        });
        assert!(!quote_renderable(unwrap_tweet_result(&tombstone)));
        let normal: Value = serde_json::json!({
            "rest_id": "123",
            "legacy": { "id_str": "123", "full_text": "hello" }
        });
        assert!(quote_renderable(&normal));
    }
}
