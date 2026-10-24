//! Settings pages: channel-level config, feed-level overrides, option pages.
//! Split out of callbacks/mod.rs (batch-B3 refactor); behavior unchanged.
//! Uses the parent module's private helpers via `super::`.

use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

use super::{back_close_row, deliver, deliver_error_page, load_sub_or_deliver, PageTarget};
use crate::db;
use crate::i18n::{self, Lang};
use crate::push::{ContentMode, SourceDisplay};
use crate::state::AppState;

/// Shows current values with a ✓ and lets the owner switch them in place.
/// Settings main page: three second-level entries + link-preview toggle.
#[allow(clippy::too_many_arguments)]
pub(super) async fn render_settings_page(
    bot: &Bot,
    target: PageTarget<'_>,
    chat_id: &str,
    state: &AppState,
    ch_page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    let cfg = load_chat_config(&state.db, chat_id).await;
    let chat = db::get_chat(&state.db, chat_id).await.ok().flatten();
    let title = super::chat_display_title(chat.as_ref().and_then(|c| c.title.as_deref()), chat_id);

    let iv_label = fmt_interval(lang, cfg.interval_minutes);
    let cm_label = fmt_content_mode(lang, &cfg.content_mode);
    let sd_label = fmt_source_display(lang, &cfg.source_display);
    let lp_label = if cfg.link_preview != 0 { i18n::t(lang, "on") } else { i18n::t(lang, "off") };

    let text = i18n::t(lang, "settings_header")
        .replacen("{}", &crate::push::format_chat_title_html(&title, chat_id), 1)
        .replacen("{}", &iv_label, 1)
        .replacen("{}", cm_label, 1)
        .replacen("{}", sd_label, 1)
        .replacen("{}", lp_label, 1);

    let pages = format!("|{}|{}", ch_page, chlist_page);
    let buttons = vec![
        vec![InlineKeyboardButton::callback(i18n::t(lang, "settings_interval"), format!("c|cfgi|{}{}", chat_id, pages))],
        vec![InlineKeyboardButton::callback(i18n::t(lang, "settings_mode"), format!("c|cfgm|{}{}", chat_id, pages))],
        vec![InlineKeyboardButton::callback(i18n::t(lang, "settings_source"), format!("c|cfgs|{}{}", chat_id, pages))],
        vec![InlineKeyboardButton::callback(
            i18n::t(lang, "settings_caption").replacen(
                "{}",
                if cfg.caption_above_media != 0 { i18n::t(lang, "on") } else { i18n::t(lang, "off") },
                1,
            ),
            format!("c|cfgcp|{}{}", chat_id, pages),
        )],
        vec![InlineKeyboardButton::callback(
            i18n::t(lang, "settings_lp").replacen("{}", lp_label, 1),
            format!("c|cfglp|{}{}", chat_id, pages),
        )],
        back_close_row(lang, "back", format!("c|ch|{}|{}|{}", chat_id, ch_page, chlist_page)),
    ];

    let kb = InlineKeyboardMarkup::new(buttons);
    deliver(bot, &target, &text, Some(kb)).await?;
    Ok(())
}

/// Data-driven option page spec (M2 fix: 6 near-identical render functions
/// collapsed into one renderer + declarative specs).
struct OptionPageSpec {
    title: String,
    /// Option rows: each row is a list of (display_label, callback_value) pairs.
    rows: Vec<Vec<(&'static str, String)>>,
    /// Currently selected value (gets ✓ mark).
    current: String,
    /// For feed-level: whether the value is overridden (✏️) vs inherited (🔗).
    overridden: bool,
    /// When true (feed-level), ✓ is shown only if overridden AND value matches.
    /// When false (channel-level), ✓ is shown unconditionally on match.
    require_override_for_mark: bool,
    /// Callback prefix (e.g. "c|cfgiv" or "c|srcfi").
    cb_prefix: &'static str,
    /// ID segment (chat_id or sub_id as string).
    id_seg: String,
    /// Suffix appended after value in callback (e.g. "|2" for ch_page, or "").
    cb_suffix: String,
    /// Back button callback data.
    back_cb: String,
    /// Show the "follow channel" inherit button (feed-level only).
    show_inherit: bool,
}

/// Render an option selection page from a declarative spec.
async fn render_option_page(bot: &Bot, target: &PageTarget<'_>, spec: &OptionPageSpec, lang: Lang) -> ResponseResult<()> {
    let mark = |opt: &str| -> &'static str {
        if spec.current == opt && (!spec.require_override_for_mark || spec.overridden) {
            "✓ "
        } else {
            ""
        }
    };
    let mut buttons: Vec<Vec<InlineKeyboardButton>> = spec
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|(label, value)| {
                    InlineKeyboardButton::callback(
                        format!("{}{}", mark(value), label),
                        format!("{}|{}|{}{}", spec.cb_prefix, spec.id_seg, value, spec.cb_suffix),
                    )
                })
                .collect()
        })
        .collect();
    if spec.show_inherit {
        buttons.push(vec![InlineKeyboardButton::callback(
            i18n::t(lang, "follow_channel"),
            format!("{}|{}|inherit{}", spec.cb_prefix, spec.id_seg, spec.cb_suffix),
        )]);
    }
    buttons.push(back_close_row(lang, "back", spec.back_cb.clone()));
    let kb = InlineKeyboardMarkup::new(buttons);
    deliver(bot, target, &spec.title, Some(kb)).await?;
    Ok(())
}

