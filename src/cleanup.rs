use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use sqlx::Row;
use tokio::fs;
use tracing::{info, warn};
use walkdir::WalkDir;

use crate::config::Config;
use crate::db::{self, Db};

/// Run all cleanup tasks once. `ctx` is a log prefix distinguishing the
/// periodic run ("cleanup") from the startup run ("startup cleanup").
/// L6 fix: run_all_cleanup and run_startup_cleanup duplicated the same
/// 5-task sequence; collapsed into one function parameterized by context.
async fn run_cleanup_tasks(config: &Config, db_pool: &Db, ctx: &str) {
    // The two media-cleanup passes below both need the outbox protection set.
    // Computing it once here (instead of once per pass) halves the full-table
    // outbox scan cost. On query failure both media passes are skipped (an
    // empty protection set would delete retry-window media), but the
    // unrelated log/database/backup passes still run.
    match outbox_protected_paths(db_pool).await {
        Ok(protected) => {
            if let Err(e) = cleanup_outbound(config, &protected).await {
                warn!(error = %e, ctx = ctx, "outbound cleanup failed");
            }
            if let Err(e) = enforce_media_quota(config, &protected).await {
                warn!(error = %e, ctx = ctx, "media quota enforcement failed");
            }
        }
        Err(e) => {
            warn!(error = %e, ctx = ctx, "outbox query failed, skipping media cleanup passes to protect retry media");
        }
    }
    if let Err(e) = cleanup_logs(config).await {
        warn!(error = %e, ctx = ctx, "log cleanup failed");
    }
    if let Err(e) = cleanup_database(db_pool, config).await {
        warn!(error = %e, ctx = ctx, "database cleanup failed");
    }
    if let Err(e) = backup_database(db_pool, config).await {
        warn!(error = %e, ctx = ctx, "database backup failed");
    }
}

/// Run all cleanup tasks once (periodic schedule).
pub async fn run_all_cleanup(config: &Config, db_pool: &Db) {
    run_cleanup_tasks(config, db_pool, "cleanup").await;
}

/// Collect all media paths referenced by the outbox (path + thumb_path), used
/// to protect entries that have not finished retrying from being mistakenly
/// deleted by cleanup. Returns Err on query failure (the caller should skip
/// this cleanup round: an empty protection set would cause media still inside
/// the retry window to be mistakenly deleted).
/// Full query (no limit): during long outages with backlog, a limited query
/// would miss entries that need protection.
async fn outbox_protected_paths(db_pool: &Db) -> anyhow::Result<std::collections::HashSet<PathBuf>> {
    let mut set = std::collections::HashSet::new();
    let media_jsons = db::outbox_all_media_json(db_pool).await?;
    for mj in &media_jsons {
        if let Ok(items) = serde_json::from_str::<Vec<crate::media::MediaItem>>(mj) {
            for item in &items {
                set.insert(item.path.clone());
                if let Some(tp) = &item.thumb_path {
                    set.insert(tp.clone());
                }
            }
        }
    }
    Ok(set)
}

/// Remove outbound date directories older than TTL.
/// Protects media referenced by the outbox (protection set precomputed by the
/// caller): even if a directory is past its TTL, deletion is skipped as long
/// as outbox entries still reference files under that directory (prevents
/// media within the 72h retry window from being cleaned up prematurely).
pub async fn cleanup_outbound(
    config: &Config,
    outbox_protected: &std::collections::HashSet<PathBuf>,
) -> anyhow::Result<u64> {
    let outbound = config.media_dir.join("outbound");
    if !outbound.exists() {
        return Ok(0);
    }
    let ttl_days = (config.outbound_ttl_hours as f64 / 24.0).ceil() as i64;
    let cutoff = chrono::Local::now().date_naive() - chrono::Duration::days(ttl_days + 1);
    let mut removed = 0u64;

    let mut entries = fs::read_dir(&outbound).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if let Ok(date) = chrono::NaiveDate::parse_from_str(&name_str, "%Y-%m-%d") {
            if date < cutoff {
                // Check whether any files under this directory are referenced by the outbox
                let dir_path = entry.path();
                let has_protected = outbox_protected.iter().any(|p| p.starts_with(&dir_path));
                if has_protected {
                    info!(dir = %name_str, "skipping outbound dir: outbox entries still reference files");
                    continue;
                }
                let size = dir_size(&dir_path).await.unwrap_or(0);
                // Only count freed space on successful deletion: counting on
                // failure would over-report (diagnostic only, but keep the
                // accounting accurate), and the directory wasn't actually
                // freed — it will be retried next round.
                if fs::remove_dir_all(dir_path).await.is_ok() {
                    removed += size;
                    info!(dir = %name_str, bytes = size, "removed expired outbound dir");
                } else {
                    warn!(dir = %name_str, "failed to remove expired outbound dir");
                }
            }
        }
    }
    Ok(removed)
}

