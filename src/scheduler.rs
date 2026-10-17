use teloxide::prelude::*;
use teloxide::types::ChatId;
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{debug, error, info, warn};

use crate::db;
use crate::fetcher;
use crate::i18n::{self, Lang};
use crate::push::{self, ContentMode};
use crate::sendpolicy;
use crate::state::AppState;

/// Register the RSS fetch job on the scheduler.
pub async fn register_fetch_job(
    sched: &JobScheduler,
    state: AppState,
    bot: Bot,
) -> anyhow::Result<()> {
    let interval = state.config.scheduler_interval_minutes;
    // The cron minute field only accepts steps of 1-59: large intervals (such
    // as the 3/6/12-hour options in the panel) cannot be written directly as
    // */180 — that is an invalid expression. Converge uniformly to a trigger
    // frequency of one tick per 1-5 minutes; whether each source is due is
    // decided precisely by the get_due_subscriptions SQL using its own interval.
    let tick = interval.clamp(1, 5);
    let expr = format!("0 */{} * * * *", tick);

    sched
        .add(Job::new_async(expr.as_str(), move |_uuid, _l| {
            let state = state.clone();
            let bot = bot.clone();
            Box::pin(async move {
                if let Err(e) = run_fetch_cycle(&state, &bot).await {
                    error!(error = %e, "fetch cycle failed");
                }
            })
        })?)
        .await?;

    info!(interval_min = interval, tick_min = tick, "fetch job registered");
    Ok(())
}

/// Register the auto-heal job: periodically re-enable subscriptions that were
/// auto-disabled by consecutive fetch failures, once the cooldown has passed.
/// A network-wide outage (proxy down, firewall blocking, etc.) causes all
/// sources to be auto-disabled one after another; once the network recovers,
/// this job brings them back automatically with no manual intervention.
/// Manually paused sources (auto_disabled=0) are unaffected. Silent when
/// healthy — only writes logs.
pub async fn register_heal_job(sched: &JobScheduler, state: AppState) -> anyhow::Result<()> {
    // Check once at the top of every hour; cooldown is 3 hours (see
    // AUTO_HEAL_COOLDOWN_HOURS).
    const AUTO_HEAL_COOLDOWN_HOURS: i64 = 3;
    let expr = "0 0 * * * *";

    sched
        .add(Job::new_async(expr, move |_uuid, _l| {
            let state = state.clone();
            Box::pin(async move {
                match db::reenable_due_auto_disabled(&state.db, AUTO_HEAL_COOLDOWN_HOURS).await {
                    Ok(0) => {}
                    Ok(n) => info!(count = n, "auto-heal: re-enabled subscriptions after cooldown"),
                    Err(e) => error!(error = %e, "auto-heal job failed"),
                }
            })
        })?)
        .await?;

    info!(cooldown_hours = AUTO_HEAL_COOLDOWN_HOURS, "auto-heal job registered");
    Ok(())
}