/// Second-level page: push interval options.
pub(super) async fn render_settings_interval(
    bot: &Bot,
    target: PageTarget<'_>,
    chat_id: &str,
    state: &AppState,
    ch_page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    let cfg = load_chat_config(&state.db, chat_id).await;
    let spec = OptionPageSpec {
        title: i18n::t(lang, "interval_title").replacen("{}", &fmt_interval(lang, cfg.interval_minutes), 1),
        rows: vec![
            vec![(i18n::t(lang, "5min"), "5".into()), (i18n::t(lang, "10min"), "10".into()), (i18n::t(lang, "30min"), "30".into())],
            vec![(i18n::t(lang, "3h"), "180".into()), (i18n::t(lang, "6h"), "360".into()), (i18n::t(lang, "12h"), "720".into())],
        ],
        current: cfg.interval_minutes.to_string(),
        overridden: false,
        require_override_for_mark: false,
        cb_prefix: "c|cfgiv",
        id_seg: chat_id.to_string(),
        cb_suffix: format!("|{}|{}", ch_page, chlist_page),
        back_cb: format!("c|cfg|{}|{}|{}", chat_id, ch_page, chlist_page),
        show_inherit: false,
    };
    render_option_page(bot, &target, &spec, lang).await
}

/// Second-level page: content mode options.
pub(super) async fn render_settings_mode(
    bot: &Bot,
    target: PageTarget<'_>,
    chat_id: &str,
    state: &AppState,
    ch_page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    let cfg = load_chat_config(&state.db, chat_id).await;
    let spec = OptionPageSpec {
        title: i18n::t(lang, "mode_title").replacen("{}", fmt_content_mode(lang, &cfg.content_mode), 1),
        rows: vec![
            vec![(i18n::t(lang, "mode_title_link"), "title_link".into())],
            vec![(i18n::t(lang, "mode_title_summary"), "title_summary".into())],
            vec![(i18n::t(lang, "mode_title_content"), "title_content".into())],
            vec![(i18n::t(lang, "mode_title_only"), "title_only".into())],
            vec![(i18n::t(lang, "mode_content_only"), "content_only".into())],
            vec![(i18n::t(lang, "mode_media_only"), "media_only".into())],
        ],
        current: content_mode_value(&cfg.content_mode).to_string(),
        overridden: false,
        require_override_for_mark: false,
        cb_prefix: "c|cfgmv",
        id_seg: chat_id.to_string(),
        cb_suffix: format!("|{}|{}", ch_page, chlist_page),
        back_cb: format!("c|cfg|{}|{}|{}", chat_id, ch_page, chlist_page),
        show_inherit: false,
    };
    render_option_page(bot, &target, &spec, lang).await
}

