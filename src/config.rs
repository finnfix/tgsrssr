use std::path::PathBuf;

/// Application configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    // Bot
    pub token: String,
    pub bot_api_url: Option<String>,
    pub allowed_user_ids: Vec<i64>,
    /// The single admin user ID: can manage cookies, fetch logs, and receive
    /// platform health notifications. Implied in allowed_user_ids. Exactly one
    /// is required — parse_admin_id rejects 0 or multiple values at startup.
    pub admin_user_id: i64,
    /// When true, any Telegram user can use the bot (open registration).
    /// When false (default), only admins + allowed_user_ids can interact.
    pub open_to_all: bool,

    // Database
    pub db_path: PathBuf,

    // Media
    pub media_dir: PathBuf,
    pub outbound_ttl_hours: u64,
    pub media_max_size_gb: u64,
    pub long_image_aspect_ratio: f64,
    /// Videos larger than this (MB) are not pushed; the entry falls back to
    /// text and the chat owner gets a dedicated notice.
    pub max_video_push_mb: u64,

    // Proxy
    pub rss_proxy: Option<String>,
    pub rss_no_proxy: Vec<String>,
    /// Hosts whose TLS certificate chain is rejected by rustls (e.g. legacy
    /// X.509 versions or intercepting CAs). For these hosts only, certificate
    /// verification is skipped. Empty by default = strict verification everywhere.
    pub rss_insecure_hosts: Vec<String>,

    // Platform login cookies are stored globally in the DB settings table
    // (key = "cookie:<platform>"); configured by admins via /cookies.

    // Media transport: where the Bot API server sees our media directory.
    // Empty => official API or unknown mount: upload via memory.
    // Set => local Bot API with a shared mount: send via file:// zero-copy.
    pub bot_api_media_dir: String,

    // Image relay
    pub img_relay_enabled: bool,
    pub img_relay_base_url: String,
    pub img_relay_domains: Vec<String>,

    // Cleanup
    pub sent_entries_retention_days: i64,
    pub cleanup_interval_minutes: u64,
    pub log_retention_days: i64,
    /// Daily DB backups kept this many days.
    pub backup_retention_days: i64,
    /// Retention days for the file_id upload dedup cache (expired entries are
    /// removed by the cleanup task).
    pub file_id_cache_retention_days: i64,
    /// Row-count cap for the sent-messages registry (oldest rows are deleted
    /// when exceeded).
    pub sent_messages_max_rows: i64,

    // Scheduler
    pub scheduler_interval_minutes: u64,
    pub fetch_concurrency: usize,
    pub download_concurrency: usize,

    // Error handling
    pub max_fetch_errors: i64,

    // Logging
    pub log_dir: PathBuf,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let token = std::env::var("TELOXIDE_TOKEN")
            .map_err(|_| anyhow::anyhow!("TELOXIDE_TOKEN is required"))?;

        let bot_api_url = std::env::var("BOT_API_URL").ok().filter(|s| !s.is_empty());

        let allowed_user_ids = parse_id_list("ALLOWED_USER_IDS");
        // Exactly one admin is required. Missing (unset/empty/all-invalid) or
        // more than one is a fatal misconfiguration: cookie/log management,
        // command-menu elevation and platform alerts all key off this one user.
        let admin_user_id = validate_single_admin(parse_id_list("ADMIN_USER_IDS"))?;
        // Empty whitelist is fine now that the admin is always present and
        // implicitly allowed (is_allowed checks admin_user_id).
        let open_to_all = env_parse_or("OPEN_TO_ALL", false);

        let db_path = PathBuf::from(env_or("DB_PATH", "data/rss_bot.db"));
        let media_dir = PathBuf::from(env_or("MEDIA_DIR", "media"));
        let log_dir = PathBuf::from(env_or("LOG_DIR", "logs"));
        // Path where the Bot API server sees our media dir (shared mount).
        // Default matches telegram-bot-api compose mount; empty disables file://.
        let bot_api_media_dir = env_or("BOT_API_MEDIA_DIR", "/var/lib/telegram-bot-api-media");

        let rss_proxy = std::env::var("RSS_PROXY").ok().filter(|s| !s.is_empty());
        let rss_no_proxy = std::env::var("RSS_NO_PROXY")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let rss_insecure_hosts = std::env::var("RSS_INSECURE_HOSTS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().trim_start_matches('.').to_lowercase().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let img_relay_domains = std::env::var("IMG_RELAY_DOMAINS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().trim_start_matches('.').to_lowercase().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        Ok(Self {
            token,
            bot_api_url,
            allowed_user_ids,
            admin_user_id,
            open_to_all,
            db_path,
            media_dir,
            log_dir,
            bot_api_media_dir,
            outbound_ttl_hours: env_parse_or("MEDIA_OUTBOUND_TTL_HOURS", 80).max(1),
            media_max_size_gb: env_parse_or("MEDIA_MAX_SIZE_GB", 10).max(1),
            long_image_aspect_ratio: env_parse_or("LONG_IMAGE_ASPECT_RATIO", 8.0),
            // Floor of 1: MAX_VIDEO_PUSH_MB=0 would make cap=min(transport,0)=0
            // and skip every media item.
            max_video_push_mb: env_parse_or("MAX_VIDEO_PUSH_MB", 100).max(1),
            rss_proxy,
            rss_no_proxy,
            rss_insecure_hosts,
            img_relay_enabled: env_parse_or("IMG_RELAY_ENABLED", true),
            img_relay_base_url: env_or("IMG_RELAY_BASE_URL", ""),
            img_relay_domains,
            // Retention-day floor of 1: a misconfigured negative/0 value would
            // make the datetime('-N days') modifier invalid (cleanup silently
            // stops working and the table grows unboundedly) or wrap the ttl
            // around to a huge value (never cleaned up).
            sent_entries_retention_days: env_parse_or("SENT_ENTRIES_RETENTION_DAYS", 180).max(1),
            cleanup_interval_minutes: env_parse_or("CLEANUP_INTERVAL_MINUTES", 60),
            log_retention_days: env_parse_or("LOG_RETENTION_DAYS", 30).max(1),
            backup_retention_days: env_parse_or("BACKUP_RETENTION_DAYS", 7).max(1),
            file_id_cache_retention_days: env_parse_or("FILE_ID_CACHE_RETENTION_DAYS", 30).max(1),
            sent_messages_max_rows: env_parse_or("SENT_MESSAGES_MAX_ROWS", 5000).max(1),
            scheduler_interval_minutes: env_parse_or("SCHEDULER_INTERVAL_MINUTES", 5),
            // Concurrency floor of 1: a misconfigured 0 would make
            // Semaphore(0)'s acquire never succeed, silently hanging fetches.
            fetch_concurrency: env_parse_or("FETCH_CONCURRENCY", 8).max(1),
            download_concurrency: env_parse_or("DOWNLOAD_CONCURRENCY", 4).max(1),
            // Floor of 1: MAX_FETCH_ERRORS=0 makes the `count >= max` check in
            // the scheduler true on the very first failure, auto-disabling a
            // source after a single transient error.
            max_fetch_errors: env_parse_or("MAX_FETCH_ERRORS", 20).max(1),
        })
    }

    pub fn media_max_bytes(&self) -> u64 {
        self.media_max_size_gb * 1024 * 1024 * 1024
    }

    /// Whether a user may interact with the bot at all.
    /// OPEN_TO_ALL=true: everyone. Otherwise: the admin + ALLOWED_USER_IDS.
    pub fn is_allowed(&self, user_id: i64) -> bool {
        self.open_to_all
            || self.admin_user_id == user_id
            || self.allowed_user_ids.contains(&user_id)
    }

    /// Whether a user is the admin (cookies / loget / platform alerts).
    pub fn is_admin(&self, user_id: i64) -> bool {
        self.admin_user_id == user_id
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_parse_or<T: std::str::FromStr + Copy>(key: &str, default: T) -> T {
    match std::env::var(key) {
        Err(_) => default,
        Ok(v) => match v.parse() {
            Ok(x) => x,
            Err(_) => {
                // Silently falling back to the default on an invalid value is a
                // debugging trap (e.g. FETCH_CONCURRENCY=abc), so leave a trace.
                // Logging isn't initialized yet, so use eprintln.
                eprintln!("WARNING: {} has invalid value '{}', using default", key, v);
                default
            }
        },
    }
}

fn parse_id_list(key: &str) -> Vec<i64> {
    let raw = std::env::var(key).unwrap_or_default();
    let mut ids = Vec::new();
    for s in raw.split(',') {
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        match s.parse::<i64>() {
            Ok(id) => ids.push(id),
            Err(_) => eprintln!("WARNING: {} contains invalid id '{}', skipped", key, s),
        }
    }
    ids
}

/// The admin list must resolve to exactly one user ID. Split out from
/// `from_env` so it is unit-testable without touching process env.
fn validate_single_admin(ids: Vec<i64>) -> anyhow::Result<i64> {
    match ids.len() {
        1 => Ok(ids[0]),
        0 => Err(anyhow::anyhow!(
            "ADMIN_USER_IDS is required and must contain exactly one user ID"
        )),
        n => Err(anyhow::anyhow!(
            "ADMIN_USER_IDS must contain exactly one user ID, got {}",
            n
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::validate_single_admin;

    #[test]
    fn single_admin_accepted() {
        assert_eq!(validate_single_admin(vec![42]).unwrap(), 42);
    }

    #[test]
    fn empty_admin_rejected() {
        let err = validate_single_admin(vec![]).unwrap_err().to_string();
        assert!(err.contains("required"), "unexpected message: {}", err);
    }

    #[test]
    fn multiple_admins_rejected() {
        let err = validate_single_admin(vec![1, 2]).unwrap_err().to_string();
        assert!(err.contains("exactly one"), "unexpected message: {}", err);
        assert!(err.contains("got 2"), "unexpected message: {}", err);
    }
}