/// RAII guard clearing an active-flag on every exit path (cycle / outbox retry).
struct ActiveGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Read the platform hysteresis streak counter (positive = failure streak,
/// negative = success streak). Returns Some(n) for a valid stored value (or
/// 0 when the key is absent — that genuinely means "no streak yet"), or None
/// on a transient DB read error. Callers must NOT write the streak back when
/// this returns None: collapsing a read error to 0 and then writing 0 would
/// reset an accumulated streak on a single DB hiccup (the LESSONS rule "a DB
/// read error must not unwrap_or to a neutral value" — for counters, skip the
/// round instead). A missing key still yields Some(0) so the normal
/// "first failure → 1" flow proceeds.
async fn read_streak(state: &AppState, key: &str, platform: &'static str) -> Option<i64> {
    match db::get_setting(&state.db, key).await {
        Ok(v) => Some(v.and_then(|s| s.parse().ok()).unwrap_or(0)),
        Err(e) => {
            warn!(platform = platform, error = %e, "streak read failed, deferring to next round");
            None
        }
    }
}

/// Per-round platform-level stats (global — no longer isolated per owner
/// since cookies were globalized). Bundled into a struct to avoid too many
/// arguments on process_feed (clippy too_many_arguments).
#[derive(Default)]
struct PlatRoundStats {
    /// Platforms scheduled this round (regardless of success/failure)
    seen: tokio::sync::Mutex<std::collections::HashSet<&'static str>>,
    /// Platforms that had failures this round
    failed: tokio::sync::Mutex<std::collections::HashSet<&'static str>>,
    /// Platforms that had successes this round
    ok: tokio::sync::Mutex<std::collections::HashSet<&'static str>>,
    /// Last error message (for display in anomaly notifications)
    last_err: tokio::sync::Mutex<std::collections::HashMap<&'static str, String>>,
}

/// One full fetch cycle: find due subscriptions, fetch, push new entries.
/// Media/pipeline warnings notify the channel owner immediately
/// (notify_owner_html) instead of being batched until the end of the cycle.
async fn run_fetch_cycle(state: &AppState, bot: &Bot) -> anyhow::Result<()> {
    // If the previous round has not finished (a round can exceed the trigger
    // interval when media downloads are slow), skip this tick to avoid two
    // concurrent rounds both passing the is_entry_sent check and pushing the
    // same entry twice.
    if state
        .fetch_cycle_active
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::Acquire,
            std::sync::atomic::Ordering::Acquire,
        )
        .is_err()
    {
        warn!("previous fetch cycle still running, skipping this tick");
        return Ok(());
    }
    let _guard = ActiveGuard(state.fetch_cycle_active.clone());

    let due = db::get_due_subscriptions(&state.db).await?;
    if due.is_empty() {
        return Ok(());
    }
    info!(count = due.len(), "fetch cycle: due subscriptions");

    // Per-round platform-level stats: seen = platforms scheduled this round,
    // failed = platforms with failures this round, ok = platforms with
    // successes, last_err = last error message.
    // Used for whole-round hysteresis decisions, avoiding anomaly/recovery
    // notification spam caused by single-round jitter.
    // Criterion: a round counts as a failure round only if ALL sources failed,
    // and as a success round only if ALL succeeded (mixed rounds reset the
    // streak), preventing one permanently failing source (when an owner has
    // multiple sources) from dragging the whole platform into a persistent
    // down state that never recovers.
    let stats = std::sync::Arc::new(PlatRoundStats::default());

    // Separate platform sources (weibo/X) from plain RSS sources:
    // platform sources run serially with random gaps (reducing the chance of
    // triggering risk control); plain sources run in parallel.
    let (platform_subs, rss_subs): (Vec<_>, Vec<_>) = due
        .into_iter()
        .partition(|(sub, _)| platform_of_url(&sub.feed_url).is_some());

    // Plain RSS sources: per-source parallelism (cap taken from the
    // FETCH_CONCURRENCY config, default 8).
    // H3 fix: the previous hardcoded 3 made the FETCH_CONCURRENCY env var
    // ineffective (the outer layer limited to 3 first, so the inner
    // fetch_semaphore never reached its cap). Now the config value is used
    // uniformly and the env var actually takes effect.
    // The permit is acquired inside the spawned task: the main loop is not
    // blocked, so the serial platform-source loop can progress in parallel
    // with the RSS sources.
    let feed_sem = std::sync::Arc::new(tokio::sync::Semaphore::new(state.config.fetch_concurrency));
    // Each handle is paired with a copy of the feed_url: once sub is moved
    // into the spawn closure, the source identifier can no longer be obtained
    // at the join site and failure logs would lose their locator info
    // (added in v1.10.7).
    let mut handles: Vec<(String, tokio::task::JoinHandle<anyhow::Result<()>>)> = Vec::new();
    for (idx, (sub, cfg)) in rss_subs.into_iter().enumerate() {
        let sem = feed_sem.clone();
        let state = state.clone();
        let bot = bot.clone();
        let stats = stats.clone();
        let feed_url = sub.feed_url.clone();
        handles.push((feed_url, tokio::spawn(async move {
            // Staggered start: avoid all sources hitting their respective
            // servers/proxies concurrently at the same instant, forming a
            // momentary burst (inspired by rsstt's sharded staggered
            // scheduling, simplified to sequential delays at this scale).
            if idx > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(600 * idx as u64)).await;
            }
            let permit = match sem.acquire_owned().await {
                Ok(p) => p,
                Err(_) => return Ok(()),
            };
            let res = process_feed(&state, &bot, sub, cfg, &stats).await;
            drop(permit);
            res
        })));
    }

    // Platform sources (weibo/X): strictly serial + random 3-8 second wait
    // before each request. Weibo risk control is sensitive to burst
    // concurrency; serial execution + random gaps effectively lowers the 403
    // probability.
    let mut first_platform = true;
    for (sub, cfg) in platform_subs {
        // Non-first platform source: random 3-8 second delay (pseudo-random
        // via nanosecond modulo, no rand dependency needed)
        if !first_platform {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos();
            let delay_ms = 3000 + (nanos % 5000) as u64;
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        first_platform = false;
        // sub is moved into process_feed; failure logs need the source
        // identifier, so extract it first (v1.10.7).
        let feed_url = sub.feed_url.clone();
        let chat_id = sub.chat_id.clone();
        if let Err(e) = process_feed(state, bot, sub, cfg, &stats).await {
            error!(feed = %feed_url, chat_id = %chat_id, error = %e, "platform feed processing failed");
        }
    }

    // Wait for the plain-source tasks to finish
    for (feed_url, handle) in handles {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => error!(feed = %feed_url, error = %e, "feed processing failed"),
            Err(e) => error!(feed = %feed_url, error = %e, "feed task join failed"),
        }
    }

    // Platform-level health hysteresis (whole-round view, serial so no race,
    // global per platform):
    // report an anomaly only after 2 consecutive all-failure rounds; report
    // recovery only after 2 consecutive all-success rounds.
    // This avoids "anomaly → recovery" notification spam caused by single-round
    // network jitter.
    // Criterion (v1.4.3 fix): a "failure round" = all of this platform's
    // sources failed this round; a "success round" = all succeeded; mixed
    // rounds reset the streak. Previously any single source failure marked the
    // round as failed, so with multiple sources one permanently failing source
    // (e.g. account deleted) would drag the whole platform into a persistent
    // down state that the other healthy sources could never pull back.
    // v1.11.0: after cookies were globalized, the health stats were globalized
    // accordingly (no longer isolated per owner); anomaly/recovery
    // notifications go to all admins.
    const HYSTERESIS_ROUNDS: i64 = 2;
    let seen = stats.seen.lock().await.clone();
    let failed = stats.failed.lock().await.clone();
    let ok_set = stats.ok.lock().await.clone();
    let last_err = stats.last_err.lock().await.clone();
    for &p in &seen {
        let streak_key = db::setting_key::platform_streak(p);
        let health_key = db::setting_key::platform_health(p);
        let is_down = matches!(
            db::get_setting(&state.db, &health_key).await,
            Ok(Some(v)) if v == "down"
        );

        let has_failed = failed.contains(&p);
        let has_ok = ok_set.contains(&p);
        // Nature of this round: all-failed / all-ok / mixed (or no valid signal)
        let all_failed = has_failed && !has_ok;
        let all_ok = has_ok && !has_failed;

        if all_failed {
            // All failed this round: increment the failure streak counter
            // (positive = failure streak, starting from 1; if the previous
            // direction was success (negative), reset to 1).
            // On a transient read error (None), skip both the write-back and
            // the down-notification this round — the next round re-reads and
            // continues accumulating, preserving the streak value.
            if let Some(prev) = read_streak(state, &streak_key, p).await {
                let streak = if prev > 0 { prev + 1 } else { 1 };
                if let Err(e) = db::set_setting(&state.db, &streak_key, &streak.to_string()).await {
                    warn!(platform = p, error = %e, "streak write failed");
                }

                // Hysteresis threshold reached and not currently down: report
                // the anomaly (notify all admins)
                if streak >= HYSTERESIS_ROUNDS && !is_down {
                    let err = last_err.get(&p).map(String::as_str).unwrap_or("unknown error");
                    maybe_notify_platform_down(bot, state, p, err).await;
                }
            }
        } else if all_ok {
            // All succeeded this round: increment the success streak counter.
            // Same "defer on read error" rule as the failure branch.
            if let Some(streak) = read_streak(state, &streak_key, p).await {
                // The success direction counts with negative numbers (-1, -2, ...)
                let ok_streak = if streak < 0 { streak - 1 } else { -1 };
                if let Err(e) = db::set_setting(&state.db, &streak_key, &ok_streak.to_string()).await {
                    warn!(platform = p, error = %e, "streak write failed");
                }

                // Hysteresis threshold reached and currently down: report recovery
                // (notify all admins)
                if ok_streak <= -HYSTERESIS_ROUNDS && is_down {
                    maybe_notify_platform_recovered(bot, state, p).await;
                }
            }
        } else {
            // Mixed round (both successes and failures): reset the streak,
            // leave the health state unchanged.
            if let Err(e) = db::set_setting(&state.db, &streak_key, "0").await {
                warn!(platform = p, error = %e, "streak reset failed");
            }
        }
    }

    Ok(())
}

/// Cooperative deadline for processing a single source (seconds). After the
/// timeout, no new entry starts processing; remaining entries are left to the
/// next round (not marked sent, so the next round's re-fetch naturally picks
/// them up). This is "cooperative" rather than a hard cancellation: an entry
/// already being processed is allowed to finish (its internal download/send
/// each have their own timeout bounds), avoiding cutting off an in-flight
/// send and causing a duplicate push. The main purpose is protecting platform
/// sources' sliding windows — one stuck source slows the whole round, and if
/// it drags on long enough, other sources' entries roll out of the sliding
/// window, which is the real permanent loss.
const FEED_DEADLINE_SECS: u64 = 240;