/// Second-level page: source display options.
pub(super) async fn render_settings_source(
    bot: &Bot,
    target: PageTarget<'_>,
    chat_id: &str,
    state: &AppState,
    ch_page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    let cfg = load_chat_config(&state.db, chat_id).await;
    let spec = OptionPageSpec {
        title: i18n::t(lang, "source_title").replacen("{}", fmt_source_display(lang, &cfg.source_display), 1),
        rows: vec![
            vec![(i18n::t(lang, "src_title"), "source".into())],
            vec![(i18n::t(lang, "src_author"), "author".into())],
            vec![(i18n::t(lang, "src_none"), "none".into())],
        ],
        current: source_display_value(&cfg.source_display).to_string(),
        overridden: false,
        require_override_for_mark: false,
        cb_prefix: "c|cfgsv",
        id_seg: chat_id.to_string(),
        cb_suffix: format!("|{}|{}", ch_page, chlist_page),
        back_cb: format!("c|cfg|{}|{}|{}", chat_id, ch_page, chlist_page),
        show_inherit: false,
    };
    render_option_page(bot, &target, &spec, lang).await
}

/// Load chat config, creating the row on first access.
/// Tolerant variant for DISPLAY paths (render_*): a transient DB read error
/// falls back to the default row so the panel still renders. Write/toggle
/// paths must use `load_chat_config_checked` instead — toggling from a wrong
/// baseline persists an override contradicting the user's real setting.
pub(super) async fn load_chat_config(db: &crate::db::Db, chat_id: &str) -> db::ChatConfigRow {
    match load_chat_config_checked(db, chat_id).await {
        Ok(c) => c,
        Err(_) => default_chat_config(chat_id.to_string()),
    }
}

/// Strict variant for WRITE/toggle paths: propagates a DB read error so the
/// caller aborts (the callback wrapper answers and clears the spinner) rather
/// than writing an override computed from a wrong default baseline. A genuinely
/// missing row (Ok(None)) is still a valid "use defaults" case, not an error.
pub(super) async fn load_chat_config_checked(
    db: &crate::db::Db,
    chat_id: &str,
) -> anyhow::Result<db::ChatConfigRow> {
    match db::get_chat_config(db, chat_id).await? {
        Some(c) => Ok(c),
        None => {
            db::ensure_chat_config(db, chat_id).await.ok();
            Ok(default_chat_config(chat_id.to_string()))
        }
    }
}

fn default_chat_config(chat_id: String) -> db::ChatConfigRow {
    db::ChatConfigRow {
        chat_id,
        interval_minutes: db::DEFAULT_INTERVAL_MINUTES,
        content_mode: db::DEFAULT_CONTENT_MODE.to_string(),
        source_display: db::DEFAULT_SOURCE_DISPLAY.to_string(),
        caption_above_media: db::DEFAULT_CAPTION_ABOVE_MEDIA,
        link_preview: db::DEFAULT_LINK_PREVIEW,
    }
}

pub(super) fn fmt_interval(lang: Lang, min: i64) -> String {
    match min {
        180 => i18n::t(lang, "3h").to_string(),
        360 => i18n::t(lang, "6h").to_string(),
        720 => i18n::t(lang, "12h").to_string(),
        m => i18n::t(lang, "n_min").replacen("{}", &m.to_string(), 1),
    }
}

pub(super) fn fmt_content_mode(lang: Lang, m: &str) -> &'static str {
    // Normalize through the same legacy-tolerant parser used by rendering, so
    // an old `TitleSummary`/`MediaOnly` value does not display as Title+Link.
    match ContentMode::from_str(m) {
        ContentMode::TitleSummary => i18n::t(lang, "mode_title_summary"),
        ContentMode::TitleContent => i18n::t(lang, "mode_title_content"),
        ContentMode::TitleOnly => i18n::t(lang, "mode_title_only"),
        ContentMode::ContentOnly => i18n::t(lang, "mode_content_only"),
        ContentMode::MediaOnly => i18n::t(lang, "mode_media_only"),
        ContentMode::TitleLink => i18n::t(lang, "mode_title_link"),
    }
}

/// Canonical stored value for the option page's ✓ comparison.
fn content_mode_value(m: &str) -> &'static str {
    match ContentMode::from_str(m) {
        ContentMode::TitleSummary => "title_summary",
        ContentMode::TitleContent => "title_content",
        ContentMode::TitleOnly => "title_only",
        ContentMode::ContentOnly => "content_only",
        ContentMode::MediaOnly => "media_only",
        ContentMode::TitleLink => "title_link",
    }
}

pub(super) fn fmt_source_display(lang: Lang, s: &str) -> &'static str {
    match SourceDisplay::from_str(s) {
        SourceDisplay::Author => i18n::t(lang, "src_author"),
        SourceDisplay::None => i18n::t(lang, "src_none"),
        SourceDisplay::Source => i18n::t(lang, "src_title"),
    }
}

