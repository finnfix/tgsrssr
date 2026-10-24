use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, Semaphore};

use crate::config::Config;
use crate::db::Db;

/// Desktop browser UA: impersonates Safari on macOS.
/// Used for RSS fetching / media downloads / X platform (desktop endpoints).
/// Note: in real Safari UAs the macOS version segment is frozen at 10_15_7
/// (privacy/compatibility); the browser identity is carried by the
/// Version/Safari segment — here we use the current major version 27.
pub const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/605.1.15 (KHTML, like Gecko) Version/27.0 Safari/605.1.15";

/// Mobile browser UA: impersonates Safari on iPhone.
/// Used for weibo m.weibo.cn (mobile endpoint, where a mobile UA is most
/// natural). The iOS version uses the real format 27_0 (iPhone UAs carry the
/// real iOS version, unlike the frozen macOS one).
pub const MOBILE_BROWSER_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 27_0 like Mac OS X) \
    AppleWebKit/605.1.15 (KHTML, like Gecko) Version/27.0 Mobile/15E148 Safari/604.1";

/// HTTP client routing decision (one of: insecure whitelist / direct / proxy).
enum RouteKind {
    Insecure,
    Direct,
    Proxy,
}

/// Pending multi-step operations (e.g. waiting for a rename reply).
#[derive(Debug, Clone)]
pub enum PendingOp {
    RenameSub {
        sub_id: i64,
        /// The prompt message (retired when the flow ends).
        prompt_msg_id: i32,
        /// The feed (source) page the user came from; refreshed in place
        /// after a successful rename instead of sending a new panel.
        panel_msg_id: i32,
        /// The source-list page on the channel page; restored when returning.
        ch_page: usize,
        /// The channel-list page; also restored on the return path.
        chlist_page: usize,
        /// When the flow started; used to auto-expire stale prompts.
        created_at: std::time::Instant,
    },
    /// Waiting for the user to reply with a platform's new login cookie.
    SetCookie {
        /// Platform id (e.g. "weibo").
        platform: String,
        /// The prompt message (retired when the flow ends).
        prompt_msg_id: i32,
        /// The /cookies panel; refreshed in place after a successful update.
        panel_msg_id: i32,
        created_at: std::time::Instant,
    },
    /// Waiting for the user to reply with filter keywords for a feed.
    SetFilterKeywords {
        sub_id: i64,
        prompt_msg_id: i32,
        /// The filter sub-page panel; refreshed in place after save.
        panel_msg_id: i32,
        ch_page: usize,
        /// Channel-list page to restore on the return path.
        chlist_page: usize,
        created_at: std::time::Instant,
    },
}

impl PendingOp {
    /// The prompt message id (present in every variant).
    pub fn prompt_msg_id(&self) -> i32 {
        match self {
            PendingOp::RenameSub { prompt_msg_id, .. } => *prompt_msg_id,
            PendingOp::SetCookie { prompt_msg_id, .. } => *prompt_msg_id,
            PendingOp::SetFilterKeywords { prompt_msg_id, .. } => *prompt_msg_id,
        }
    }

    /// When the flow started (present in every variant); used by the
    /// periodic sweeper to expire abandoned flows.
    pub fn created_at(&self) -> std::time::Instant {
        match self {
            PendingOp::RenameSub { created_at, .. } => *created_at,
            PendingOp::SetCookie { created_at, .. } => *created_at,
            PendingOp::SetFilterKeywords { created_at, .. } => *created_at,
        }
    }
}

/// Pending ops older than this are treated as abandoned (the user walked
/// away mid-rename); the stale prompt is cleaned up and the next message is
/// handled normally instead of being swallowed as a "new name".
pub const PENDING_OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// Per-host media download concurrency cap. Prevents hammering a single CDN
/// with many parallel connections (rate limiting / anti-crawl triggers).
pub const HOST_DOWNLOAD_CONCURRENCY: usize = 3;

/// Cap on distinct per-host semaphore buckets. Hosts come from
/// (attacker-controllable) media URLs, so the map must stay bounded.
const MAX_HOST_BUCKETS: usize = 512;