/// Process a single source: fetch + push new entries in chronological order.
/// Parallel across sources, serial within a source.
/// All failure notifications are sent immediately, no longer batched until the
/// end of the cycle.
/// `stats` records this round's platform-level scheduling and success/failure
/// outcomes, for run_fetch_cycle to perform hysteresis evaluation after the
/// whole round ends.
async fn process_feed(
    state: &AppState,
    bot: &Bot,
    sub: db::SubRow,
    cfg: db::EffectiveConfig,
    stats: &PlatRoundStats,
) -> anyhow::Result<()> {
    let transport = crate::media::MediaTransport::new(&state.config);

    // Resolve the chat owner early: used for owner-routed notifications
    // (auto-disable / media failures / oversized video) and language preference.
    // On a DB query error, log a warn and treat it as "no owner" (do not
    // silently swallow the error).
    // v1.11.0: after cookies were globalized, platform-source fetching no
    // longer depends on the owner; channels without an owner can also fetch
    // normally.
    let owner_id = match db::get_chat_owner(&state.db, &sub.chat_id).await {
        Ok(o) => o,
        Err(e) => {
            warn!(chat_id = %sub.chat_id, error = %e, "get_chat_owner failed, treating as no owner");
            None
        }
    };

    // Owner's language preference (for notification copy i18n); no owner
    // defaults to English.
    let lang = match owner_id {
        Some(o) => owner_lang(state, o).await,
        None => Lang::En,
    };

    // Record the platforms scheduled this round (regardless of outcome), for
    // whole-round recovery evaluation (global criterion).
    if let Some(p) = platform_of_url(&sub.feed_url) {
        stats.seen.lock().await.insert(p);
    }

    // v1.10.3: debounce retry for transient errors on RSS sources (1 retry,
    // 2s backoff). Applies only to non-platform sources — platform sources
    // (weibo/x/telegram) have serial delays and risk-control strategies, and
    // blind retries may trigger rate limiting. Typical scenario: transient
    // proxy/network connection errors that recover on their own after a few
    // seconds; a single retry can save the round.
    let is_rss_source = platform_of_url(&sub.feed_url).is_none();
    let result = fetcher::fetch_feed(
        state,
        &sub.feed_url,
        sub.http_etag.as_deref(),
        sub.http_last_modified.as_deref(),
    )
    .await;
    let result = match result {
        // Render the anyhow error chain with `{:#}` (alternates Display):
        // fetcher::fetch_feed wraps reqwest errors via `.with_context(...)`
        // so plain `e.to_string()` returns only the outer "request failed:
        // <url>" and the inner reqwest source string ("error sending request
        // for url ...", "connection closed", etc.) is dropped — is_net_error
        // would then never match on RSS connection-class failures. `{:#}`
        // appends the full source chain separated by ": ", letting the
        // substring check see the underlying reqwest error text.
        Err(ref e) if is_rss_source && sendpolicy::is_net_error(&format!("{:#}", e)) => {
            warn!(
                feed = %sub.feed_url,
                error = %e,
                "fetch transient error, retrying once after 2s"
            );
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            fetcher::fetch_feed(
                state,
                &sub.feed_url,
                sub.http_etag.as_deref(),
                sub.http_last_modified.as_deref(),
            )
            .await
        }
        other => other,
    };

    match result {
        Ok(fr) => {
            // Reset error count on success (a 304 "source not updated" also
            // counts as success).
            // Do not use `?`: a single DB hiccup should not invalidate the
            // whole round's fetch results (otherwise this round's entries
            // would all go unpushed and health stats would be missing); a
            // failed reset merely leaves a trace and is retried next round.
            if let Err(e) = db::reset_error_count(&state.db, sub.id).await {
                warn!(feed = %sub.feed_url, error = %e, "reset error count failed");
            }
            // After a successful fetch, clear the disable-notification flag so
            // the next failure can notify again (#9 fix)
            let notify_key = db::setting_key::auto_disable_notified(&sub.chat_id, &sub.feed_url);
            db::delete_setting(&state.db, &notify_key).await.ok();

            // Record that this platform had a success this round (for the
            // whole-round hysteresis all-success/mixed criterion).
            if let Some(p) = platform_of_url(&sub.feed_url) {
                stats.ok.lock().await.insert(p);
            }

            // 304 Not Modified: the source has no updates, nothing to push
            if fr.not_modified {
                return Ok(());
            }

            // Backfill/refresh feed title when not manually overridden.
            // Covers subscriptions added while fetching was broken (title fell
            // back to the raw URL) and feeds whose title changes later.
            if sub.title_overridden == 0 {
                if let Some(t) = &fr.feed_title {
                    if Some(t.as_str()) != sub.feed_title.as_deref() {
                        db::rename_subscription_auto(&state.db, sub.id, t).await.ok();
                    }
                }
            }

            // Push new entries (skip already-sent)
            let feed_title = sub.feed_title.clone().unwrap_or_default();
            let opts = push::RenderOpts {
                mode: ContentMode::from_str(&cfg.content_mode),
                source: push::SourceDisplay::from_str(&cfg.source_display),
                link_preview: cfg.link_preview,
            };
            let chat_id: i64 = match cfg.chat_id.parse() {
                Ok(id) => id,
                Err(_) => {
                    // chat_id is not numeric (e.g. legacy dirty data): silently
                    // sending to chat 0 would fail forever and be hard to
                    // debug, so warn explicitly and skip this subscription's
                    // push for this round.
                    warn!(chat_id = %cfg.chat_id, feed = %cfg.feed_url, "invalid chat_id, skipping subscription");
                    return Ok(());
                }
            };
            // Channel title for notices (owner already resolved at function top).
            let chat_title = db::get_chat(&state.db, &cfg.chat_id)
                .await
                .ok()
                .flatten()
                .and_then(|c| c.title)
                .unwrap_or_else(|| cfg.chat_id.clone());

            // Content filter config (feed level): takes effect when the switch
            // is on and the keywords are non-empty.
            // Distinguish "DB error" from "no filter configured": treating an
            // error as default (empty filter) means every entry passes and the
            // blocklist silently stops working (the LESSONS "DB read error must
            // not unwrap_or to a neutral value" rule). Fail-open here is
            // preferable to aborting the whole round, but must leave a trace.
            let filter_cfg = match db::get_feed_filter(&state.db, &cfg.chat_id, &cfg.feed_url).await {
                Ok(cfg) => cfg,
                Err(e) => {
                    warn!(feed = %cfg.feed_url, error = %e, "feed filter read failed, treating as no-filter for this round");
                    Default::default()
                }
            };

            // First filter out unsent entries, then push in ascending order of
            // publish time (old → new). RSS feeds are mostly "newest first";
            // pushing in the original order would invert the timeline of the
            // channel's message stream.
            let mut pending: Vec<&feed_rs::model::Entry> = Vec::new();
            for entry in &fr.entries {
                let eid = fetcher::entry_id(entry);
                if db::is_entry_sent(&state.db, &cfg.chat_id, &cfg.feed_url, &eid).await? {
                    continue;
                }
                // Cross-cycle double-send guard: the fetch cycle and the outbox
                // retry cycle run under independent AtomicBool locks and can
                // overlap. An entry that failed a previous round is queued in
                // the outbox but not yet in sent_entries (mark happens only
                // after a successful send), so both cycles would otherwise pass
                // is_entry_sent and push it concurrently. Skip entries the
                // outbox already owns — the outbox has sole retry authority.
                if db::is_in_outbox(&state.db, &cfg.chat_id, &cfg.feed_url, &eid).await {
                    continue;
                }
                // Content filter: entries whose title matches a rule are marked
                // as sent directly (never pushed), avoiding repeated checks
                // every round eating into the fetch window.
                if !filter_cfg.allows_title(&fetcher::entry_title(entry)) {
                    db::mark_entry_sent(&state.db, &cfg.chat_id, &cfg.feed_url, &eid).await?;
                    continue;
                }
                pending.push(entry);
            }
            // Sort only when every entry has a timestamp (stable sort, equal
            // timestamps keep the feed's original order); sources missing
            // timestamps keep the feed's original order — no risky reordering.
            if pending.len() > 1
                && pending.iter().all(|e| e.published.or(e.updated).is_some())
            {
                pending.sort_by_key(|e| e.published.or(e.updated));
            }

            // Cooperative deadline: after the timeout, no new entry starts
            // processing (remaining entries continue next round).
            let deadline = tokio::time::Instant::now()
                + std::time::Duration::from_secs(FEED_DEADLINE_SECS);

            // True when the entry loop was cut short by the deadline. The
            // response's negotiation headers are only saved when the loop
            // drained fully (see after the loop): saving them while some
            // entries were deferred would let the next round's conditional
            // request return 304 and permanently swallow the unpushed entries
            // (the v1.11.2 "deadline → clear headers" fix only covered the
            // deadline break, not the `?`-propagation early returns inside
            // the loop; deferring the save covers all interruption paths at
            // once). A `?` abort returns before this point too, so headers
            // stay at the previous round's value and the next round re-fetches
            // the full body (sent_entries dedup prevents double-push).
            let mut deferred = false;
            for entry in pending {
                if tokio::time::Instant::now() >= deadline {
                    warn!(
                        feed = %cfg.feed_url,
                        "feed processing deadline reached, remaining entries deferred to next cycle"
                    );
                    deferred = true;
                    break;
                }
                let eid = fetcher::entry_id(entry);
                // Group-C sidecar: look up structured metadata by entry_id
                // (populated by platform sources, always empty for RSS sources).
                let meta = fr.metas.get(&eid);

                // Title+link / title-only modes: never fetch media, always take
                // the pure-text path.
                let prepared = if opts.mode.skip_media() {
                    crate::media::PreparedMedia::empty()
                } else {
                    // Reuse the round-level owner language resolved once at the
                    // top of process_feed; re-querying the settings row per
                    // entry wasted a DB round-trip and fell back to En via a
                    // different (owner_id.unwrap_or(0)) path.
                    crate::media::prepare_entry_media(state, entry, &transport, meta, lang).await
                };
                let download_warnings = prepared.warnings;

                // Oversized videos: stash for now, record after a successful
                // send (the TG message link is needed).
                let oversized = prepared.oversized_videos;

                // Media-only mode: silently skip when there is no media.
                // Mark as sent only when the entry itself has no media at all
                // (no download warnings), avoiding repeated re-checks later;
                // if there was media but all downloads failed (warnings
                // non-empty), do not mark it — leave it for the next round's
                // retry, preventing the entry from being silently lost.
                // Retry cap is 10: once exceeded, mark as sent and notify the
                // owner to prevent an infinite loop.
                if opts.mode == ContentMode::MediaOnly && prepared.items.is_empty() {
                    // Oversized videos: notify the owner immediately (media-only
                    // mode, entry not pushed).
                    let entry_link = fetcher::entry_link(entry).unwrap_or_default();
                    for ov in &oversized {
                        notify_oversized_video(bot, lang, owner_id, &chat_title, &entry_link, None, ov).await;
                    }
                    // Download failures: notify immediately.
                    if !download_warnings.is_empty() {
                        const MEDIA_DL_MAX_RETRIES: i64 = 10;
                        let retry_key = db::setting_key::media_dl_fail(&cfg.chat_id, &cfg.feed_url, &eid);
                        // Read the current counter, distinguishing a genuine
                        // DB error from a missing/invalid value. On a DB error we
                        // must NOT treat it as 0: that would reset the failure
                        // streak every time the DB hiccups, so the 10-retry cap
                        // is never reached and the entry retries forever. Skip
                        // the counter step this round and let the next round
                        // re-read (the entry is not marked sent, so it is safe).
                        let stored = match db::get_setting(&state.db, &retry_key).await {
                            Ok(v) => v.and_then(|s| s.parse::<i64>().ok()).unwrap_or(0),
                            Err(e) => {
                                warn!(entry_id = %eid, feed = %cfg.feed_url, error = %e, "media_dl_fail counter read failed, deferring to next round");
                                continue;
                            }
                        };
                        let fail_count: i64 = stored + 1;
                        if fail_count >= MEDIA_DL_MAX_RETRIES {
                            // Cap reached: abandon this entry and notify the
                            // owner.
                            warn!(entry_id = %eid, feed = %cfg.feed_url, count = fail_count, "MediaOnly download retries exhausted, abandoning");
                            let msg = push_fail_msg(
                                lang,
                                i18n::t(lang, "notif_abandoned"),
                                &feed_title,
                                &i18n::t(lang, "notif_media_dl_fail").replacen("{}", &fail_count.to_string(), 1),
                                "label_entry",
                                &entry_link,
                            );
                            notify_owner_html(bot, state, &cfg.chat_id, &msg).await;
                            // Delete the counter key BEFORE marking sent: if the
                            // mark fails (and `?` returns), the counter is
                            // already cleared, so the next round re-counts from 0
                            // instead of re-triggering the abandon branch every
                            // round and spamming the owner with "abandoned".
                            db::delete_setting(&state.db, &retry_key).await.ok();
                            db::mark_entry_sent(&state.db, &cfg.chat_id, &cfg.feed_url, &eid).await?;
                        } else {
                            db::set_setting(&state.db, &retry_key, &fail_count.to_string()).await.ok();
                            notify_download_failed(
                                bot,
                                lang,
                                owner_id,
                                &feed_title,
                                &download_warnings,
                                &i18n::t(lang, "notif_dl_note")
                                    .replacen("{}", &fail_count.to_string(), 1)
                                    .replacen("{}", &MEDIA_DL_MAX_RETRIES.to_string(), 1),
                                None,
                                &entry_link,
                            )
                            .await;
                        }
                    }
                    if download_warnings.is_empty() {
                        db::mark_entry_sent(&state.db, &cfg.chat_id, &cfg.feed_url, &eid).await?;
                    }
                    continue;
                }

                // Reaching here in MediaOnly means the media download succeeded
                // (items non-empty): clear the retry counter key, preventing
                // keys written by earlier failed rounds from lingering in the
                // settings table forever.
                if opts.mode == ContentMode::MediaOnly {
                    let retry_key = db::setting_key::media_dl_fail(&cfg.chat_id, &cfg.feed_url, &eid);
                    db::delete_setting(&state.db, &retry_key).await.ok();
                }

                let author = fetcher::entry_author(entry);
                // No whole-entry retry for sending (#11 fix: removed the
                // SEND_ATTEMPTS=1 infinite loop): send_entry_msg (text
                // segments) and send_entry_with_media (media ops) already have
                // segment-level/op-level resume internally (only the failed
                // current segment is resent, delivered segments are not).
                // A whole-entry retry would resend already-delivered
                // segments/ops under compound failures, producing duplicate
                // messages. Persistent failures are caught by the outbox
                // persistent retry queue (exponential backoff, abandon at 72h).
                let result: anyhow::Result<push::SendOutcome> = if prepared.items.is_empty() {
                    // The pure-text path uniformly goes through send_text_only
                    // (H1 fix: it used to be a verbatim duplicate of the outbox
                    // retry path; the poll-resend logic was fixed twice because
                    // of this, v1.9.3/v1.9.6).
                    push::send_text_only(bot, chat_id, entry, &feed_title, author.as_deref(), opts, meta).await
                } else {
                    push::send_entry_with_media(
                        bot,
                        &state.db,
                        chat_id,
                        push::PushCtx {
                            entry,
                            feed_title: &feed_title,
                            author: author.as_deref(),
                            opts,
                            caption_above_media: cfg.caption_above_media,
                            aspect_threshold: state.config.long_image_aspect_ratio,
                            media: &prepared.items,
                            meta,
                        },
                        &transport,
                    )
                    .await
                };

                match result {
                    Ok(send_outcome) => {
                        let entry_link = fetcher::entry_link(entry).unwrap_or_default();
                        // Download failures: notify the owner immediately
                        // (including the delivered-message link and the source
                        // entry link).
                        if !download_warnings.is_empty() {
                            notify_download_failed(
                                bot,
                                lang,
                                owner_id,
                                &feed_title,
                                &download_warnings,
                                "",
                                send_outcome.delivered_tg_link.as_deref(),
                                &entry_link,
                            )
                            .await;
                        }
                        // Media send failed (text delivered but media not
                        // attached): notify immediately.
                        if send_outcome.media_failed {
                            let mut msg = i18n::t(lang, "notif_media_failed")
                                .replacen("{}", &push::escape_html(&chat_title), 1);
                            if let Some(tg) = &send_outcome.delivered_tg_link {
                                push_line(&mut msg, lang, "label_message", tg);
                            }
                            if let Some(el) = &send_outcome.entry_link {
                                push_line(&mut msg, lang, "label_entry", el);
                            }
                            notify_owner_html(bot, state, &cfg.chat_id, &msg).await;
                        }
                        // Oversized videos: notify immediately.
                        for ov in &oversized {
                            notify_oversized_video(
                                bot,
                                lang,
                                owner_id,
                                &chat_title,
                                &entry_link,
                                send_outcome.delivered_tg_link.as_deref(),
                                ov,
                            )
                            .await;
                        }
                        // Register the sent message: TG message id → RSS entry,
                        // for /find reverse lookup. Registration failure does
                        // not affect the main push flow.
                        if let Some(mid) = send_outcome.delivered_message_id {
                            db::record_sent_message(
                                &state.db,
                                &cfg.chat_id,
                                mid,
                                &cfg.feed_url,
                                &eid,
                                send_outcome.entry_link.as_deref(),
                                state.config.sent_messages_max_rows,
                            )
                            .await
                            .ok();
                        }
                        // Message already delivered to TG: a persistence failure
                        // must not abort (the original `?` would discard the
                        // "delivered" fact, and the next round's re-fetch would
                        // push it again, producing duplicate messages).
                        // Best-effort + warn.
                        if let Err(e) = db::mark_entry_sent(&state.db, &cfg.chat_id, &cfg.feed_url, &eid).await {
                            warn!(feed = %cfg.feed_url, entry_id = %eid, error = %e, "mark_entry_sent failed after successful delivery (may re-push next cycle)");
                        }
                    }
                    Err(e) => {
                        warn!(feed = %cfg.feed_url, error = %e, "send failed, queuing to outbox");
                        // Push failed: write into the outbox persistent retry
                        // queue (exponential backoff, abandon at 72h).
                        // entry_json is the sole source for deserialization on
                        // retry: if serialization failed and an empty string
                        // were enqueued, the retry would fail to deserialize,
                        // only delete the row without mark_sent, and the entry
                        // would be permanently lost once it rolls out of the
                        // window. So on failure we take the "enqueue failed"
                        // branch (mark_sent + notify owner), leaving no hidden
                        // hazard.
                        let entry_json = match serde_json::to_string(entry) {
                            Ok(j) => j,
                            Err(se) => {
                                error!(feed = %cfg.feed_url, entry_id = %eid, error = %se, "entry_json serialization failed, marking sent");
                                db::mark_entry_sent(&state.db, &cfg.chat_id, &cfg.feed_url, &eid).await?;
                                let elink = fetcher::entry_link(entry).unwrap_or_default();
                                let msg = push_fail_msg(
                                    lang,
                                    i18n::t(lang, "notif_serialize_failed"),
                                    &feed_title,
                                    &push::escape_html(&format!("{:#}", e)),
                                    "label_source",
                                    &elink,
                                );
                                notify_owner_html(bot, state, &cfg.chat_id, &msg).await;
                                continue;
                            }
                        };
                        // A media_json failure degrades to an empty array (the
                        // retry proceeds as text without media); a meta_json
                        // failure degrades to no meta (the retry loses the
                        // poll/prerendered markers) — neither loses the entry,
                        // which is acceptable.
                        let media_json = serde_json::to_string(&prepared.items).unwrap_or_default();
                        // Group-C sidecar: serialize EntryMeta so it can be
                        // restored on retry.
                        let meta_json = meta
                            .map(|m| serde_json::to_string(m).unwrap_or_default())
                            .unwrap_or_default();
                        let now_ts = chrono::Utc::now().timestamp();
                        let next_retry = now_ts + outbox_backoff_secs(0);
                        let entry_link = fetcher::entry_link(entry).unwrap_or_default();
                        match db::outbox_insert(
                            &state.db,
                            &cfg.chat_id,
                            &cfg.feed_url,
                            &eid,
                            &feed_title,
                            &entry_json,
                            &media_json,
                            &meta_json,
                            &cfg.content_mode,
                            &cfg.source_display,
                            cfg.link_preview,
                            cfg.caption_above_media,
                            author.as_deref(),
                            next_retry,
                            now_ts,
                        )
                        .await
                        {
                            Ok(true) => {
                                // Newly enqueued: notify the owner (only notify
                                // on the first failure).
                                let msg = push_fail_msg(
                                    lang,
                                    i18n::t(lang, "notif_queued"),
                                    &feed_title,
                                    &push::escape_html(&format!("{:#}", e)),
                                    "label_source",
                                    &entry_link,
                                );
                                notify_owner_html(bot, state, &cfg.chat_id, &msg).await;
                                // Download-failure/oversized-video alerts are
                                // not persisted: send them at enqueue time,
                                // otherwise these alerts would be lost forever
                                // once the outbox retry succeeds.
                                if !download_warnings.is_empty() {
                                    notify_download_failed(
                                        bot,
                                        lang,
                                        owner_id,
                                        &feed_title,
                                        &download_warnings,
                                        "",
                                        None,
                                        &entry_link,
                                    )
                                    .await;
                                }
                                for ov in &oversized {
                                    notify_oversized_video(
                                        bot,
                                        lang,
                                        owner_id,
                                        &chat_title,
                                        &entry_link,
                                        None,
                                        ov,
                                    )
                                    .await;
                                }
                            }
                            Ok(false) => {
                                // Already in the queue (enqueued by a previous
                                // fetch round): do not notify again, avoiding
                                // the same notice being spammed every round
                                // during a persistent outage.
                                debug!(feed = %cfg.feed_url, entry_id = %eid, "already in outbox, skipping notice");
                            }
                            Err(insert_err) => {
                                // outbox write failed: fall back to marking sent
                                // (prevents infinite retries)
                                error!(feed = %cfg.feed_url, error = %insert_err, "outbox insert failed, marking sent");
                                db::mark_entry_sent(&state.db, &cfg.chat_id, &cfg.feed_url, &eid).await?;
                                let msg = push_fail_msg(
                                    lang,
                                    i18n::t(lang, "notif_enqueue_failed"),
                                    &feed_title,
                                    &push::escape_html(&format!("{:#}", e)),
                                    "label_source",
                                    &entry_link,
                                );
                                notify_owner_html(bot, state, &cfg.chat_id, &msg).await;
                            }
                        }
                    }
                }
                // Send rate limiting is handled by the push module's ChatPacer,
                // which queues uniformly per channel (≥2s/message); no extra
                // sleep is needed here.
            }

            // Save this response's negotiation headers for the next
            // conditional request — but only when every entry in this round
            // was fully processed. On deadline deferral (or a `?` abort that
            // skips this code) the headers stay stale, forcing the next round
            // to re-fetch the full body and pick up the leftovers (sent_entries
            // dedup keeps already-pushed entries from being re-pushed).
            if !deferred {
                db::update_fetch_headers(
                    &state.db,
                    sub.id,
                    fr.http_etag.as_deref(),
                    fr.http_last_modified.as_deref(),
                )
                .await
                .ok();
            }
        }
        Err(e) => {
            let err_msg = format!("{:#}", e);
            warn!(feed = %sub.feed_url, error = %err_msg, "fetch failed");

            // Record that this platform had a failure this round + save the
            // last error message (for whole-round hysteresis evaluation).
            if let Some(p) = platform_of_url(&sub.feed_url) {
                stats.failed.lock().await.insert(p);
                stats.last_err.lock().await.insert(p, err_msg.clone());
            }

            // Cookie-expired flag (for the /cookies panel status display):
            // set immediately (global). Detection rule: if the error message
            // contains "cookie", treat the cookie as expired (under normal
            // circumstances legitimate errors never contain the word cookie —
            // only authentication-class errors returned by platforms do).
            // SUBSTRING CONTRACT: the check below matches the "cookie"
            // substring in platform fetch error messages to detect cookie
            // expiry; platform code must keep including "cookie" in such
            // error messages.
            if err_msg.to_lowercase().contains("cookie") {
                if let Some(p) = platform_of_url(&sub.feed_url) {
                    db::set_setting(&state.db, &db::setting_key::cookie_expired_notified(p), "1")
                        .await
                        .ok();
                }
            }

            let count = db::increment_error_count(&state.db, sub.id, &err_msg).await?;

            if count >= state.config.max_fetch_errors && sub.active == 1 {
                // Auto-disable (marked as system-auto-disabled; the self-heal
                // job auto-recovers it after the cooldown). The UPDATE carries
                // an `active = 1` guard closing the manual-pause race: the
                // round-start snapshot above may be stale if the user paused
                // the source mid-round, in which case the disable is skipped
                // (rows_affected = 0) — a manual pause must not be converted
                // into an auto-disable that the self-heal job would undo.
                if !db::auto_disable_subscription(&state.db, sub.id).await? {
                    info!(
                        feed = %sub.feed_url,
                        "skip auto-disable: subscription paused by user during this round"
                    );
                    return Ok(());
                }
                info!(feed = %sub.feed_url, errors = count, "subscription auto-disabled");
                // Edge deduplication (#9 fix): no duplicate notification when
                // the same source goes disable → self-heal → fail again.
                // Notify only on the first disable; a successful fetch clears
                // the flag so the next failure can notify again.
                let notify_key = db::setting_key::auto_disable_notified(&sub.chat_id, &sub.feed_url);
                let already_notified = db::get_setting(&state.db, &notify_key)
                    .await
                    .ok()
                    .flatten()
                    .is_some();
                if !already_notified {
                    db::set_setting(&state.db, &notify_key, "1").await.ok();
                    // Format: title = source-name hyperlink; body = check hint
                    // + /set command (inline code).
                    let name = sub
                        .feed_title
                        .as_deref()
                        .filter(|t| !t.is_empty())
                        .map(|t| format!(
                            "<a href=\"{}\">{}</a>",
                            crate::push::escape_attr(&sub.feed_url),
                            crate::push::escape_html(t)
                        ))
                        .unwrap_or_else(|| sub.feed_url.clone());
                    notify_owner_html(
                        bot,
                        state,
                        &sub.chat_id,
                        &i18n::t(lang, "notif_auto_disable")
                            .replacen("{}", &name, 1)
                            .replacen("{}", &crate::push::escape_html(&sub.chat_id), 1),
                    )
                    .await;
                }
            }
        }
    }

    Ok(())
}

