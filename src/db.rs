use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

pub type Db = SqlitePool;

/// Unified constructor for settings-table key names.
///
/// Previously the various runtime keys (cookie / cookie_expired_notified /
/// platform_health / platform_streak / media_dl_fail) were hand-assembled with
/// `format!` scattered across scheduler / callbacks / platforms — a single
/// mistyped character produced no error and silently disabled the key.
/// Centralizing them here means the key formats have a single definition,
/// eliminating cross-file inconsistencies.
///
/// Since v1.11.0 the cookie and platform-health keys are global (no user_id
/// segment): cookies are configured once by admins and shared by all users;
/// platform health state was globalized accordingly. The old per-user keys
/// are cleaned up at startup by migration logic in main.rs.
pub mod setting_key {
    /// Global platform login cookie: `cookie:<platform>`
    pub fn cookie(platform: &str) -> String {
        format!("cookie:{}", platform)
    }
    /// Cookie-expired notification flag: `cookie_expired_notified:<platform>`
    pub fn cookie_expired_notified(platform: &str) -> String {
        format!("cookie_expired_notified:{}", platform)
    }
    /// Platform health state (ok/down): `platform_health:<platform>`
    pub fn platform_health(platform: &str) -> String {
        format!("platform_health:{}", platform)
    }
    /// Platform health hysteresis streak counter: `platform_streak:<platform>`
    pub fn platform_streak(platform: &str) -> String {
        format!("platform_streak:{}", platform)
    }
    /// Media-only mode download-failure retry counter: `media_dl_fail:<chat_id>:<feed_url>:<entry_id>`
    pub fn media_dl_fail(chat_id: &str, feed_url: &str, entry_id: &str) -> String {
        format!("media_dl_fail:{}:{}:{}", chat_id, feed_url, entry_id)
    }
    /// Per-feed auto-disable notification flag: `auto_disable_notified:<chat_id>:<feed_url>`
    /// Used for edge deduplication — when the same feed is disabled, later
    /// self-healed, and then fails again, no duplicate notice is sent (#9 fix).
    pub fn auto_disable_notified(chat_id: &str, feed_url: &str) -> String {
        format!("auto_disable_notified:{}:{}", chat_id, feed_url)
    }
    /// Consecutive-round counter of successful full-body fetches (HTTP 200,
    /// not 304) that parsed zero entries: `stale_zero:<chat_id>:<feed_url>`.
    /// Structure-independent silence detector: the per-platform zero-entry
    /// warns need the platform response structure to still be recognized; if
    /// a redesign makes the container keys vanish, the parser returns "valid
    /// empty" and those warns never fire. This counter catches that case after
    /// a sustained streak (warn log only, no user notification).
    pub fn stale_zero(chat_id: &str, feed_url: &str) -> String {
        format!("stale_zero:{}:{}", chat_id, feed_url)
    }
    /// One-shot owner notice flag for media that expired while an outbox row
    /// was waiting for retry: `outbox_media_lost:<outbox_id>`.
    /// Deleted on success/abandon; cleanup_stale_settings removes orphans.
    pub fn outbox_media_lost(outbox_id: i64) -> String {
        format!("outbox_media_lost:{}", outbox_id)
    }
}

/// Default config values (single source of truth on the Rust side).
/// The SQL COALESCE literals must stay in sync with these (sqlx 0.9 forbids
/// dynamic SQL so they cannot be injected via format!; consistency is asserted
/// by test_default_constants_match_sql below).
pub const DEFAULT_INTERVAL_MINUTES: i64 = 30;
pub const DEFAULT_CONTENT_MODE: &str = "title_link";
pub const DEFAULT_SOURCE_DISPLAY: &str = "source";
pub const DEFAULT_CAPTION_ABOVE_MEDIA: i64 = 0;
pub const DEFAULT_LINK_PREVIEW: i64 = 1;

/// Row types for queries.
// Some fields (chat_type/enabled etc.) only appear in SQL
// WHERE/COALESCE and are never read on the Rust side — kept to preserve the
// complete mapping of the database schema. (Exception: added_by is read on
// the Rust side by get_chat_owner / can_manage_chat.)
#[allow(dead_code)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChatRow {
    pub chat_id: String,
    pub chat_type: String,
    pub title: Option<String>,
    pub username: Option<String>,
    pub added_by: Option<String>,
    pub enabled: i64,
}

// error_count/last_fetch_at/last_error/auto_disabled/disabled_at are read and
// written by SQL (due checks / error counting / self-heal cooldown) and never
// read directly on the Rust side; kept for a complete schema mapping.
#[allow(dead_code)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SubRow {
    pub id: i64,
    pub chat_id: String,
    pub feed_url: String,
    pub feed_title: Option<String>,
    pub title_overridden: i64,
    pub original_title: Option<String>,
    pub active: i64,
    pub error_count: i64,
    pub last_fetch_at: Option<String>,
    pub last_error: Option<String>,
    /// HTTP conditional-request negotiation headers (server replies 304 when
    /// the source has not changed, saving a fetch)
    pub http_etag: Option<String>,
    pub http_last_modified: Option<String>,
    /// 1 = system auto-disabled after consecutive failures (may auto-recover
    /// after the cooldown); 0 = manually paused (never auto-recovers)
    pub auto_disabled: i64,
    /// Moment of auto-disable (UTC); the cooldown is measured from this point
    pub disabled_at: Option<String>,
}

// chat_id/caption_above_media are not read on the Rust side (the caption
// position is consumed via the COALESCE value in EffectiveConfig); kept for a
// complete schema mapping.
#[allow(dead_code)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChatConfigRow {
    pub chat_id: String,
    pub interval_minutes: i64,
    pub content_mode: String,
    pub source_display: String,
    pub caption_above_media: i64,
    pub link_preview: i64,
}

/// Effective config for a subscription (after COALESCE).
#[allow(dead_code)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EffectiveConfig {
    pub chat_id: String,
    pub feed_url: String,
    pub interval_minutes: i64,
    pub content_mode: String,
    pub source_display: String,
    pub caption_above_media: bool,
    pub link_preview: bool,
    pub iv_overridden: bool,
    pub cm_overridden: bool,
    pub sd_overridden: bool,
    pub lp_overridden: bool,
}

pub async fn connect(db_path: &Path) -> anyhow::Result<Db> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let options = SqliteConnectOptions::from_str(&db_path.to_string_lossy())?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
        .pragma("journal_size_limit", "52428800");

    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;

    // Run migrations
    sqlx::migrate!("./migrations").run(&pool).await?;

    Ok(pool)
}

// ===== Settings (key-value store for runtime-mutable config) =====

pub async fn get_setting(db: &Db, key: &str) -> anyhow::Result<Option<String>> {
    let row = sqlx::query("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(db)
        .await?;
    Ok(row.map(|r| r.get::<String, _>("value")))
}

pub async fn set_setting(db: &Db, key: &str, value: &str) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, datetime('now'))
           ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = datetime('now')"#,
    )
    .bind(key)
    .bind(value)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn delete_setting(db: &Db, key: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM settings WHERE key = ?")
        .bind(key)
        .execute(db)
        .await?;
    Ok(())
}