fn source_display_value(s: &str) -> &'static str {
    match SourceDisplay::from_str(s) {
        SourceDisplay::Author => "author",
        SourceDisplay::None => "none",
        SourceDisplay::Source => "source",
    }
}

/// Second-level page: feed-level push interval override.
/// `chlist_page` is the channel-list page carried through the round trip so
/// "back to feed page" → "back to channel page" → "back to channel list"
/// keeps the user's original positions.
pub(super) async fn render_feed_interval(
    bot: &Bot,
    target: PageTarget<'_>,
    sub_id: i64,
    state: &AppState,
    ch_page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    let Some((_sub, cfg)) = load_feed_ctx(bot, &target, sub_id, state, lang).await? else {
        return Ok(());
    };
    let spec = OptionPageSpec {
        title: i18n::t(lang, "interval_title_feed")
            .replacen("{}", &fmt_interval(lang, cfg.interval_minutes), 1)
            .replacen("{}", if cfg.iv_overridden { "· ✏️" } else { "· 🔗" }, 1),
        rows: vec![
            vec![(i18n::t(lang, "5min"), "5".into()), (i18n::t(lang, "10min"), "10".into()), (i18n::t(lang, "30min"), "30".into())],
            vec![(i18n::t(lang, "3h"), "180".into()), (i18n::t(lang, "6h"), "360".into()), (i18n::t(lang, "12h"), "720".into())],
        ],
        current: cfg.interval_minutes.to_string(),
        overridden: cfg.iv_overridden,
        require_override_for_mark: true,
        cb_prefix: "c|srcfi",
        id_seg: sub_id.to_string(),
        cb_suffix: format!("|{}|{}", ch_page, chlist_page),
        back_cb: format!("c|src|{}|{}|{}", sub_id, ch_page, chlist_page),
        show_inherit: true,
    };
    render_option_page(bot, &target, &spec, lang).await
}

/// Second-level page: feed-level content mode override.
pub(super) async fn render_feed_mode(
    bot: &Bot,
    target: PageTarget<'_>,
    sub_id: i64,
    state: &AppState,
    ch_page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    let Some((_sub, cfg)) = load_feed_ctx(bot, &target, sub_id, state, lang).await? else {
        return Ok(());
    };
    let spec = OptionPageSpec {
        title: i18n::t(lang, "mode_title_feed")
            .replacen("{}", fmt_content_mode(lang, &cfg.content_mode), 1)
            .replacen("{}", if cfg.cm_overridden { "· ✏️" } else { "· 🔗" }, 1),
        rows: vec![
            vec![(i18n::t(lang, "mode_title_link"), "title_link".into())],
            vec![(i18n::t(lang, "mode_title_summary"), "title_summary".into())],
            vec![(i18n::t(lang, "mode_title_content"), "title_content".into())],
            vec![(i18n::t(lang, "mode_title_only"), "title_only".into())],
            vec![(i18n::t(lang, "mode_content_only"), "content_only".into())],
            vec![(i18n::t(lang, "mode_media_only"), "media_only".into())],
        ],
        current: content_mode_value(&cfg.content_mode).to_string(),
        overridden: cfg.cm_overridden,
        require_override_for_mark: true,
        cb_prefix: "c|srcfm",
        id_seg: sub_id.to_string(),
        cb_suffix: format!("|{}|{}", ch_page, chlist_page),
        back_cb: format!("c|src|{}|{}|{}", sub_id, ch_page, chlist_page),
        show_inherit: true,
    };
    render_option_page(bot, &target, &spec, lang).await
}

/// Second-level page: feed-level source display override.
pub(super) async fn render_feed_source(
    bot: &Bot,
    target: PageTarget<'_>,
    sub_id: i64,
    state: &AppState,
    ch_page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    let Some((_sub, cfg)) = load_feed_ctx(bot, &target, sub_id, state, lang).await? else {
        return Ok(());
    };
    let spec = OptionPageSpec {
        title: i18n::t(lang, "source_title_feed")
            .replacen("{}", fmt_source_display(lang, &cfg.source_display), 1)
            .replacen("{}", if cfg.sd_overridden { "· ✏️" } else { "· 🔗" }, 1),
        rows: vec![
            vec![(i18n::t(lang, "src_title"), "source".into())],
            vec![(i18n::t(lang, "src_author"), "author".into())],
            vec![(i18n::t(lang, "src_none"), "none".into())],
        ],
        current: source_display_value(&cfg.source_display).to_string(),
        overridden: cfg.sd_overridden,
        require_override_for_mark: true,
        cb_prefix: "c|srcfs",
        id_seg: sub_id.to_string(),
        cb_suffix: format!("|{}|{}", ch_page, chlist_page),
        back_cb: format!("c|src|{}|{}|{}", sub_id, ch_page, chlist_page),
        show_inherit: true,
    };
    render_option_page(bot, &target, &spec, lang).await
}