/// Determine which platform (weibo / x / telegram) a feed_url belongs to;
/// returns None for non-platform sources. Registry-driven (platform_for_url),
/// so adding a new platform requires no changes to this file.
fn platform_of_url(feed_url: &str) -> Option<&'static str> {
    crate::platforms::platform_for_url(feed_url).map(|p| p.id)
}

/// Platform-level health detection (failure direction): called after the
/// hysteresis evaluation in run_fetch_cycle. The atomic flip guarantees a
/// single notification on ok → down only; no repeat while persistently down.
/// v1.11.0: after cookies were globalized, notifications go to all admins.
async fn maybe_notify_platform_down(bot: &Bot, state: &AppState, platform: &str, err_msg: &str) {
    // Atomic edge trigger: returns true only when the state flips from
    // non-down to down. DB errors and "already down" are handled differently:
    // errors log a warn and leave a trace (it will be retried next round if
    // the streak still meets the threshold, so it self-heals), instead of
    // being silently swallowed and letting the alert vanish without a trace.
    let health_key = db::setting_key::platform_health(platform);
    let flipped = match db::flip_setting_once(&state.db, &health_key, "down").await {
        Ok(f) => f,
        Err(e) => {
            warn!(platform = platform, error = %e, "platform down flip failed, alert may be lost this round");
            return;
        }
    };
    if !flipped {
        return; // Already down, do not notify again
    }

    let label = crate::platforms::find_platform(platform)
        .map(|p| p.label)
        .unwrap_or(platform);

    // Unified format: title + detailed error wrapped in a blockquote.
    // Whether it is folded is decided by the unified rule inside
    // notify_user_html (fold when >110 characters).
    notify_admins_html(bot, state, |lang| {
        i18n::t(lang, "notif_platform_down")
            .replacen("{}", label, 1)
            .replacen("{}", &crate::push::escape_html(err_msg), 1)
    }).await;
}

