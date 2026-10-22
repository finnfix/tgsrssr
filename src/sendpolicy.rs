//! Send policy: per-chat pacing, flood-control backoff, and retryable-error
//! classification for Telegram sends.
//!
//! Extracted from push.rs (batch-B2 architecture refactor) so the
//! rate-limiting / retry-strategy concern lives apart from message rendering
//! and media orchestration. Pure policy — no rendering, no DB.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// Retry seconds extracted from a Telegram flood control error.
/// Case-insensitive: teloxide/Telegram Display text has been observed with
/// both "Retry after N" and lowercase "retry after N" spellings across
/// versions; anchoring to one case silently disables flood backoff when the
/// wording drifts (v1.11.2 #25 typed the "message is not modified" check;
/// this hardens the flood path the same way is not possible without a typed
/// variant, so at least match case-insensitively).
static RE_RETRY_AFTER: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)Retry after (\d+)").unwrap());

/// Extract the flood-control backoff seconds demanded by Telegram from an
/// error string (capped at 60s). Returns None for non-flood errors. Shared
/// by both the text and media send paths.
pub fn flood_retry_secs(err_str: &str) -> Option<u64> {
    RE_RETRY_AFTER
        .captures(err_str)
        .map(|caps| caps[1].parse().unwrap_or(5).min(60))
}

/// Flood-control backoff for media sends: if the error is a 429, sleep for
/// the number of seconds Telegram demands and return true (caller should
/// retry); returns false for non-flood errors or when retries are exhausted
/// (caller should return the error). `tries` is capped at 2 to avoid an
/// infinite loop in edge cases.
pub async fn flood_backoff(err_str: &str, tries: &mut u32, chat_id: i64) -> bool {
    if *tries >= 2 {
        return false;
    }
    if let Some(secs) = flood_retry_secs(err_str) {
        *tries += 1;
        tracing::warn!(chat_id = chat_id, secs = secs, "media send flood control, backing off");
        tokio::time::sleep(std::time::Duration::from_secs(secs + 1)).await;
        return true;
    }
    false
}

/// Only retry on connection-class errors that are definitely undelivered;
/// timeouts are not retried — on timeout the server may have already received
/// and started processing the request, and resending that op would duplicate
/// the media group.
/// (#14 fix: extracted as a single module-level function, deduplicating the
/// two original closures.)
/// pub: the scheduler's transient RSS-fetch retry (scheduler.rs `process_feed`,
/// the v1.10.3 debounce path) reuses this same criterion via
/// `sendpolicy::is_net_error`, so fetch and send classify connection errors
/// identically.
pub fn is_net_error(err_str: &str) -> bool {
    // Two reqwest-flavoured prefixes plus the pre-send DNS check from
    // fetcher.rs (entry + redirect hops): "dns resolve failed: ..." is
    // emitted when tokio's lookup_host cannot confirm the target. Matching it
    // here lets the scheduler's one-shot retry absorb a transient DNS blip
    // instead of counting it on the first round; a *sustained* DNS outage
    // still escalates (the retry fails too and the error is counted), which is
    // correct — repeated unreachability should eventually disable the source.
    (err_str.contains("error sending request")
        || err_str.contains("connection")
        || err_str.contains("dns resolve failed"))
        && !err_str.contains("timeout")
        && !err_str.contains("timed out")
}

/// Transient internal errors on the Telegram server side (added in v1.10.2).
/// Typical scenario: during a file upload the TG backend returns 400
/// "internal Server Error during file upload" — the HTTP status is 400 but
/// the description clearly indicates a server-side internal exception rather
/// than a request-parameter problem, and it usually recovers on its own
/// within seconds to minutes.
/// The matched substrings never appear in parameter-class 400 errors (such
/// as "file is too big" or "wrong type of the web-page content"), so the
/// false-positive risk is extremely low.
fn is_tg_transient_error(err_str: &str) -> bool {
    err_str.contains("internal Server Error")
        || err_str.contains("Internal Server Error")
}

/// Unified retryable-error determination (v1.10.2): connection-class errors
/// and transient TG server-side errors. All send retry points (rich ops /
/// text chunks / documents) use this function uniformly to keep retry
/// criteria consistent.
pub fn is_retryable_error(err_str: &str) -> bool {
    is_net_error(err_str) || is_tg_transient_error(err_str)
}

/// Bounded exponential backoff sequence (seconds) for network errors
/// (proxy flaps / upstream outages) and transient TG errors. Shared by
/// media-op-level and text-chunk-level retries (#14 fix: merged the two
/// identical constants NET_BACKOFF_SECS and CHUNK_BACKOFF_SECS).
pub const SEND_BACKOFF_SECS: [u64; 3] = [2, 5, 10];

/// Per-chat send rate limiting: when multiple sources push to the same chat
/// in parallel, without pacing they all hit Telegram's per-chat rate limit
/// (~20 messages/minute) at once, triggering passive flood-control backoff.
/// Sends to the same chat are queued centrally here: two adjacent messages
/// are spaced at least MIN_INTERVAL apart.
struct ChatPacer {
    last: tokio::sync::Mutex<HashMap<i64, Instant>>,
}

impl ChatPacer {
    const MIN_INTERVAL: Duration = Duration::from_millis(2000);

    async fn wait(&self, chat_id: i64) {
        loop {
            let delay = {
                let mut m = self.last.lock().await;
                let now = Instant::now();
                match m.get(&chat_id).copied() {
                    Some(t) if t + Self::MIN_INTERVAL > now => {
                        Some(t + Self::MIN_INTERVAL - now)
                    }
                    _ => {
                        m.insert(chat_id, now);
                        None
                    }
                }
            };
            match delay {
                Some(d) => tokio::time::sleep(d).await,
                None => return,
            }
        }
    }
}

static PACER: std::sync::OnceLock<ChatPacer> = std::sync::OnceLock::new();

/// Queue before sending: guarantees adjacent messages to the same chat are
/// spaced at least 2 seconds apart.
pub async fn pace(chat_id: i64) {
    PACER
        .get_or_init(|| ChatPacer {
            last: tokio::sync::Mutex::new(HashMap::new()),
        })
        .wait(chat_id)
        .await
}