/// Enforce total media directory size limit.
pub async fn enforce_media_quota(
    config: &Config,
    outbox_protected: &std::collections::HashSet<PathBuf>,
) -> anyhow::Result<u64> {
    let max_bytes = config.media_max_bytes();
    let total = dir_size(&config.media_dir).await?;
    if total <= max_bytes {
        return Ok(0);
    }

    let mut files: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
    for entry in WalkDir::new(&config.media_dir).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            files.push((entry.path().to_path_buf(), mtime, meta.len()));
        }
    }
    files.sort_by_key(|(_, t, _)| *t);

    // outbox_protected (paths referenced by pending retries) is passed in by
    // the caller: those must not be deleted, otherwise MediaOnly retries lose
    // their media.
    let mut freed = 0u64;
    let mut remaining = total;
    // Protect recently written files: media files from a fetch cycle that is
    // still running have not entered the outbox yet (in-flight); quota
    // cleanup deletes oldest-first by mtime and could delete a file that was
    // just downloaded and about to be sent. Skipping files modified within
    // the last hour avoids this race.
    let recent_cutoff = SystemTime::now() - Duration::from_secs(3600);
    for (path, mtime, size) in files {
        if remaining <= max_bytes {
            break;
        }
        if outbox_protected.contains(&path) {
            continue;
        }
        if mtime >= recent_cutoff {
            continue;
        }
        // Only deduct from the quota on successful deletion: deducting on
        // failure would over-report freed space and stop cleanup prematurely.
        match fs::remove_file(&path).await {
            Ok(()) => {
                remaining -= size;
                freed += size;
                warn!(path = ?path, "removed file due to quota");
            }
            Err(e) => {
                warn!(path = ?path, error = %e, "failed to remove file for quota");
            }
        }
    }
    Ok(freed)
}

/// Remove log files older than retention period.
pub async fn cleanup_logs(config: &Config) -> anyhow::Result<u64> {
    let log_dir = &config.log_dir;
    if !log_dir.exists() {
        return Ok(0);
    }
    let ttl = Duration::from_secs((config.log_retention_days as u64) * 86400);
    let now = SystemTime::now();
    let mut removed = 0u64;

    let mut entries = fs::read_dir(log_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let meta = match entry.metadata().await {
            Ok(m) => m,
            Err(_) => continue, // A single failure does not abort the whole round
        };
        if meta.is_file() {
            if let Ok(modified) = meta.modified() {
                if now.duration_since(modified).unwrap_or_default() > ttl {
                    let size = meta.len();
                    // Only count freed space on success (consistent with
                    // cleanup_outbound); failures leave a warning trace.
                    match fs::remove_file(entry.path()).await {
                        Ok(()) => {
                            removed += size;
                            info!(path = ?entry.path(), "removed old log file");
                        }
                        Err(e) => {
                            warn!(path = ?entry.path(), error = %e, "failed to remove old log file");
                        }
                    }
                }
            }
        }
    }
    Ok(removed)
}

/// Clean up database tables.
pub async fn cleanup_database(db_pool: &Db, config: &Config) -> anyhow::Result<()> {
    let r1 = db::cleanup_sent_entries(db_pool, config.sent_entries_retention_days).await?;
    // The file_id cache is cleaned by retention period (default 30 days);
    // sent_messages is trimmed by row-count cap, so no time-based cleanup needed.
    let r2 = db::cleanup_file_id_cache(db_pool, config.file_id_cache_retention_days).await?;
    // media_dl_fail: keys are only removed at two points — "download
    // succeeded" and "gave up after hitting the limit". If an entry rolls
    // out of the source's rolling window before hitting the limit, neither
    // point ever fires and the key lingers forever. A retention-period
    // cleanup on updated_at acts as the safety net (keys actively being
    // retried refresh updated_at on every failed round, so they're unaffected).
    let r3 = db::cleanup_stale_settings(db_pool, "media_dl_fail:", 7).await?;
    // Note: teloxide_dialogues table has no timestamp column.
    // Dialogue cleanup is handled via chat purge events instead.
    info!(
        sent_entries_deleted = r1,
        file_id_cache_deleted = r2,
        stale_settings_deleted = r3,
        "database cleanup done"
    );
    Ok(())
}