/// Platform-level health detection (recovery direction): called by
/// run_fetch_cycle at the end of the whole round, triggered only when all of
/// this platform's sources succeeded this round (avoiding flapping where a
/// partial success reports recovery). Edge-triggered: notify only on
/// down → ok; also clears the cookie-expired flag.
/// v1.11.0: after cookies were globalized, notifications go to all admins.
async fn maybe_notify_platform_recovered(bot: &Bot, state: &AppState, platform: &str) {
    let health_key = db::setting_key::platform_health(platform);
    // Pre-check: attempt recovery only when clearly in the down state.
    // Avoids mistakenly sending a "recovered" notification on a fresh install
    // (key does not exist).
    let was_down = matches!(
        db::get_setting(&state.db, &health_key).await,
        Ok(Some(v)) if v == "down"
    );
    if !was_down {
        return;
    }

    // Atomic edge trigger: returns true only when the state flips from non-ok
    // to ok. DB errors log a warn and leave a trace (retried next round if
    // everything still succeeds); not silently swallowed.
    let flipped = match db::flip_setting_once(&state.db, &health_key, "ok").await {
        Ok(f) => f,
        Err(e) => {
            warn!(platform = platform, error = %e, "platform recovery flip failed");
            return;
        }
    };
    if !flipped {
        return;
    }

    // Clear the cookie-expired flag (the /cookies panel no longer shows
    // "expired")
    db::set_setting(&state.db, &db::setting_key::cookie_expired_notified(platform), "0")
        .await
        .ok();

    let label = crate::platforms::find_platform(platform)
        .map(|p| p.label)
        .unwrap_or(platform);
    notify_admins_html(bot, state, |lang| {
        i18n::t(lang, "notif_platform_recovered").replacen("{}", label, 1)
    }).await;
}