/// Second-level page: content filter settings for a feed.
///
/// Public entry point kept for the command path (filter-keyword reply refresh).
/// PendingOp stores both page positions now, so the refresh preserves the
/// channel-list page instead of resetting it.
pub async fn render_feed_filter(
    bot: &Bot,
    target: PageTarget<'_>,
    sub_id: i64,
    state: &AppState,
    ch_page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    render_feed_filter_impl(bot, target, sub_id, state, ch_page, chlist_page, lang).await
}

/// Internal filter renderer carrying `chlist_page` through every button.
pub(super) async fn render_feed_filter_impl(
    bot: &Bot,
    target: PageTarget<'_>,
    sub_id: i64,
    state: &AppState,
    ch_page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    let Some(sub) = load_sub_or_deliver(bot, &target, &state.db, sub_id, lang).await? else {
        return Ok(());
    };
    let fc = db::get_feed_filter(&state.db, &sub.chat_id, &sub.feed_url).await.unwrap_or_default();

    let status = if fc.is_active() {
        i18n::t(lang, "filter_on")
    } else if fc.enabled {
        // The switch is on but no keywords are configured: say so instead of
        // claiming "off" while the toggle button says "disable".
        i18n::t(lang, "filter_on_no_keywords")
    } else {
        i18n::t(lang, "filter_off")
    };
    let mode_label = if fc.mode == "whitelist" { i18n::t(lang, "filter_wl_desc") } else { i18n::t(lang, "filter_bl_desc") };
    let kw_display = if fc.keywords.is_empty() {
        i18n::t(lang, "none").to_string()
    } else {
        format!("<code>{}</code>", crate::push::escape_html(&fc.keywords))
    };

    let text = i18n::t(lang, "filter_panel")
        .replacen("{}", status, 1)
        .replacen("{}", mode_label, 1)
        .replacen("{}", &kw_display, 1);

    let toggle_label = if fc.enabled { i18n::t(lang, "filter_disable") } else { i18n::t(lang, "filter_enable") };
    let mode_btn_label = if fc.mode == "whitelist" { i18n::t(lang, "filter_to_bl") } else { i18n::t(lang, "filter_to_wl") };

    let buttons = vec![
        vec![InlineKeyboardButton::callback(toggle_label, format!("c|srcftoggle|{}|{}|{}", sub_id, ch_page, chlist_page))],
        vec![InlineKeyboardButton::callback(mode_btn_label, format!("c|srcfmode|{}|{}|{}", sub_id, ch_page, chlist_page))],
        vec![InlineKeyboardButton::callback(i18n::t(lang, "filter_set_kw"), format!("c|srcfkw|{}|{}|{}", sub_id, ch_page, chlist_page))],
        back_close_row(lang, "back", format!("c|src|{}|{}|{}", sub_id, ch_page, chlist_page)),
    ];
    let kb = InlineKeyboardMarkup::new(buttons);
    deliver(bot, &target, &text, Some(kb)).await?;
    Ok(())
}

async fn load_feed_ctx(
    bot: &Bot,
    target: &PageTarget<'_>,
    sub_id: i64,
    state: &AppState,
    lang: Lang,
) -> ResponseResult<Option<(db::SubRow, db::EffectiveConfig)>> {
    let Some(sub) = load_sub_or_deliver(bot, target, &state.db, sub_id, lang).await? else {
        return Ok(None);
    };
    match db::get_effective_config(&state.db, &sub.chat_id, &sub.feed_url).await {
        Ok(cfg) => Ok(Some((sub, cfg))),
        Err(e) => {
            deliver_error_page(bot, target, lang, &format!("{}", e)).await?;
            Ok(None)
        }
    }
}