/// Atomically flip a state bit in settings: writes `to` only when the current
/// value != `to`. Returns true when this call performed the flip (the caller
/// sends an edge notification based on it). Used for platform health state
/// (ok/down): when multiple sources fail concurrently only the first flip
/// succeeds, eliminating duplicate notifications (a read-check-write sequence
/// has a race window under concurrency).
pub async fn flip_setting_once(db: &Db, key: &str, to: &str) -> anyhow::Result<bool> {
    let res = sqlx::query(
        r#"INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, datetime('now'))
           ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = datetime('now')
           WHERE settings.value != excluded.value"#,
    )
    .bind(key)
    .bind(to)
    .execute(db)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Get all settings whose key starts with the given prefix.
/// Returns a Result: a transient DB error must not be treated as "no data"
/// (consistent with the project's fail-closed principle); the caller decides
/// explicitly (startup cleanup / cookie-loading scenarios log a warn and skip).
pub async fn get_settings_by_prefix(db: &Db, prefix: &str) -> anyhow::Result<Vec<(String, String)>> {
    let rows = sqlx::query("SELECT key, value FROM settings WHERE key LIKE ? || '%'")
        .bind(prefix)
        .fetch_all(db)
        .await?;
    Ok(rows.iter()
        .map(|r| (r.get::<String, _>("key"), r.get::<String, _>("value")))
        .collect())
}

/// Get the owner (added_by) of a chat.
/// Ok(None) = chat does not exist or has no owner; Err = DB query failed
/// (callers must log it and must not silently treat it as "no owner" —
/// otherwise owner notifications would be silently lost).
pub async fn get_chat_owner(db: &Db, chat_id: &str) -> anyhow::Result<Option<i64>> {
    Ok(get_chat(db, chat_id)
        .await?
        .and_then(|c| c.added_by)
        .and_then(|s| s.parse::<i64>().ok()))
}

// ===== Chats =====

/// Classify a Telegram chat for the DB chat_type column.
/// Accepts the three predicates so it works with both `Chat` and `ChatFullInfo`.
/// Defined in db.rs (batch A3: breaks the callbacks→commands reverse
/// dependency edge); commands keeps a re-export to preserve existing call paths.
pub fn chat_type_str(is_channel: bool, is_group: bool, is_supergroup: bool) -> &'static str {
    if is_channel {
        "channel"
    } else if is_group || is_supergroup {
        "group"
    } else {
        "private"
    }
}

pub async fn upsert_chat(
    db: &Db,
    chat_id: &str,
    chat_type: &str,
    title: Option<&str>,
    username: Option<&str>,
    added_by: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO chats (chat_id, chat_type, title, username, added_by, enabled, last_seen_at)
           VALUES (?1, ?2, ?3, ?4, ?5, 1, datetime('now'))
           ON CONFLICT(chat_id) DO UPDATE SET
             chat_type = excluded.chat_type,
             title = COALESCE(excluded.title, chats.title),
             username = COALESCE(excluded.username, chats.username),
             added_by = COALESCE(chats.added_by, excluded.added_by),
             enabled = 1,
             last_seen_at = datetime('now'),
             updated_at = datetime('now')"#,
    )
    .bind(chat_id)
    .bind(chat_type)
    .bind(title)
    .bind(username)
    .bind(added_by)
    .execute(db)
    .await?;
    Ok(())
}

/// Delete all data for a chat (subscriptions, configs, sent_entries, chat itself).
/// Wrapped in a transaction: any step failing rolls everything back, avoiding
/// half-deleted residual state.
pub async fn purge_chat(db: &Db, chat_id: &str) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM sent_entries WHERE chat_id = ?").bind(chat_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM sent_messages WHERE chat_id = ?").bind(chat_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM outbox WHERE chat_id = ?").bind(chat_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM feed_configs WHERE chat_id = ?").bind(chat_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM subscriptions WHERE chat_id = ?").bind(chat_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM chat_configs WHERE chat_id = ?").bind(chat_id).execute(&mut *tx).await?;
    // Cascade-clean the media_dl_fail retry keys (prevents settings from
    // slowly leaking after purge).
    sqlx::query("DELETE FROM settings WHERE key LIKE ?")
        .bind(format!("media_dl_fail:{}:%", chat_id))
        .execute(&mut *tx)
        .await?;
    // Cascade-clean the auto_disable_notified edge-dedup flag (v1.9.7 fix:
    // purge_chat previously missed this key, inconsistent with
    // delete_subscription/delete_all_subscriptions).
    sqlx::query("DELETE FROM settings WHERE key LIKE ?")
        .bind(format!("auto_disable_notified:{}:%", chat_id))
        .execute(&mut *tx)
        .await?;
    // Cascade-clean the stale-zero streak counters.
    sqlx::query("DELETE FROM settings WHERE key LIKE ?")
        .bind(format!("stale_zero:{}:%", chat_id))
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM chats WHERE chat_id = ?").bind(chat_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn get_chat(db: &Db, chat_id: &str) -> anyhow::Result<Option<ChatRow>> {
    let row = sqlx::query_as::<_, ChatRow>(
        "SELECT chat_id, chat_type, title, username, added_by, enabled FROM chats WHERE chat_id = ?",
    )
    .bind(chat_id)
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// Whether `user_id` can manage `chat_id`. No admin bypass: data always belongs
/// to the individual who added the bot. Unregistered chats → false.
pub async fn can_manage_chat(db: &Db, user_id: i64, chat_id: &str) -> anyhow::Result<bool> {
    let Some(chat) = get_chat(db, chat_id).await? else {
        return Ok(false);
    };
    Ok(chat.added_by.as_deref() == Some(&user_id.to_string()))
}

/// Count subscriptions per chat (for the channel list page display).
pub async fn count_subscriptions_per_chat(db: &Db) -> anyhow::Result<std::collections::HashMap<String, i64>> {
    let rows = sqlx::query("SELECT chat_id, COUNT(*) AS cnt FROM subscriptions GROUP BY chat_id")
        .fetch_all(db)
        .await?;
    let mut map = std::collections::HashMap::new();
    for r in rows {
        map.insert(r.get::<String, _>("chat_id"), r.get::<i64, _>("cnt"));
    }
    Ok(map)
}

pub async fn list_chats(db: &Db, user_id: i64) -> anyhow::Result<Vec<ChatRow>> {
    // Pure ownership isolation: everyone (including admins) only sees the
    // channels whose added_by is themselves.
    let rows = sqlx::query_as::<_, ChatRow>(
        "SELECT chat_id, chat_type, title, username, added_by, enabled FROM chats WHERE enabled = 1 AND added_by = ? ORDER BY title",
    )
    .bind(user_id.to_string())
    .fetch_all(db)
    .await?;
    Ok(rows)
}

// ===== Subscriptions =====

/// Insert (or re-activate) a subscription and mark its current entries as
/// sent in one transaction. /sub uses this so a failure can neither leave a
/// partially-marked subscription nor roll back a pre-existing (re-enabled)
/// row: the previous two-step add + mark + delete-on-error flow deleted the
/// user's original subscription when the mark step failed.
pub async fn add_subscription_with_sent(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    feed_title: Option<&str>,
    entry_ids: &[String],
) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query(
        r#"INSERT INTO subscriptions (chat_id, feed_url, feed_title, title_overridden, active, error_count)
           VALUES (?1, ?2, ?3, 0, 1, 0)
           ON CONFLICT(chat_id, feed_url) DO UPDATE SET active = 1, error_count = 0, last_error = NULL,
           auto_disabled = 0, disabled_at = NULL,
           feed_title = CASE WHEN title_overridden = 1 THEN feed_title ELSE excluded.feed_title END"#,
    )
    .bind(chat_id)
    .bind(feed_url)
    .bind(feed_title)
    .execute(&mut *tx)
    .await?;
    for eid in entry_ids {
        sqlx::query(
            "INSERT OR IGNORE INTO sent_entries (chat_id, feed_url, entry_id) VALUES (?, ?, ?)",
        )
        .bind(chat_id)
        .bind(feed_url)
        .bind(eid)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Batch form of `is_entry_sent` for one subscription. The fetch cycle filters
/// entries against this set instead of running two queries per entry (a large
/// feed could hold tens of thousands of entries), and platform fetchers use it
/// to skip enrichment requests for already-pushed entries.
pub async fn sent_entry_ids(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
) -> anyhow::Result<std::collections::HashSet<String>> {
    let rows = sqlx::query("SELECT entry_id FROM sent_entries WHERE chat_id = ? AND feed_url = ?")
        .bind(chat_id)
        .bind(feed_url)
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().map(|r| r.get("entry_id")).collect())
}