/// Send an HTML notification to a single user (falls back to plain text
/// when HTML parsing fails, so the notice is never lost).
pub async fn notify_user_html(bot: &Bot, user_id: i64, html_msg: &str) {
    // Unified blockquote folding rule (fold when >110 characters), consistent
    // with message pushing.
    let html_msg = crate::push::normalize_blockquote_fold(html_msg);
    // Notifications uniformly disable link previews: they often contain
    // source/message links, and preview cards are noise.
    let no_preview = crate::push::no_preview();
    let res = bot
        .send_message(ChatId(user_id), html_msg.as_str())
        .parse_mode(teloxide::types::ParseMode::Html)
        .link_preview_options(no_preview.clone())
        .await;
    if let Err(e) = res {
        // 403 = the owner never /start'ed the bot (or blocked it): the plain-
        // text fallback would also inevitably 403, so log a prominent warn and
        // skip the fallback, saving one wasted API call.
        let err_str = e.to_string();
        if err_str.contains("Forbidden") || err_str.contains("403") {
            warn!(
                user = user_id,
                "NOTIFICATION UNDELIVERABLE: user has not started a private chat with the bot (or blocked it). \
                 All alerts for this user are silently lost. Ask the user to send /start to the bot."
            );
            return;
        }
        // Downgrade to plain text: first restore <a href="URL">text</a> to
        // "URL text" (link information in notifications must not be lost),
        // then strip tags uniformly (reusing fetcher::strip_tags).
        let plain = html_msg
            .replace("<a href=\"", "")
            .replace("\">", " ")
            .replace("</a>", "");
        let plain = crate::fetcher::strip_tags(&plain);
        if let Err(e2) = bot
            .send_message(ChatId(user_id), &plain)
            .link_preview_options(no_preview.clone())
            .await
        {
            error!(user = user_id, error = %e2, "failed to notify user (html fallback also failed, first error: {})", e);
        }
    }
}

/// Resolve the owner of a chat and send an HTML notification to them.
/// If the chat has no owner (shouldn't happen post-migration), log and skip.
async fn notify_owner_html(bot: &Bot, state: &AppState, chat_id: &str, html_msg: &str) {
    match db::get_chat_owner(&state.db, chat_id).await {
        Ok(Some(owner)) => notify_user_html(bot, owner, html_msg).await,
        Ok(None) => warn!(chat_id = chat_id, "cannot notify: chat has no owner"),
        Err(e) => warn!(chat_id = chat_id, error = %e, "cannot notify: get_chat_owner failed"),
    }
}

/// Send an HTML notification to all admins (v1.11.0: after cookies were
/// globalized, platform-level alerts go to admins instead of channel owners).
/// Each admin receives the copy rendered in their own language preference;
/// when no admins are configured, log a warn and leave a trace (a clue for
/// troubleshooting silently lost alerts).
async fn notify_admins_html(
    bot: &Bot,
    state: &AppState,
    build_msg: impl Fn(Lang) -> String,
) {
    let admins = &state.config.admin_user_ids;
    if admins.is_empty() {
        warn!("platform alert has no ADMIN_USER_IDS to notify");
        return;
    }
    for &admin in admins {
        let lang = owner_lang(state, admin).await;
        notify_user_html(bot, admin, &build_msg(lang)).await;
    }
}