/// Daily database backup: first run a WAL checkpoint(TRUNCATE) to flush data
/// back into the main database file, then copy the main file to obtain a
/// consistent snapshot. At most one backup per day; keep the last N days.
/// Backups are placed under backups/ in the same directory as the DB
/// (inside the container: /app/data/backups, persisted with the data
/// volume).
pub async fn backup_database(db_pool: &Db, config: &Config) -> anyhow::Result<()> {
    let Some(data_dir) = config.db_path.parent() else {
        return Ok(());
    };
    let backups = data_dir.join("backups");
    fs::create_dir_all(&backups).await?;

    // Clean up orphaned .tmp files left behind by a crash (the normal flow
    // leaves none after the rename).
    if let Ok(mut rd) = fs::read_dir(&backups).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let n = e.file_name();
            if n.to_string_lossy().ends_with(".db.tmp") {
                fs::remove_file(e.path()).await.ok();
                info!(file = %n.to_string_lossy(), "removed stale backup tmp file");
            }
        }
    }

    let today = chrono::Local::now().format("%Y%m%d").to_string();
    let target = backups.join(format!("rss_bot_{}.db", today));
    if !target.exists() {
        // Checkpoint first: flush the entire WAL back into the main database
        // file and truncate the WAL; afterwards, copying the main database
        // file directly yields a consistent snapshot (equivalent to VACUUM
        // INTO, and without dynamic SQL).
        // Note: when concurrent read connections exist, the checkpoint
        // completes only partially with busy=1 but does NOT return an error;
        // you must read the busy column of the result row to check it
        // explicitly. Relying solely on execute()'s Err cannot catch this
        // case and would produce a stale backup missing the latest WAL data.
        let row = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_one(db_pool)
            .await?;
        let busy: i64 = row.try_get("busy").unwrap_or(1);
        if busy != 0 {
            anyhow::bail!("wal_checkpoint reported busy (concurrent readers), backup skipped");
        }
        // Copy to a temp file first, then rename (atomic replacement): a
        // mid-copy failure won't leave a half-written file behind; otherwise
        // target.exists() would make all later retries for the day skip
        // permanently.
        let tmp = backups.join(format!("rss_bot_{}.db.tmp", today));
        fs::copy(&config.db_path, &tmp).await?;
        fs::rename(&tmp, &target).await?;
        info!(backup = %target.display(), "database backed up");
    }

    // Clean up old backups past the retention period (judged by the date in
    // the filename). Retention days must be at least 1: a negative value
    // would push the cutoff into the future and delete all backups.
    let retention = config.backup_retention_days.max(1);
    let cutoff = chrono::Local::now().date_naive() - chrono::Duration::days(retention);
    let mut entries = fs::read_dir(&backups).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let date_part = name_str
            .strip_prefix("rss_bot_")
            .and_then(|s| s.strip_suffix(".db"));
        if let Some(d) = date_part {
            if let Ok(date) = chrono::NaiveDate::parse_from_str(d, "%Y%m%d") {
                if date < cutoff {
                    match fs::remove_file(entry.path()).await {
                        Ok(()) => info!(backup = %name_str, "removed old database backup"),
                        Err(e) => warn!(backup = %name_str, error = %e, "failed to remove old database backup"),
                    }
                }
            }
        }
    }
    Ok(())
}

/// Startup cleanup: run housekeeping once so a long-stopped bot starts tidy.
/// Each failure logs a warn (consistent with run_all_cleanup): the first
/// startup after a long shutdown is exactly when these cleanups most need to
/// be visible, so they must not be silently swallowed with let _ =. A backup
/// is also taken at startup (automatically skipped if one already exists for
/// the day), without waiting for the hourly cleanup job.
pub async fn run_startup_cleanup(config: &Config, db_pool: &Db) {
    run_cleanup_tasks(config, db_pool, "startup cleanup").await;
}

/// Recursive directory size.
pub async fn dir_size(path: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries = fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let meta = entry.metadata().await?;
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    Ok(total)
}