/// Make room for `host` in the per-host bucket map when it is at capacity.
///
/// Evicts only IDLE buckets first — a bucket is idle when all its permits are
/// available (`available_permits() == HOST_DOWNLOAD_CONCURRENCY`, i.e. nobody
/// currently holds one). The previous behaviour cleared the entire map on
/// overflow; in-flight downloads kept running on their `Arc` clones while the
/// next request to a busy host got a brand-new full-strength bucket, briefly
/// doubling that host's effective concurrency.
///
/// If the map is still full after idle eviction (every bucket busy — a rare
/// pathological case), evict a single entry. Keeping the map bounded matters
/// more than an isolated, brief doubling of one host, and the global download
/// semaphore still caps total concurrency. Returning an uncached transient
/// bucket instead was rejected: it would drop per-host limiting for a hot new
/// host, letting it open unbounded parallel connections to one CDN.
fn cap_host_buckets(map: &mut HashMap<String, Arc<Semaphore>>, host: &str) {
    if map.contains_key(host) || map.len() < MAX_HOST_BUCKETS {
        return;
    }
    map.retain(|k, sem| k == host || sem.available_permits() < HOST_DOWNLOAD_CONCURRENCY);
    if map.len() >= MAX_HOST_BUCKETS {
        // All buckets busy: evict one (arbitrary) busy bucket.
        if let Some(k) = map.keys().find(|k| *k != host).cloned() {
            map.remove(&k);
        }
    }
}

/// RAII guard that clears an AtomicBool flag on every exit path. Shared by the
/// fetch-cycle, outbox-retry and cleanup reentrancy guards (previously two
/// byte-identical local guard structs).
pub struct ActiveFlagGuard(pub Arc<AtomicBool>);

impl Drop for ActiveFlagGuard {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Shared application state passed to all handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Db,
    /// HTTP client with proxy (for RSS fetching).
    pub http: reqwest::Client,
    /// HTTP client without proxy (for no_proxy targets).
    pub http_direct: reqwest::Client,
    /// HTTP client with proxy that skips TLS certificate verification, used
    /// only for hosts listed in RSS_INSECURE_HOSTS (legacy/broken cert chains
    /// that rustls rejects but browsers tolerate).
    pub http_insecure: reqwest::Client,
    /// Media download client (via proxy, auto-redirect disabled — SSRF protection, see download_one).
    pub media: reqwest::Client,
    /// Media download client (direct connection, auto-redirect disabled).
    pub media_direct: reqwest::Client,
    /// Media download client (TLS verification disabled + auto-redirect disabled, whitelisted hosts only).
    pub media_insecure: reqwest::Client,
    /// Bot username (needed for command parsing in groups).
    pub bot_username: String,
    /// Limits concurrent RSS feed fetches.
    pub fetch_semaphore: Arc<Semaphore>,
    /// Limits concurrent media downloads.
    pub download_semaphore: Arc<Semaphore>,
    /// Per-host download concurrency limit: prevents opening too many
    /// simultaneous connections to a single CDN (e.g. weibocdn rate-limiting
    /// / anti-crawl). Keyed by lowercase hostname; created lazily.
    pub host_download_semaphores: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    /// True while a fetch cycle is running; prevents overlapping cycles
    /// (a slow cycle with media downloads can outlive the cron interval,
    /// and two concurrent cycles would push the same entries twice).
    pub fetch_cycle_active: Arc<AtomicBool>,
    /// True while an outbox retry round is running; prevents overlapping
    /// rounds (a slow round with large media uploads can outlive the 5-min
    /// cron interval, and two concurrent rounds would double-send entries).
    pub outbox_retry_active: Arc<AtomicBool>,
    /// True while a cleanup round is running; prevents overlapping rounds
    /// (a slow round scanning multi-GB media dirs can outlive the hourly
    /// cron interval; operations are mostly idempotent but overlap wastes IO).
    pub cleanup_active: Arc<AtomicBool>,
    /// In-flight multi-step flows keyed by (chat_id, user_id).
    pub pending_ops: Arc<Mutex<HashMap<(i64, i64), PendingOp>>>,
    /// Runtime-mutable platform login cookies, keyed by platform id.
    /// Global: admin configures once, all users share the same cookie.
    /// Persisted in the `settings` table (key = "cookie:<platform>").
    pub cookies: Arc<RwLock<HashMap<String, String>>>,
    /// Weibo visitor cookie cache (genvisitor/incarnate self-issued guest identity).
    /// Used by the desktop video API to fetch playback_list without login. TTL 1h.
    pub weibo_visitor_cookie: Arc<Mutex<Option<(String, std::time::Instant)>>>,
}