/// Resolve the owner's language preference from DB.
async fn owner_lang(state: &AppState, owner_id: i64) -> Lang {
    // Delegate to the single source of truth for language resolution
    // (callbacks::resolve_lang) — notifications have no Telegram client
    // language hint, so pass None (falls back to En like before).
    crate::callbacks::resolve_lang(state, owner_id, None).await
}

/// Append a line "\nlabel：value" (skipped when the value is empty). The
/// unified way of assembling notification copy.
fn push_line(msg: &mut String, lang: Lang, label_key: &str, value: &str) {
    if !value.is_empty() {
        // Values are mostly URLs (containing & query parameters); escape them
        // uniformly so they are not parsed as HTML entities.
        msg.push_str(&format!("\n{}：{}", i18n::t(lang, label_key), push::escape_html(value)));
    }
}

/// Oversized-video notification (build + send).
/// When `delivered_tg_link` is Some, the entry was delivered as pure text
/// (with the message link attached); when None, the entry was not pushed in
/// media-only mode.
async fn notify_oversized_video(
    bot: &Bot,
    lang: Lang,
    owner_id: Option<i64>,
    chat_title: &str,
    entry_link: &str,
    delivered_tg_link: Option<&str>,
    ov: &crate::media::OversizedVideo,
) {
    let Some(owner) = owner_id else { return };
    // The video's actual size is folded into the first line (e.g. "video
    // ~155 MB not pushed"); omitted when the size is unknown, leaving no
    // extra spaces. The video direct link is no longer listed separately.
    let size_part = ov
        .size
        .map(|s| i18n::t(lang, "notif_video_size").replacen("{}", &(s / (1024 * 1024)).to_string(), 1))
        .unwrap_or_default();
    let mut msg = match delivered_tg_link {
        Some(_) => i18n::t(lang, "notif_video_oversized")
            .replacen("{}", &size_part, 1)
            .replacen("{}", &push::escape_html(chat_title), 1),
        None => i18n::t(lang, "notif_video_oversized_no_text")
            .replacen("{}", &size_part, 1)
            .replacen("{}", &push::escape_html(chat_title), 1),
    };
    if let Some(tg) = delivered_tg_link {
        push_line(&mut msg, lang, "label_message", tg);
    }
    push_line(&mut msg, lang, "label_entry", entry_link);
    notify_user_html(bot, owner, &msg).await;
}

/// Media download failure notification (build + send).
/// `header_note` is an additional note (e.g. the media-only mode's "entry not
/// pushed, retrying next round"); an empty string means none.
#[allow(clippy::too_many_arguments)]
async fn notify_download_failed(
    bot: &Bot,
    lang: Lang,
    owner_id: Option<i64>,
    feed_title: &str,
    warnings: &[String],
    header_note: &str,
    delivered_tg_link: Option<&str>,
    entry_link: &str,
) {
    let Some(owner) = owner_id else { return };
    let mut msg = i18n::t(lang, "notif_dl_failed")
        .replacen("{}", header_note, 1)
        .replacen("{}", &push::escape_html(feed_title), 1);
    for w in warnings {
        msg.push('\n');
        msg.push_str(&push::escape_html(w));
    }
    if let Some(tg) = delivered_tg_link {
        push_line(&mut msg, lang, "label_message", tg);
    }
    push_line(&mut msg, lang, "label_source", entry_link);
    notify_user_html(bot, owner, &msg).await;
}


// ===== Outbox retry (persistent retry queue for failed sends) =====

/// Register the outbox retry job: runs every 5 minutes, retries failed sends
/// with exponential backoff (capped at 2h intervals), abandons after 72h.
pub async fn register_outbox_retry_job(
    sched: &JobScheduler,
    state: AppState,
    bot: Bot,
) -> anyhow::Result<()> {
    sched
        .add(Job::new_async("0 0,5,10,15,20,25,30,35,40,45,50,55 * * * *", move |_uuid, _l| {
            let state = state.clone();
            let bot = bot.clone();
            Box::pin(async move {
                if let Err(e) = run_outbox_retry(&state, &bot).await {
                    error!(error = %e, "outbox retry cycle failed");
                }
            })
        })?)
        .await?;
    Ok(())
}

/// Exponential backoff intervals (seconds) for outbox retries.
/// Index = attempts (0-based after first failure). Capped at 2h.
fn outbox_backoff_secs(attempts: i64) -> i64 {
    match attempts {
        0 => 60,       // 1 min
        1 => 300,      // 5 min
        2 => 900,      // 15 min
        3 => 1800,     // 30 min
        4 => 3600,     // 1 hour
        _ => 7200,     // 2 hours (cap)
    }
}

/// Total elapsed time before abandoning an outbox entry (72 hours).
const OUTBOX_ABANDON_SECS: i64 = 72 * 3600;

/// Unified construction of push-failure notifications (M5 fix: the original
/// 4 format! templates were verbatim duplicates).
/// header = the text inside the parentheses (e.g. "queued for retry" /
/// "abandoned" / "enqueue failed, entry lost"), reason = the details
/// inside the blockquote, link_label/link_url = optional source/entry link.
fn push_fail_msg(lang: Lang, header: &str, feed_title: &str, reason: &str, link_label_key: &str, link_url: &str) -> String {
    let mut msg = i18n::t(lang, "notif_push_failed")
        .replacen("{}", header, 1)
        .replacen("{}", &push::escape_html(feed_title), 1)
        .replacen("{}", reason, 1);
    if !link_url.is_empty() {
        push_line(&mut msg, lang, link_label_key, link_url);
    }
    msg
}

/// Abandon one outbox entry: notify the owner + mark sent + delete the queue
/// row. Shared by the 72h-timeout abandonment and the MediaOnly media-expiry
/// abandonment (reason_key is the i18n key for the blockquote copy).
/// Marking sent prevents the entry from being re-pushed by the next fetch
/// while it is still in the source's sliding window, which would form an
/// infinite loop of "re-push → fail → re-enqueue → abandon again" and
/// duplicate notifications.
async fn abandon_outbox_row(bot: &Bot, state: &AppState, row: &db::OutboxRow, reason_key: &str) {
    let lang = match db::get_chat_owner(&state.db, &row.chat_id).await {
        Ok(Some(owner)) => owner_lang(state, owner).await,
        _ => Lang::En,
    };
    let entry_link = extract_entry_link_from_json(&row.entry_json);
    let msg = push_fail_msg(
        lang,
        i18n::t(lang, "notif_abandoned"),
        &row.feed_title,
        i18n::t(lang, reason_key),
        "label_source",
        entry_link.as_deref().unwrap_or(""),
    );
    notify_owner_html(bot, state, &row.chat_id, &msg).await;
    if let Err(e) = db::mark_entry_sent(&state.db, &row.chat_id, &row.feed_url, &row.entry_id).await {
        warn!(entry_id = %row.entry_id, error = %e, "outbox: failed to mark abandoned entry as sent");
    }
    if let Err(e) = db::outbox_delete(&state.db, row.id).await {
        warn!(entry_id = %row.entry_id, error = %e, "outbox: failed to delete abandoned entry");
    }
}