/// Batch form of the per-entry outbox check for one subscription. The fetch
/// cycle skips entries the outbox already owns (the outbox has sole retry
/// authority). Fail-open semantics: callers treat a query error as empty,
/// because the outbox re-checks is_entry_sent before sending.
pub async fn outbox_entry_ids(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
) -> anyhow::Result<std::collections::HashSet<String>> {
    let rows = sqlx::query("SELECT entry_id FROM outbox WHERE chat_id = ? AND feed_url = ?")
        .bind(chat_id)
        .bind(feed_url)
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().map(|r| r.get("entry_id")).collect())
}

/// Ask both dedup tables about one entry in a single query. The scheduler
/// rechecks this immediately before sending: the batch snapshots taken before
/// the fetch can go stale while an overlapping outbox retry finishes the same
/// entry, and a duplicate push is worse than one extra indexed lookup.
/// Returns (already_sent, queued_in_outbox). Callers must fail closed on Err.
pub async fn entry_dedup_state(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    entry_id: &str,
) -> anyhow::Result<(bool, bool)> {
    let row = sqlx::query(
        r#"SELECT
             EXISTS(SELECT 1 FROM sent_entries WHERE chat_id = ?1 AND feed_url = ?2 AND entry_id = ?3) AS sent,
             EXISTS(SELECT 1 FROM outbox WHERE chat_id = ?1 AND feed_url = ?2 AND entry_id = ?3) AS queued"#,
    )
    .bind(chat_id)
    .bind(feed_url)
    .bind(entry_id)
    .fetch_one(db)
    .await?;
    Ok((row.get::<i64, _>("sent") != 0, row.get::<i64, _>("queued") != 0))
}