impl AppState {
    pub fn new(config: Config, db: Db, bot_username: String) -> anyhow::Result<Self> {
        // Build a client. follow_redirects=false is used for media downloads
        // (SSRF redirect protection: download_one follows redirects manually
        // hop by hop and re-runs is_private_url on each hop).
        // `ssrf_guard` installs the connection-layer DNS guard (ssrf.rs
        // GuardedResolver) on direct clients only: it re-checks every address
        // reqwest actually resolves, closing the DNS-rebinding TOCTOU window
        // between the pre-check and the connection. Proxy-routed clients get
        // false: the target lookup happens on the proxy side, so a local
        // resolver would never see it.
        fn build_client(
            proxy: Option<&str>,
            insecure: bool,
            follow_redirects: bool,
            ssrf_guard: bool,
        ) -> anyhow::Result<reqwest::Client> {
            let mut b = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .read_timeout(std::time::Duration::from_secs(30))
                .user_agent(BROWSER_UA);
            if insecure {
                b = b.danger_accept_invalid_certs(true);
            }
            if !follow_redirects {
                b = b.redirect(reqwest::redirect::Policy::none());
            }
            if ssrf_guard {
                b = b.dns_resolver(crate::ssrf::GuardedResolver);
            }
            if let Some(p) = proxy {
                b = b.proxy(reqwest::Proxy::all(p)?);
            }
            Ok(b.build()?)
        }

        let proxy = config.rss_proxy.as_deref();
        let has_proxy = proxy.is_some();

        // RSS/platform fetch clients: follow redirects (common with feedburner etc.).
        let http_direct = build_client(None, false, true, true)?;
        let http = if has_proxy { build_client(proxy, false, true, false)? } else { http_direct.clone() };
        // Insecure variant: same proxy routing, but TLS verification disabled.
        // Only selected for whitelisted hosts (see client_for_url).
        let http_insecure = build_client(proxy, true, true, !has_proxy)?;

        // Media download clients: auto-redirect disabled (SSRF protection, see download_one).
        let media_direct = build_client(None, false, false, true)?;
        let media = if has_proxy { build_client(proxy, false, false, false)? } else { media_direct.clone() };
        let media_insecure = build_client(proxy, true, false, !has_proxy)?;

        Ok(Self {
            fetch_semaphore: Arc::new(Semaphore::new(config.fetch_concurrency)),
            download_semaphore: Arc::new(Semaphore::new(config.download_concurrency)),
            host_download_semaphores: Arc::new(Mutex::new(HashMap::new())),
            fetch_cycle_active: Arc::new(AtomicBool::new(false)),
            outbox_retry_active: Arc::new(AtomicBool::new(false)),
            cleanup_active: Arc::new(AtomicBool::new(false)),
            cookies: Arc::new(RwLock::new(HashMap::new())),
            weibo_visitor_cookie: Arc::new(Mutex::new(None)),
            config: Arc::new(config),
            db,
            bot_username,
            http,
            http_direct,
            http_insecure,
            media,
            media_direct,
            media_insecure,
            pending_ops: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Get (or lazily create) the per-host download semaphore for a media URL.
    /// Falls back to a shared "default" bucket when the URL has no host.
    /// The map is capped (see cap_host_buckets): hosts come from
    /// (attacker-controllable) media URLs, so an unbounded map could grow
    /// without limit.
    pub async fn host_download_semaphore(&self, url: &str) -> Arc<Semaphore> {
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
            .unwrap_or_else(|| "default".to_string());
        let mut m = self.host_download_semaphores.lock().await;
        cap_host_buckets(&mut m, &host);
        m.entry(host)
            .or_insert_with(|| Arc::new(Semaphore::new(HOST_DOWNLOAD_CONCURRENCY)))
            .clone()
    }

    /// Get the global login cookie for a platform (e.g. "weibo"). None if unset.
    /// Admin-configured; shared by all users.
    pub async fn get_cookie(&self, platform: &str) -> Option<String> {
        self.cookies.read().await.get(platform).cloned()
    }

    /// Set a platform's global login cookie at runtime (in-memory only;
    /// persistence is the caller's responsibility via db::set_setting).
    pub async fn set_cookie(&self, platform: &str, value: &str) {
        let mut w = self.cookies.write().await;
        if value.trim().is_empty() {
            w.remove(platform);
        } else {
            w.insert(platform.to_string(), value.trim().to_string());
        }
    }

    /// Client routing decision: based on the insecure whitelist / no_proxy
    /// list, decides which class of client the target URL should use and
    /// whether the actual connection is made through the proxy. Both values
    /// come from the same decision so callers cannot mix them (previously the
    /// client was chosen from the original URL while the SSRF skip used the
    /// redirect target's routing).
    fn route_kind(&self, url: &str) -> (RouteKind, bool) {
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
            .unwrap_or_default();

        // Whitelisted broken-cert hosts: skip TLS verification (scoped!).
        // The insecure client is built with the proxy whenever RSS_PROXY is
        // set (even for a host also listed in RSS_NO_PROXY), so via_proxy
        // follows that construction.
        if crate::config::host_in_list(&host, &self.config.rss_insecure_hosts) {
            return (RouteKind::Insecure, self.config.rss_proxy.is_some());
        }

        if self.config.rss_proxy.is_none() {
            return (RouteKind::Direct, false);
        }
        if crate::config::host_in_list(&host, &self.config.rss_no_proxy) {
            (RouteKind::Direct, false)
        } else {
            (RouteKind::Proxy, true)
        }
    }

    /// Choose the appropriate HTTP client based on no_proxy rules and the
    /// insecure-host whitelist.
    pub fn client_for_url(&self, url: &str) -> &reqwest::Client {
        match self.route_kind(url).0 {
            RouteKind::Insecure => &self.http_insecure,
            RouteKind::Direct => &self.http_direct,
            RouteKind::Proxy => &self.http,
        }
    }

    /// Media-download specific client selection: same routing rules as
    /// client_for_url (no_proxy / insecure whitelist), but returns a client
    /// with auto-redirect disabled (SSRF protection: download_one follows
    /// redirects manually hop by hop and re-runs the SSRF check on each hop).
    pub fn media_client_for_url(&self, url: &str) -> &reqwest::Client {
        self.media_route_for_url(url).0
    }

    /// Like media_client_for_url, but also reports whether that client routes
    /// the connection through the proxy. Used for per-hop redirect checks.
    pub fn media_route_for_url(&self, url: &str) -> (&reqwest::Client, bool) {
        let (kind, via_proxy) = self.route_kind(url);
        let client = match kind {
            RouteKind::Insecure => &self.media_insecure,
            RouteKind::Direct => &self.media_direct,
            RouteKind::Proxy => &self.media,
        };
        (client, via_proxy)
    }

    /// Whether this URL's connection will be made via the RSS_PROXY proxy
    /// (rather than a direct local connection). The SSRF local DNS check only
    /// makes sense for direct connections: when going through the proxy, the
    /// actual connection is made on the proxy side, so the local DNS
    /// resolution result does not affect the real connection target
    /// (v1.10.14: fixed false blocks caused by local DNS resolving CDN
    /// domains to private addresses).
    pub fn is_routed_via_proxy(&self, url: &str) -> bool {
        self.route_kind(url).1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket() -> Arc<Semaphore> {
        Arc::new(Semaphore::new(HOST_DOWNLOAD_CONCURRENCY))
    }

    /// Fill `map` with `n` buckets named host-0..host-n. When `busy` is true,
    /// hold one permit per bucket (a leaked guard) so it reads as in-use.
    fn fill(map: &mut HashMap<String, Arc<Semaphore>>, n: usize, busy: bool) {
        for i in 0..n {
            let sem = bucket();
            if busy {
                // Leak the guard: the bucket stays busy for the whole test.
                let permit = sem.try_acquire().unwrap();
                std::mem::forget(permit);
            }
            map.insert(format!("host-{}", i), sem);
        }
    }

    #[test]
    fn cap_below_limit_is_noop() {
        let mut m = HashMap::new();
        fill(&mut m, 4, false);
        cap_host_buckets(&mut m, "new-host");
        assert_eq!(m.len(), 4, "below-cap calls must not evict");
    }

    #[test]
    fn cap_evicts_only_idle_buckets() {
        let mut m = HashMap::new();
        // Half busy, half idle, at capacity.
        for i in 0..MAX_HOST_BUCKETS / 2 {
            let sem = bucket();
            let permit = sem.try_acquire().unwrap();
            std::mem::forget(permit);
            m.insert(format!("busy-{}", i), sem);
        }
        fill(&mut m, MAX_HOST_BUCKETS / 2, false);
        // Rename idle keys distinctly (fill reuses host-N names; ensure count).
        assert_eq!(m.len(), MAX_HOST_BUCKETS);
        cap_host_buckets(&mut m, "new-host");
        // All idle buckets gone, all busy buckets kept, room for the new host.
        assert_eq!(m.len(), MAX_HOST_BUCKETS / 2);
        assert!(m.values().all(|s| s.available_permits() < HOST_DOWNLOAD_CONCURRENCY));
        assert!(m.len() < MAX_HOST_BUCKETS);
    }

    #[test]
    fn cap_all_busy_evicts_exactly_one() {
        let mut m = HashMap::new();
        fill(&mut m, MAX_HOST_BUCKETS, true);
        cap_host_buckets(&mut m, "new-host");
        // Still bounded, one bucket sacrificed — never cleared wholesale.
        assert_eq!(m.len(), MAX_HOST_BUCKETS - 1);
    }

    #[test]
    fn cap_existing_host_never_evicts() {
        let mut m = HashMap::new();
        fill(&mut m, MAX_HOST_BUCKETS, false);
        cap_host_buckets(&mut m, "host-0");
        assert_eq!(m.len(), MAX_HOST_BUCKETS, "requests to cached hosts must not evict");
    }
}