async fn run_outbox_retry(state: &AppState, bot: &Bot) -> anyhow::Result<()> {
    // Reentrancy protection: one retry round can exceed the 5-minute cron
    // interval due to large media uploads; without a lock, the next tick
    // would concurrently pick up the same batch of rows and double-send.
    if state
        .outbox_retry_active
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::Acquire,
            std::sync::atomic::Ordering::Acquire,
        )
        .is_err()
    {
        return Ok(());
    }
    let _guard = ActiveGuard(state.outbox_retry_active.clone());

    let now = chrono::Utc::now().timestamp();
    let due = db::outbox_fetch_due(&state.db, now, 10).await?;
    if due.is_empty() {
        return Ok(());
    }

    let transport = crate::media::MediaTransport::new(&state.config);

    for row in due {
        // Dedup guard first: if the entry was already delivered via the normal
        // fetch path (e.g. network recovered and the next cycle re-pushed it),
        // drop the queued copy instead of double-posting. Checking this before
        // the 72h-abandon branch avoids a false "abandoned" notice for an entry
        // that was actually delivered while sitting in the queue.
        // Fail-closed: when the DB query errors, skip this round (keep the
        // queue and re-check next round); do not treat a transient error as
        // "not sent" and keep sending (prevents double-sends).
        match db::is_entry_sent(&state.db, &row.chat_id, &row.feed_url, &row.entry_id).await {
            Ok(true) => {
                info!(entry_id = %row.entry_id, feed = %row.feed_url, "outbox entry already sent, dropping");
                if let Err(e) = db::outbox_delete(&state.db, row.id).await {
                    warn!(entry_id = %row.entry_id, error = %e, "outbox: failed to delete entry");
                }
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                warn!(entry_id = %row.entry_id, error = %e, "outbox: is_entry_sent check failed, skipping this round (fail-closed)");
                continue;
            }
        }

        // Check if 72h elapsed -> abandon
        if now - row.created_at >= OUTBOX_ABANDON_SECS {
            warn!(
                entry_id = %row.entry_id,
                feed = %row.feed_url,
                "outbox entry abandoned after 72h"
            );
            abandon_outbox_row(bot, state, &row, "notif_timeout_72h").await;
            continue;
        }

        // Deserialize entry
        let entry: feed_rs::model::Entry = match serde_json::from_str(&row.entry_json) {
            Ok(e) => e,
            Err(e) => {
                error!(entry_id = %row.entry_id, error = %e, "outbox: failed to deserialize entry");
                if let Err(e) = db::outbox_delete(&state.db, row.id).await {
                    warn!(entry_id = %row.entry_id, error = %e, "outbox: failed to delete entry");
                }
                continue;
            }
        };

        // Deserialize media items (filter out files that no longer exist)
        let all_media: Vec<crate::media::MediaItem> =
            serde_json::from_str(&row.media_json).unwrap_or_default();
        let had_media = !all_media.is_empty();
        let original_count = all_media.len();
        let media_items: Vec<crate::media::MediaItem> = all_media
            .into_iter()
            .filter(|m| m.path.exists())
            .collect();
        // Some media files expired (not the MediaOnly all-gone scenario):
        // degrade to sending the remaining media, log a warn and leave a trace
        // (owner visibility is covered by the media_failed notification).
        if had_media && media_items.len() < original_count && !media_items.is_empty() {
            warn!(
                entry_id = %row.entry_id,
                feed = %row.feed_url,
                original = original_count,
                remaining = media_items.len(),
                "outbox retry: some media files expired, sending with reduced media"
            );
        }

        // MediaOnly mode with all media files gone (cleaned up): cannot send
        // anything meaningful. Notify the owner and abandon instead of silent mark-sent.
        if row.content_mode == "media_only" && had_media && media_items.is_empty() {
            warn!(
                entry_id = %row.entry_id,
                feed = %row.feed_url,
                "outbox entry abandoned: MediaOnly but all media files expired"
            );
            abandon_outbox_row(bot, state, &row, "notif_media_expired").await;
            continue;
        }

        // Reconstruct render opts
        let opts = push::RenderOpts {
            mode: ContentMode::from_str(&row.content_mode),
            source: push::SourceDisplay::from_str(&row.source_display),
            link_preview: row.link_preview,
        };
        let chat_id: i64 = match row.chat_id.parse() {
            Ok(id) => id,
            Err(_) => {
                // chat_id is not numeric: this entry can never be delivered,
                // delete it to avoid infinite retries.
                warn!(chat_id = %row.chat_id, entry_id = %row.entry_id, "outbox: invalid chat_id, dropping entry");
                if let Err(e) = db::outbox_delete(&state.db, row.id).await {
                    warn!(entry_id = %row.entry_id, error = %e, "outbox: failed to delete entry");
                }
                continue;
            }
        };
        let author = row.author.as_deref();

        // Second dedup check: the fetch cycle may have pushed the same entry
        // between fetch_due and this row (check-then-act window); querying
        // once more right before sending shrinks the window.
        // Likewise fail-closed: on a DB error, skip this round rather than
        // risking a send.
        match db::is_entry_sent(&state.db, &row.chat_id, &row.feed_url, &row.entry_id).await {
            Ok(true) => {
                info!(entry_id = %row.entry_id, feed = %row.feed_url, "outbox entry sent between fetch and send, dropping");
                if let Err(e) = db::outbox_delete(&state.db, row.id).await {
                    warn!(entry_id = %row.entry_id, error = %e, "outbox: failed to delete entry");
                }
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                warn!(entry_id = %row.entry_id, error = %e, "outbox: second is_entry_sent check failed, skipping this round (fail-closed)");
                continue;
            }
        }

        // Group-C sidecar: the meta for outbox retries comes from the
        // meta_json column. An empty string or a deserialization failure is
        // treated as no meta (compatibility with historical rows; behavior
        // degrades to pure text).
        let outbox_meta: Option<crate::entrymeta::EntryMeta> = if row.meta_json.is_empty() {
            None
        } else {
            serde_json::from_str(&row.meta_json).ok()
        };
        let meta_ref = outbox_meta.as_ref();

        // Attempt send
        let result = if media_items.is_empty() {
            // The pure-text path uniformly goes through send_text_only (H1 fix:
            // converged with the main push path into a single function; the
            // poll-resend logic had previously been fixed twice due to dual-path
            // drift, v1.9.3/v1.9.6).
            push::send_text_only(bot, chat_id, &entry, &row.feed_title, author, opts, meta_ref).await
        } else {
            push::send_entry_with_media(
                bot,
                &state.db,
                chat_id,
                push::PushCtx {
                    entry: &entry,
                    feed_title: &row.feed_title,
                    author,
                    opts,
                    caption_above_media: row.caption_above_media,
                    aspect_threshold: state.config.long_image_aspect_ratio,
                    media: &media_items,
                    meta: meta_ref,
                },
                &transport,
            )
            .await
        };

        match result {
            Ok(send_outcome) => {
                info!(entry_id = %row.entry_id, feed = %row.feed_url, "outbox retry succeeded");
                if let Some(mid) = send_outcome.delivered_message_id {
                    db::record_sent_message(
                        &state.db,
                        &row.chat_id,
                        mid,
                        &row.feed_url,
                        &row.entry_id,
                        send_outcome.entry_link.as_deref(),
                        state.config.sent_messages_max_rows,
                    )
                    .await
                    .ok();
                }
                // Mark sent so a later re-fetch/re-subscribe won't double-post.
                if let Err(e) = db::mark_entry_sent(&state.db, &row.chat_id, &row.feed_url, &row.entry_id).await {
                    warn!(entry_id = %row.entry_id, error = %e, "outbox: failed to mark entry sent (may re-push on re-subscribe)");
                }
                if let Err(e) = db::outbox_delete(&state.db, row.id).await {
                    warn!(entry_id = %row.entry_id, error = %e, "outbox: failed to delete entry");
                }
            }
            Err(e) => {
                let attempts = row.attempts + 1;
                let backoff = outbox_backoff_secs(attempts);
                let next_retry = now + backoff;
                warn!(
                    entry_id = %row.entry_id,
                    feed = %row.feed_url,
                    attempts = attempts,
                    next_in_secs = backoff,
                    error = %e,
                    "outbox retry failed"
                );
                if let Err(e) = db::outbox_update_retry(
                    &state.db,
                    row.id,
                    attempts,
                    next_retry,
                    &format!("{:#}", e),
                )
                .await
                {
                    warn!(entry_id = %row.entry_id, error = %e, "outbox: failed to update retry state");
                }
            }
        }
    }
    Ok(())
}

/// Extract entry link from serialized entry JSON (best-effort).
fn extract_entry_link_from_json(entry_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(entry_json).ok()?;
    v.get("links")
        .and_then(|l| l.as_array())
        .and_then(|arr| arr.first())
        .and_then(|l| l.get("href"))
        .and_then(|h| h.as_str())
        .map(|s| s.to_string())
}