pub async fn delete_subscription(db: &Db, sub_id: i64) -> anyhow::Result<()> {
    // Drop pending outbox retries for this subscription first (while the
    // subscriptions row still exists so the correlated lookup works).
    // Wrapped in a transaction: outbox cleanup and subscription deletion are
    // atomic, avoiding intermediate states.
    let mut tx = db.begin().await?;
    sqlx::query(
        "DELETE FROM outbox WHERE EXISTS (SELECT 1 FROM subscriptions s WHERE s.id = ? AND s.chat_id = outbox.chat_id AND s.feed_url = outbox.feed_url)",
    )
    .bind(sub_id)
    .execute(&mut *tx)
    .await?;
    // Cascade-clean the media_dl_fail retry keys (prevents settings from
    // slowly leaking after unsubscribe). Uses instr prefix matching instead of
    // LIKE: feed_url often contains % or _ (URL encoding / underscores), and
    // LIKE would treat them as wildcards and wrongly delete keys of adjacent
    // sources.
    sqlx::query(
        "DELETE FROM settings WHERE instr(key, (SELECT 'media_dl_fail:' || chat_id || ':' || feed_url || ':' FROM subscriptions WHERE id = ?)) = 1",
    )
    .bind(sub_id)
    .execute(&mut *tx)
    .await?;
    // Cascade-clean the auto_disable_notified edge-dedup flag (this key has
    // no suffix, exact match).
    sqlx::query(
        "DELETE FROM settings WHERE key = (SELECT 'auto_disable_notified:' || chat_id || ':' || feed_url FROM subscriptions WHERE id = ?)",
    )
    .bind(sub_id)
    .execute(&mut *tx)
    .await?;
    // Cascade-clean the stale-zero streak counter (exact match, same shape).
    sqlx::query(
        "DELETE FROM settings WHERE key = (SELECT 'stale_zero:' || chat_id || ':' || feed_url FROM subscriptions WHERE id = ?)",
    )
    .bind(sub_id)
    .execute(&mut *tx)
    .await?;
    // Drop per-feed config overrides in the same transaction: a later
    // re-subscribe must start from channel defaults instead of reviving stale
    // interval/content/filter settings (previously this ran outside the
    // transaction and a failure silently kept the old row).
    sqlx::query(
        "DELETE FROM feed_configs WHERE EXISTS (SELECT 1 FROM subscriptions s WHERE s.id = ? AND s.chat_id = feed_configs.chat_id AND s.feed_url = feed_configs.feed_url)",
    )
    .bind(sub_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM subscriptions WHERE id = ?").bind(sub_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn delete_all_subscriptions(db: &Db, chat_id: &str) -> anyhow::Result<u64> {
    // Drop pending outbox retries for this chat (before subscriptions vanish).
    // Wrapped in a transaction: outbox cleanup and subscription deletion are
    // atomic, avoiding intermediate states.
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM outbox WHERE chat_id = ?")
        .bind(chat_id)
        .execute(&mut *tx)
        .await?;
    // Cascade-clean the media_dl_fail retry keys (prevents settings from
    // slowly leaking after unsubscribe).
    sqlx::query("DELETE FROM settings WHERE key LIKE ?")
        .bind(format!("media_dl_fail:{}:%", chat_id))
        .execute(&mut *tx)
        .await?;
    // Cascade-clean the auto_disable_notified edge-dedup flag.
    sqlx::query("DELETE FROM settings WHERE key LIKE ?")
        .bind(format!("auto_disable_notified:{}:%", chat_id))
        .execute(&mut *tx)
        .await?;
    // Cascade-clean the stale-zero streak counters.
    sqlx::query("DELETE FROM settings WHERE key LIKE ?")
        .bind(format!("stale_zero:{}:%", chat_id))
        .execute(&mut *tx)
        .await?;
    // Drop per-feed config overrides for this chat as well (same contract as
    // single unsubscribe: a later re-subscribe starts from defaults).
    sqlx::query("DELETE FROM feed_configs WHERE chat_id = ?")
        .bind(chat_id)
        .execute(&mut *tx)
        .await?;
    let result = sqlx::query("DELETE FROM subscriptions WHERE chat_id = ?")
        .bind(chat_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result.rows_affected())
}

/// Column list shared by every `SubRow` query (keep in sync with the struct).
const SUB_COLUMNS: &str = "id, chat_id, feed_url, feed_title, title_overridden, original_title, active, error_count, last_fetch_at, last_error, http_etag, http_last_modified, auto_disabled, disabled_at";

pub async fn get_subscriptions(db: &Db, chat_id: &str) -> anyhow::Result<Vec<SubRow>> {
    let sql = format!(
        "SELECT {SUB_COLUMNS} FROM subscriptions WHERE chat_id = ? ORDER BY id DESC"
    );
    let rows = sqlx::query_as::<_, SubRow>(sqlx::AssertSqlSafe(sql))
    .bind(chat_id)
    .fetch_all(db)
    .await?;
    Ok(rows)
}

pub async fn get_subscription_by_id(db: &Db, sub_id: i64) -> anyhow::Result<Option<SubRow>> {
    let sql = format!("SELECT {SUB_COLUMNS} FROM subscriptions WHERE id = ?");
    let row = sqlx::query_as::<_, SubRow>(sqlx::AssertSqlSafe(sql))
    .bind(sub_id)
    .fetch_optional(db)
    .await?;
    Ok(row)
}

pub async fn get_subscription_by_url(db: &Db, chat_id: &str, feed_url: &str) -> anyhow::Result<Option<SubRow>> {
    let sql = format!("SELECT {SUB_COLUMNS} FROM subscriptions WHERE chat_id = ? AND feed_url = ?");
    let row = sqlx::query_as::<_, SubRow>(sqlx::AssertSqlSafe(sql))
    .bind(chat_id)
    .bind(feed_url)
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// Rename a subscription manually. Sets the override flag so the scheduler
/// stops auto-syncing the feed title.
///
/// Smart reset (v1.3.3): if the new title equals the recorded original title,
/// the override is cleared (title_overridden=0, original_title=NULL) — the
/// source resumes following feed title updates and the "original name" display
/// disappears. This makes "rename back to original" a true reset.
///
/// NOTE: SET clause order matters — SQLite evaluates left-to-right and later
/// expressions see already-updated values. original_title must be computed
/// before feed_title is overwritten.
pub async fn rename_subscription(db: &Db, sub_id: i64, new_title: &str) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE subscriptions SET
           title_overridden = CASE WHEN ? = original_title THEN 0 ELSE 1 END,
           original_title = CASE WHEN ? = original_title THEN NULL ELSE COALESCE(original_title, feed_title) END,
           feed_title = ?
         WHERE id = ?",
    )
    .bind(new_title)
    .bind(new_title)
    .bind(new_title)
    .bind(sub_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Auto-update feed title from RSS without setting title_overridden.
pub async fn rename_subscription_auto(db: &Db, sub_id: i64, new_title: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE subscriptions SET feed_title = ? WHERE id = ? AND title_overridden = 0")
        .bind(new_title)
        .bind(sub_id)
        .execute(db)
        .await?;
    Ok(())
}

/// Effective config for one subscription (chat defaults + feed overrides).
pub async fn get_effective_config(db: &Db, chat_id: &str, feed_url: &str) -> anyhow::Result<EffectiveConfig> {
    // Build from a one-row base so a missing chat_configs row does not hide
    // feed_configs overrides. This is the same precedence as the due query
    // below: feed override > channel config > DEFAULT_*.
    let row = sqlx::query(
        r#"SELECT ?1 AS chat_id, ?2 AS feed_url,
                  COALESCE(f.interval_minutes, c.interval_minutes, 30) AS interval_minutes,
                  COALESCE(f.content_mode, c.content_mode, 'title_link') AS content_mode,
                  COALESCE(f.source_display, c.source_display, 'source') AS source_display,
                  COALESCE(f.caption_above_media, c.caption_above_media, 0) AS caption_above_media,
                  COALESCE(f.link_preview, c.link_preview, 1) AS link_preview,
                  f.interval_minutes IS NOT NULL AS iv_overridden,
                  f.content_mode IS NOT NULL AS cm_overridden,
                  f.source_display IS NOT NULL AS sd_overridden,
                  f.link_preview IS NOT NULL AS lp_overridden
           FROM (SELECT 1) AS base
           LEFT JOIN chat_configs c ON c.chat_id = ?1
           LEFT JOIN feed_configs f ON f.chat_id = ?1 AND f.feed_url = ?2"#,
    )
    .bind(chat_id)
    .bind(feed_url)
    .fetch_optional(db)
    .await?;
    // Defensive: the base row means a row should always exist, but keep a
    // defaults fallback if SQLite behavior changes.
    let Some(row) = row else {
        return Ok(EffectiveConfig {
            chat_id: chat_id.to_string(),
            feed_url: feed_url.to_string(),
            interval_minutes: DEFAULT_INTERVAL_MINUTES,
            content_mode: DEFAULT_CONTENT_MODE.to_string(),
            source_display: DEFAULT_SOURCE_DISPLAY.to_string(),
            caption_above_media: DEFAULT_CAPTION_ABOVE_MEDIA != 0,
            link_preview: DEFAULT_LINK_PREVIEW != 0,
            iv_overridden: false,
            cm_overridden: false,
            sd_overridden: false,
            lp_overridden: false,
        });
    };
    Ok(EffectiveConfig {
        chat_id: row.get("chat_id"),
        feed_url: row.get("feed_url"),
        interval_minutes: row.get("interval_minutes"),
        content_mode: row.get("content_mode"),
        source_display: row.get("source_display"),
        caption_above_media: row.get::<i64, _>("caption_above_media") != 0,
        link_preview: row.get::<i64, _>("link_preview") != 0,
        iv_overridden: row.get::<i64, _>("iv_overridden") != 0,
        cm_overridden: row.get::<i64, _>("cm_overridden") != 0,
        sd_overridden: row.get::<i64, _>("sd_overridden") != 0,
        lp_overridden: row.get::<i64, _>("lp_overridden") != 0,
    })
}

/// Manual pause/resume (bot button). Manual operations set auto_disabled=0,
/// guaranteeing that a source the user actively stopped is never auto-restored
/// by the self-heal job.
///
/// Resuming (active=true) also mirrors the auto-heal recovery: it resets the
/// failure counter and clears the auto-disable notification dedup flag, so a
/// source the user manually resumes starts from a clean slate. Without this,
/// a manual resume left the stale error_count in place, so the very next
/// failure could immediately re-trigger auto-disable — and because the
/// notified flag was still set, that re-disable was silent (the user perceives
/// the resume button as "not working"). Pausing leaves the counters untouched.
pub async fn set_subscription_active(db: &Db, sub_id: i64, active: bool) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    if active {
        sqlx::query(
            "UPDATE subscriptions SET active = 1, auto_disabled = 0, disabled_at = NULL, error_count = 0, last_error = NULL WHERE id = ?",
        )
        .bind(sub_id)
        .execute(&mut *tx)
        .await?;
        // Clear the per-feed auto-disable notification dedup flag (exact key
        // match, same shape as the unsubscribe cascade) so the next genuine
        // disable can notify again.
        sqlx::query(
            "DELETE FROM settings WHERE key = (SELECT 'auto_disable_notified:' || chat_id || ':' || feed_url FROM subscriptions WHERE id = ?)",
        )
        .bind(sub_id)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            "UPDATE subscriptions SET active = 0, auto_disabled = 0, disabled_at = NULL WHERE id = ?",
        )
        .bind(sub_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// System auto-disable (consecutive failures reached the threshold): sets
/// auto_disabled=1 and records the disable moment so the self-heal job can
/// auto-recover it after the cooldown.
/// The `active = 1` guard closes the race against a manual pause made during
/// the fetch round: the scheduler decides from a round-start snapshot, so
/// without the guard a just-paused (auto_disabled=0) source would be flipped
/// to auto-disabled and resurrected by the self-heal cooldown, overriding the
/// user's intent. Returns whether a row was actually disabled (false = the
/// user already paused it this round, caller should skip the notification).
pub async fn auto_disable_subscription(db: &Db, sub_id: i64) -> anyhow::Result<bool> {
    let res = sqlx::query(
        "UPDATE subscriptions SET active = 0, auto_disabled = 1, disabled_at = datetime('now') WHERE id = ? AND active = 1",
    )
    .bind(sub_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Self-heal: re-enable sources that are "auto-disabled and past the cooldown"
/// and reset their error counts to zero. Returns the number of recovered
/// sources. Manually paused (auto_disabled=0) sources are not affected.
pub async fn reenable_due_auto_disabled(db: &Db, cooldown_hours: i64) -> anyhow::Result<u64> {
    let res = sqlx::query(
        r#"UPDATE subscriptions
           SET active = 1, auto_disabled = 0, disabled_at = NULL, error_count = 0, last_error = NULL
           WHERE active = 0 AND auto_disabled = 1
             AND disabled_at IS NOT NULL
             AND (julianday('now') - julianday(disabled_at)) * 24 >= ?"#,
    )
    .bind(cooldown_hours)
    .execute(db)
    .await?;
    Ok(res.rows_affected())
}

/// One-click enable all disabled sources under a channel (manual operation):
/// resets error counts and auto-disable flags. Returns the number of enabled
/// sources.
pub async fn resume_all_subscriptions(db: &Db, chat_id: &str) -> anyhow::Result<u64> {
    let mut tx = db.begin().await?;
    let res = sqlx::query(
        r#"UPDATE subscriptions
           SET active = 1, auto_disabled = 0, disabled_at = NULL, error_count = 0, last_error = NULL
           WHERE chat_id = ? AND active = 0"#,
    )
    .bind(chat_id)
    .execute(&mut *tx)
    .await?;
    // Clear the per-feed auto-disable notification dedup flags so a source
    // that fails again before any success can notify the owner (same cleanup
    // as the single-subscription resume path).
    sqlx::query("DELETE FROM settings WHERE key LIKE ?")
        .bind(format!("auto_disable_notified:{}:%", chat_id))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(res.rows_affected())
}

pub async fn increment_error_count(db: &Db, sub_id: i64, error_msg: &str) -> anyhow::Result<i64> {
    // Wrap UPDATE+SELECT in a transaction: guarantees we read the value after
    // this increment. fetch_optional: the subscription may have been deleted
    // during fetch, in which case return 0 instead of erroring.
    let mut tx = db.begin().await?;
    sqlx::query(
        "UPDATE subscriptions SET error_count = error_count + 1, last_error = ?, last_fetch_at = datetime('now') WHERE id = ?",
    )
    .bind(error_msg)
    .bind(sub_id)
    .execute(&mut *tx)
    .await?;
    let row = sqlx::query("SELECT error_count FROM subscriptions WHERE id = ?")
        .bind(sub_id)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(row.map(|r| r.get::<i64, _>("error_count")).unwrap_or(0))
}

pub async fn reset_error_count(db: &Db, sub_id: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE subscriptions SET error_count = 0, last_error = NULL, last_fetch_at = datetime('now') WHERE id = ?")
        .bind(sub_id)
        .execute(db)
        .await?;
    Ok(())
}

/// Flat row combining subscription + effective-config columns for
/// `get_due_subscriptions` (avoids manual per-column `row.get` mapping).
#[derive(Debug, sqlx::FromRow)]
struct DueRow {
    #[sqlx(flatten)]
    sub: SubRow,
    #[sqlx(flatten)]
    cfg: EffectiveConfig,
}

/// Get all active subscriptions that are due for fetching.
pub async fn get_due_subscriptions(db: &Db) -> anyhow::Result<Vec<(SubRow, EffectiveConfig)>> {
    // The COALESCE fallback defaults must stay in sync with the DEFAULT_*
    // constants (asserted by test_default_constants).
    let rows = sqlx::query_as::<_, DueRow>(
        r#"SELECT s.id, s.chat_id, s.feed_url, s.feed_title, s.title_overridden, s.original_title, s.active, s.error_count, s.last_fetch_at, s.last_error,
                  s.http_etag, s.http_last_modified, s.auto_disabled, s.disabled_at,
                  COALESCE(f.interval_minutes, c.interval_minutes, 30) AS interval_minutes,
                  COALESCE(f.content_mode, c.content_mode, 'title_link') AS content_mode,
                  COALESCE(f.source_display, c.source_display, 'source') AS source_display,
                  COALESCE(f.caption_above_media, c.caption_above_media, 0) AS caption_above_media,
                  COALESCE(f.link_preview, c.link_preview, 1) AS link_preview,
                  f.interval_minutes IS NOT NULL AS iv_overridden,
                  f.content_mode IS NOT NULL AS cm_overridden,
                  f.source_display IS NOT NULL AS sd_overridden,
                  f.link_preview IS NOT NULL AS lp_overridden
           FROM subscriptions s
           LEFT JOIN chat_configs c ON c.chat_id = s.chat_id
           LEFT JOIN feed_configs f ON f.chat_id = s.chat_id AND f.feed_url = s.feed_url
           JOIN chats ch ON ch.chat_id = s.chat_id AND ch.enabled = 1
           WHERE s.active = 1
             AND (s.last_fetch_at IS NULL OR (julianday('now') - julianday(s.last_fetch_at)) * 1440 >= COALESCE(f.interval_minutes, c.interval_minutes, 30))"#,
    )
    .fetch_all(db)
    .await?;

    Ok(rows.into_iter().map(|r| (r.sub, r.cfg)).collect())
}

/// Save the HTTP negotiation headers (ETag / Last-Modified) of this fetch
/// response for the next conditional request; passing None means the response
/// did not carry that header (keep the old value, do not overwrite).
pub async fn update_fetch_headers(
    db: &Db,
    sub_id: i64,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(e) = etag {
        sqlx::query("UPDATE subscriptions SET http_etag = ? WHERE id = ?")
            .bind(e)
            .bind(sub_id)
            .execute(db)
            .await?;
    }
    if let Some(lm) = last_modified {
        sqlx::query("UPDATE subscriptions SET http_last_modified = ? WHERE id = ?")
            .bind(lm)
            .bind(sub_id)
            .execute(db)
            .await?;
    }
    Ok(())
}

// ===== Chat configs =====

pub async fn ensure_chat_config(db: &Db, chat_id: &str) -> anyhow::Result<()> {
    // New channels default to a 30-minute fetch interval (migration 002 has
    // already changed existing rows to 30, but the DDL DEFAULT is still 10,
    // so pass the value explicitly here to stay consistent).
    // The value must match DEFAULT_INTERVAL_MINUTES (asserted by
    // test_default_constants).
    sqlx::query(
        "INSERT OR IGNORE INTO chat_configs (chat_id, interval_minutes) VALUES (?, 30)",
    )
    .bind(chat_id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn get_chat_config(db: &Db, chat_id: &str) -> anyhow::Result<Option<ChatConfigRow>> {
    let row = sqlx::query_as::<_, ChatConfigRow>(
        "SELECT chat_id, interval_minutes, content_mode, source_display, caption_above_media, link_preview FROM chat_configs WHERE chat_id = ?",
    )
    .bind(chat_id)
    .fetch_optional(db)
    .await?;
    Ok(row)
}

pub async fn set_chat_config_field(
    db: &Db,
    chat_id: &str,
    field: &str,
    value: &str,
) -> anyhow::Result<()> {
    // Callers usually opened the settings page first, but do not rely on it:
    // an UPDATE on a missing row affects zero rows and silently returns Ok,
    // and the DDL default for a new interval is still 10. Create the row with
    // the documented 30-minute default before updating the requested field.
    ensure_chat_config(db, chat_id).await?;
    // The column name is a compile-time constant, so this does not violate
    // the no-dynamic-SQL convention.
    let sql = match field {
        "interval_minutes" => "UPDATE chat_configs SET interval_minutes = ?, updated_at = datetime('now') WHERE chat_id = ?",
        "content_mode" => "UPDATE chat_configs SET content_mode = ?, updated_at = datetime('now') WHERE chat_id = ?",
        "source_display" => "UPDATE chat_configs SET source_display = ?, updated_at = datetime('now') WHERE chat_id = ?",
        "caption_above_media" => "UPDATE chat_configs SET caption_above_media = ?, updated_at = datetime('now') WHERE chat_id = ?",
        "link_preview" => "UPDATE chat_configs SET link_preview = ?, updated_at = datetime('now') WHERE chat_id = ?",
        _ => anyhow::bail!("unknown field: {}", field),
    };
    if field == "interval_minutes" {
        // Defense: ensure the value bound to an INTEGER column is a valid
        // number (callers usually validate already, but the db layer should
        // not assume so).
        let v: i64 = value.parse()
            .map_err(|_| anyhow::anyhow!("interval_minutes must be numeric, got '{}'", value))?;
        sqlx::query(sql).bind(v).bind(chat_id).execute(db).await?;
    } else {
        sqlx::query(sql).bind(value).bind(chat_id).execute(db).await?;
    }
    Ok(())
}

// ===== Feed configs =====

pub async fn set_feed_config_field(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    field: &str,
    value: Option<&str>,
) -> anyhow::Result<()> {
    if value.is_none() {
        // Reset to inherit: set column to NULL.
        let sql = match field {
            "interval_minutes" => "UPDATE feed_configs SET interval_minutes = NULL, updated_at = datetime('now') WHERE chat_id = ? AND feed_url = ?",
            "content_mode" => "UPDATE feed_configs SET content_mode = NULL, updated_at = datetime('now') WHERE chat_id = ? AND feed_url = ?",
            "source_display" => "UPDATE feed_configs SET source_display = NULL, updated_at = datetime('now') WHERE chat_id = ? AND feed_url = ?",
            "caption_above_media" => "UPDATE feed_configs SET caption_above_media = NULL, updated_at = datetime('now') WHERE chat_id = ? AND feed_url = ?",
            "link_preview" => "UPDATE feed_configs SET link_preview = NULL, updated_at = datetime('now') WHERE chat_id = ? AND feed_url = ?",
            _ => anyhow::bail!("unknown field: {}", field),
        };
        sqlx::query(sql).bind(chat_id).bind(feed_url).execute(db).await?;
        // If all fields are NULL, delete the row.
        // The three filter columns are NOT NULL DEFAULT (0/'blacklist'/'') and
        // are never NULL, so we must judge by "filter completely unconfigured":
        // as long as the user ever touched the filter (enabled the switch or
        // stored keywords) keep the whole row, otherwise resetting other fields
        // would silently lose the content-filter config.
        sqlx::query(
            "DELETE FROM feed_configs WHERE chat_id = ? AND feed_url = ? AND interval_minutes IS NULL AND content_mode IS NULL AND source_display IS NULL AND caption_above_media IS NULL AND link_preview IS NULL AND filter_enabled = 0 AND filter_keywords = '' AND filter_mode = 'blacklist'",
        )
        .bind(chat_id)
        .bind(feed_url)
        .execute(db)
        .await?;
    } else if let Some(v) = value {
        let sql = match field {
            "interval_minutes" => "INSERT INTO feed_configs (chat_id, feed_url, interval_minutes) VALUES (?1, ?2, ?3) ON CONFLICT(chat_id, feed_url) DO UPDATE SET interval_minutes = excluded.interval_minutes, updated_at = datetime('now')",
            "content_mode" => "INSERT INTO feed_configs (chat_id, feed_url, content_mode) VALUES (?1, ?2, ?3) ON CONFLICT(chat_id, feed_url) DO UPDATE SET content_mode = excluded.content_mode, updated_at = datetime('now')",
            "source_display" => "INSERT INTO feed_configs (chat_id, feed_url, source_display) VALUES (?1, ?2, ?3) ON CONFLICT(chat_id, feed_url) DO UPDATE SET source_display = excluded.source_display, updated_at = datetime('now')",
            "caption_above_media" => "INSERT INTO feed_configs (chat_id, feed_url, caption_above_media) VALUES (?1, ?2, ?3) ON CONFLICT(chat_id, feed_url) DO UPDATE SET caption_above_media = excluded.caption_above_media, updated_at = datetime('now')",
            "link_preview" => "INSERT INTO feed_configs (chat_id, feed_url, link_preview) VALUES (?1, ?2, ?3) ON CONFLICT(chat_id, feed_url) DO UPDATE SET link_preview = excluded.link_preview, updated_at = datetime('now')",
            _ => anyhow::bail!("unknown field: {}", field),
        };
        if field == "interval_minutes" {
            // Defense: ensure the value bound to an INTEGER column is a valid
            // number.
            let n: i64 = v.parse()
                .map_err(|_| anyhow::anyhow!("interval_minutes must be numeric, got '{}'", v))?;
            sqlx::query(sql).bind(chat_id).bind(feed_url).bind(n).execute(db).await?;
        } else {
            sqlx::query(sql).bind(chat_id).bind(feed_url).bind(v).execute(db).await?;
        }
    }
    Ok(())
}

// ===== Sent entries =====

pub async fn is_entry_sent(db: &Db, chat_id: &str, feed_url: &str, entry_id: &str) -> anyhow::Result<bool> {
    let row = sqlx::query(
        "SELECT 1 FROM sent_entries WHERE chat_id = ? AND feed_url = ? AND entry_id = ?",
    )
    .bind(chat_id)
    .bind(feed_url)
    .bind(entry_id)
    .fetch_optional(db)
    .await?;
    Ok(row.is_some())
}

pub async fn mark_entry_sent(db: &Db, chat_id: &str, feed_url: &str, entry_id: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO sent_entries (chat_id, feed_url, entry_id) VALUES (?, ?, ?)",
    )
    .bind(chat_id)
    .bind(feed_url)
    .bind(entry_id)
    .execute(db)
    .await?;
    Ok(())
}

// ===== Cleanup =====

pub async fn cleanup_sent_entries(db: &Db, retention_days: i64) -> anyhow::Result<u64> {
    let result = sqlx::query(
        "DELETE FROM sent_entries WHERE sent_at < datetime('now', ?)",
    )
    .bind(format!("-{} days", retention_days))
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

// ===== File-ID cache (upload dedup) =====

/// Cache lookup: whether a reusable Telegram file_id already exists for the
/// same content hash.
pub async fn get_cached_file_id(db: &Db, content_hash: &str) -> anyhow::Result<Option<String>> {
    let row = sqlx::query("SELECT file_id FROM file_id_cache WHERE content_hash = ?")
        .bind(content_hash)
        .fetch_optional(db)
        .await?;
    Ok(row.map(|r| r.get::<String, _>("file_id")))
}

/// Cache write: record the file_id after a successful media upload
/// (refreshes the timestamp on conflict).
pub async fn put_cached_file_id(db: &Db, content_hash: &str, file_id: &str) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO file_id_cache (content_hash, file_id)
           VALUES (?1, ?2)
           ON CONFLICT(content_hash) DO UPDATE SET
             file_id = excluded.file_id,
             created_at = datetime('now')"#,
    )
    .bind(content_hash)
    .bind(file_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Cache invalidation: delete the entry when sending by file_id is rejected by
/// Telegram (bot changed / file cleaned up); the next attempt automatically
/// falls back to re-uploading.
pub async fn invalidate_cached_file_id(db: &Db, content_hash: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM file_id_cache WHERE content_hash = ?")
        .bind(content_hash)
        .execute(db)
        .await?;
    Ok(())
}

/// Clean up expired cache entries (retention in days); returns the number of
/// deleted rows.
pub async fn cleanup_file_id_cache(db: &Db, retention_days: i64) -> anyhow::Result<u64> {
    let result = sqlx::query(
        "DELETE FROM file_id_cache WHERE created_at < datetime('now', ?)",
    )
    .bind(format!("-{} days", retention_days))
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

/// Clean up stale keys with the given prefix in the settings table
/// (updated_at older than the retention period). Used for keys that can linger
/// past an entry's lifecycle (e.g. media_dl_fail: when an entry rolls out of
/// the source's sliding window before reaching the retry cap, neither the
/// download-success nor the abandon deletion point will ever fire again).
/// Prefix matching uses instr instead of LIKE (feed_url may contain the % / _
/// wildcard characters).
pub async fn cleanup_stale_settings(db: &Db, prefix: &str, retention_days: i64) -> anyhow::Result<u64> {
    let result = sqlx::query(
        "DELETE FROM settings WHERE instr(key, ?) = 1 AND updated_at < datetime('now', ?)",
    )
    .bind(prefix)
    .bind(format!("-{} days", retention_days))
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

// ===== Sent messages registry (/find reverse lookup) =====

/// Register one sent message: TG message → RSS feed entry. Row-count
/// trimming is done by the periodic cleanup job, not on every insert: the
/// previous COUNT(*) ran once per delivered message.
pub async fn record_sent_message(
    db: &Db,
    chat_id: &str,
    message_id: i64,
    feed_url: &str,
    entry_id: &str,
    entry_link: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO sent_messages (chat_id, message_id, feed_url, entry_id, entry_link)
           VALUES (?1, ?2, ?3, ?4, ?5)
           ON CONFLICT(chat_id, message_id) DO UPDATE SET
             feed_url = excluded.feed_url,
             entry_id = excluded.entry_id,
             entry_link = excluded.entry_link,
             created_at = datetime('now')"#,
    )
    .bind(chat_id)
    .bind(message_id)
    .bind(feed_url)
    .bind(entry_id)
    .bind(entry_link)
    .execute(db)
    .await?;
    Ok(())
}

/// Enforce the sent-messages row cap (called from the periodic cleanup job).
/// Returns the number of rows removed.
pub async fn trim_sent_messages(db: &Db, max_rows: i64) -> anyhow::Result<u64> {
    if max_rows <= 0 {
        return Ok(0);
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sent_messages")
        .fetch_one(db)
        .await?;
    if count <= max_rows {
        return Ok(0);
    }
    let result = sqlx::query(
        r#"DELETE FROM sent_messages
           WHERE id NOT IN (SELECT id FROM sent_messages ORDER BY id DESC LIMIT ?)"#,
    )
    .bind(max_rows)
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

/// /find reverse lookup: find the RSS entry for a channel + message id.
pub struct SentMessageRow {
    pub feed_url: String,
    pub entry_id: String,
    pub entry_link: Option<String>,
    pub created_at: String,
}

pub async fn find_sent_message(
    db: &Db,
    chat_id: &str,
    message_id: i64,
) -> anyhow::Result<Option<SentMessageRow>> {
    let row = sqlx::query(
        "SELECT feed_url, entry_id, entry_link, created_at FROM sent_messages WHERE chat_id = ? AND message_id = ?",
    )
    .bind(chat_id)
    .bind(message_id)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|r| SentMessageRow {
        feed_url: r.get("feed_url"),
        entry_id: r.get("entry_id"),
        entry_link: r.get("entry_link"),
        created_at: r.get("created_at"),
    }))
}


// ===== Outbox (persistent retry queue for failed sends) =====

// next_retry_at/last_error are only used in SQL WHERE/UPDATE and never read
// directly on the Rust side — kept to preserve the complete mapping of the
// database schema.
#[allow(dead_code)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OutboxRow {
    pub id: i64,
    pub chat_id: String,
    pub feed_url: String,
    pub entry_id: String,
    pub feed_title: String,
    pub entry_json: String,
    pub media_json: String,
    /// Group-C sidecar: serialized EntryMeta JSON (empty string means no
    /// metadata).
    pub meta_json: String,
    pub content_mode: String,
    pub source_display: String,
    pub link_preview: bool,
    pub caption_above_media: bool,
    pub author: Option<String>,
    pub attempts: i64,
    pub next_retry_at: i64,
    pub created_at: i64,
    pub last_error: Option<String>,
}

/// Insert a failed entry into the outbox for later retry.
/// Uses INSERT OR IGNORE to avoid duplicates (same entry already queued).
#[allow(clippy::too_many_arguments)]
pub async fn outbox_insert(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    entry_id: &str,
    feed_title: &str,
    entry_json: &str,
    media_json: &str,
    meta_json: &str,
    content_mode: &str,
    source_display: &str,
    link_preview: bool,
    caption_above_media: bool,
    author: Option<&str>,
    next_retry_at: i64,
    created_at: i64,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "INSERT OR IGNORE INTO outbox (chat_id, feed_url, entry_id, feed_title, entry_json, media_json, meta_json, content_mode, source_display, link_preview, caption_above_media, author, next_retry_at, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(chat_id)
    .bind(feed_url)
    .bind(entry_id)
    .bind(feed_title)
    .bind(entry_json)
    .bind(media_json)
    .bind(meta_json)
    .bind(content_mode)
    .bind(source_display)
    .bind(link_preview)
    .bind(caption_above_media)
    .bind(author)
    .bind(next_retry_at)
    .bind(created_at)
    .execute(db)
    .await?;
    // INSERT OR IGNORE: rows_affected=0 means the entry is already in the
    // queue (duplicate enqueue is ignored).
    Ok(result.rows_affected() > 0)
}

/// Fetch outbox entries that are due for retry.
pub async fn outbox_fetch_due(db: &Db, now_ts: i64, limit: i64) -> anyhow::Result<Vec<OutboxRow>> {
    let rows = sqlx::query_as::<_, OutboxRow>(
        "SELECT id, chat_id, feed_url, entry_id, feed_title, entry_json, media_json, meta_json, content_mode, source_display, link_preview, caption_above_media, author, attempts, next_retry_at, created_at, last_error FROM outbox WHERE next_retry_at <= ? ORDER BY next_retry_at ASC LIMIT ?",
    )
    .bind(now_ts)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// Return every media_json value still referenced by the outbox (all rows,
/// no limit, one narrow column). Used by the media cleanup pass to build the
/// "protected paths" set — loading media_json alone avoids dragging each
/// row's entry_json / meta_json blobs into memory, which outbox_fetch_due
/// (the full-row variant) would.
pub async fn outbox_all_media_json(db: &Db) -> anyhow::Result<Vec<String>> {
    let rows = sqlx::query("SELECT media_json FROM outbox WHERE media_json IS NOT NULL AND media_json <> '' AND media_json <> '[]'")
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().filter_map(|r| r.get::<Option<String>, _>("media_json")).collect())
}

/// Update outbox entry after a failed retry attempt.
pub async fn outbox_update_retry(
    db: &Db,
    id: i64,
    attempts: i64,
    next_retry_at: i64,
    last_error: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE outbox SET attempts = ?, next_retry_at = ?, last_error = ? WHERE id = ?",
    )
    .bind(attempts)
    .bind(next_retry_at)
    .bind(last_error)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

/// Remove an outbox entry (after successful send or abandonment).
pub async fn outbox_delete(db: &Db, id: i64) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM outbox WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}


// ===== Content filtering (per-feed) =====

/// Per-feed content filter config.
#[derive(Debug, Clone, Default)]
pub struct FilterConfig {
    pub enabled: bool,
    /// "blacklist" (exclude matching) or "whitelist" (only matching).
    pub mode: String,
    /// Comma-separated keywords matched against entry titles (case-insensitive).
    pub keywords: String,
}

impl FilterConfig {
    /// True when filtering should actually be applied (switch on + keywords set).
    pub fn is_active(&self) -> bool {
        self.enabled && !self.keywords.trim().is_empty()
    }

    /// Parsed keyword list (trimmed, lowercased, empty items dropped).
    pub fn keyword_list(&self) -> Vec<String> {
        self.keywords
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Whether an entry title passes the filter (true = should be pushed).
    /// The scheduler runs
    /// this once per entry; re-splitting/lowercasing the same string for a
    /// large feed is pure overhead.
    pub fn allows_title_with_keywords(&self, title: &str, keywords: &[String]) -> bool {
        if !self.is_active() {
            return true;
        }
        let lower = title.to_lowercase();
        let matched = keywords.iter().any(|kw| lower.contains(kw));
        match self.mode.as_str() {
            "whitelist" => matched,
            _ => !matched, // blacklist: exclude matches
        }
    }
}

/// Read the content filter config for a feed (defaults when no row exists).
pub async fn get_feed_filter(db: &Db, chat_id: &str, feed_url: &str) -> anyhow::Result<FilterConfig> {
    let row = sqlx::query(
        "SELECT filter_enabled, filter_mode, filter_keywords FROM feed_configs WHERE chat_id = ? AND feed_url = ?",
    )
    .bind(chat_id)
    .bind(feed_url)
    .fetch_optional(db)
    .await?;
    Ok(match row {
        Some(r) => FilterConfig {
            enabled: r.get::<i64, _>("filter_enabled") != 0,
            mode: r.get("filter_mode"),
            keywords: r.get("filter_keywords"),
        },
        None => FilterConfig::default(),
    })
}

/// Update one filter field (upsert; creates the feed_configs row if missing).
async fn set_feed_filter_field(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    field: &str,
    value: &str,
) -> anyhow::Result<()> {
    let sql = match field {
        "filter_enabled" => "INSERT INTO feed_configs (chat_id, feed_url, filter_enabled) VALUES (?1, ?2, ?3) ON CONFLICT(chat_id, feed_url) DO UPDATE SET filter_enabled = excluded.filter_enabled, updated_at = datetime('now')",
        "filter_mode" => "INSERT INTO feed_configs (chat_id, feed_url, filter_mode) VALUES (?1, ?2, ?3) ON CONFLICT(chat_id, feed_url) DO UPDATE SET filter_mode = excluded.filter_mode, updated_at = datetime('now')",
        "filter_keywords" => "INSERT INTO feed_configs (chat_id, feed_url, filter_keywords) VALUES (?1, ?2, ?3) ON CONFLICT(chat_id, feed_url) DO UPDATE SET filter_keywords = excluded.filter_keywords, updated_at = datetime('now')",
        _ => anyhow::bail!("unknown filter field: {}", field),
    };
    sqlx::query(sql)
        .bind(chat_id)
        .bind(feed_url)
        .bind(value)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn set_feed_filter_enabled(db: &Db, chat_id: &str, feed_url: &str, enabled: bool) -> anyhow::Result<()> {
    set_feed_filter_field(db, chat_id, feed_url, "filter_enabled", if enabled { "1" } else { "0" }).await
}

pub async fn set_feed_filter_mode(db: &Db, chat_id: &str, feed_url: &str, mode: &str) -> anyhow::Result<()> {
    set_feed_filter_field(db, chat_id, feed_url, "filter_mode", mode).await
}

pub async fn set_feed_filter_keywords(db: &Db, chat_id: &str, feed_url: &str, keywords: &str) -> anyhow::Result<()> {
    set_feed_filter_field(db, chat_id, feed_url, "filter_keywords", keywords).await
}

/// Clear both keywords and the enable switch in one transaction. "Clear"
/// in the UI means filtering is off; clearing only the keywords left
/// `filter_enabled=1`, so the panel showed "off" while the button still
/// offered "disable", and the next keyword entry silently re-enabled it.
pub async fn clear_feed_filter(db: &Db, chat_id: &str, feed_url: &str) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query(
        "UPDATE feed_configs SET filter_keywords = '', filter_enabled = 0, updated_at = datetime('now') WHERE chat_id = ? AND feed_url = ?",
    )
    .bind(chat_id)
    .bind(feed_url)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tripwire: the DEFAULT_* constants must stay in sync with the SQL
    /// COALESCE literals in get_due_subscriptions / ensure_chat_config
    /// (sqlx 0.9 forbids dynamic SQL, so they cannot be injected via format!).
    /// When changing a constant, the SQL literal must be changed too,
    /// otherwise this test fails.
    #[test]
    fn default_constants_match_sql_literals() {
        assert_eq!(DEFAULT_INTERVAL_MINUTES, 30);
        assert_eq!(DEFAULT_CONTENT_MODE, "title_link");
        assert_eq!(DEFAULT_SOURCE_DISPLAY, "source");
        assert_eq!(DEFAULT_CAPTION_ABOVE_MEDIA, 0);
        assert_eq!(DEFAULT_LINK_PREVIEW, 1);

        // Ensure the SQL COALESCE fallbacks use the same literals. The old
        // version only compared the constants with hardcoded numbers and
        // could never catch SQL drift.
        let src = include_str!("db.rs");
        for (field, value) in [
            ("interval_minutes", "30"),
            ("content_mode", "'title_link'"),
            ("source_display", "'source'"),
            ("caption_above_media", "0"),
            ("link_preview", "1"),
        ] {
            let needle = format!("COALESCE(f.{field}, c.{field}, {value})");
            assert!(src.contains(&needle), "missing SQL fallback: {needle}");
        }
    }

    #[test]
    fn filter_keywords_preparsed_match_single_shot() {
        let blacklist = FilterConfig {
            enabled: true,
            mode: "blacklist".into(),
            keywords: "foo, Bar".into(),
        };
        let kws = blacklist.keyword_list();
        assert!(!blacklist.allows_title_with_keywords("a FOO b", &kws));
        assert!(blacklist.allows_title_with_keywords("baz", &kws));
        let whitelist = FilterConfig {
            enabled: true,
            mode: "whitelist".into(),
            keywords: "foo".into(),
        };
        let kws = whitelist.keyword_list();
        assert!(whitelist.allows_title_with_keywords("foo", &kws));
        assert!(!whitelist.allows_title_with_keywords("bar", &kws));
        assert!(FilterConfig::default().allows_title_with_keywords("anything", &[]));
    }

    async fn test_pool() -> Db {
        // Single connection: every sqlite::memory: connection would otherwise
        // see its own empty database.
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn effective_config_keeps_feed_override_without_chat_row() {
        let pool = test_pool().await;
        sqlx::query(
            "INSERT INTO feed_configs (chat_id, feed_url, content_mode) VALUES ('1', 'https://example.com/feed', 'content_only')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let cfg = get_effective_config(&pool, "1", "https://example.com/feed")
            .await
            .unwrap();
        assert_eq!(cfg.content_mode, "content_only");
        assert!(cfg.cm_overridden);
        assert_eq!(cfg.interval_minutes, DEFAULT_INTERVAL_MINUTES);
        assert_eq!(cfg.source_display, DEFAULT_SOURCE_DISPLAY);
    }

    #[tokio::test]
    async fn due_subscriptions_flatten_effective_config() {
        let pool = test_pool().await;
        sqlx::query("INSERT INTO chats (chat_id, chat_type, enabled) VALUES ('1', 'channel', 1)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO subscriptions (chat_id, feed_url) VALUES ('1', 'https://example.com/feed')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO feed_configs (chat_id, feed_url, interval_minutes, content_mode) VALUES ('1', 'https://example.com/feed', 5, 'content_only')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let due = get_due_subscriptions(&pool).await.unwrap();
        assert_eq!(due.len(), 1);
        let (sub, cfg) = &due[0];
        assert_eq!(sub.chat_id, "1");
        assert_eq!(cfg.feed_url, "https://example.com/feed");
        assert_eq!(cfg.interval_minutes, 5);
        assert_eq!(cfg.content_mode, "content_only");
        assert!(cfg.iv_overridden);
        assert!(cfg.cm_overridden);
        assert!(cfg.link_preview);
    }

    #[tokio::test]
    async fn set_chat_config_field_creates_row_with_defaults() {
        let pool = test_pool().await;
        set_chat_config_field(&pool, "1", "content_mode", "content_only")
            .await
            .unwrap();
        let cfg = get_chat_config(&pool, "1").await.unwrap().expect("row created");
        assert_eq!(cfg.interval_minutes, DEFAULT_INTERVAL_MINUTES);
        assert_eq!(cfg.content_mode, "content_only");
    }

    #[tokio::test]
    async fn trim_sent_messages_keeps_newest_rows() {
        let pool = test_pool().await;
        for mid in 1..=3 {
            record_sent_message(&pool, "1", mid, "https://example.com/feed", &format!("e{mid}"), None)
                .await
                .unwrap();
        }
        assert_eq!(trim_sent_messages(&pool, 2).await.unwrap(), 1);
        let rows: Vec<i64> = sqlx::query_scalar("SELECT message_id FROM sent_messages ORDER BY message_id")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(rows, vec![2, 3]);
    }

    #[tokio::test]
    async fn entry_dedup_state_reports_sent_and_queued() {
        let pool = test_pool().await;

        mark_entry_sent(&pool, "1", "https://example.com/feed", "sent-entry")
            .await
            .unwrap();
        outbox_insert(
            &pool,
            "1",
            "https://example.com/feed",
            "queued-entry",
            "Feed",
            "{}",
            "[]",
            "",
            "title_link",
            "source",
            true,
            false,
            None,
            0,
            0,
        )
        .await
        .unwrap();

        assert_eq!(
            entry_dedup_state(&pool, "1", "https://example.com/feed", "sent-entry").await.unwrap(),
            (true, false)
        );
        assert_eq!(
            entry_dedup_state(&pool, "1", "https://example.com/feed", "queued-entry").await.unwrap(),
            (false, true)
        );
        assert_eq!(
            entry_dedup_state(&pool, "1", "https://example.com/feed", "new-entry").await.unwrap(),
            (false, false)
        );
        assert_eq!(
            entry_dedup_state(&pool, "1", "https://example.com/other", "sent-entry").await.unwrap(),
            (false, false)
        );
    }
}
