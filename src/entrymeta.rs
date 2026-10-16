//! Structured per-entry metadata (sidecar).
//!
//! Replaces the legacy in-band markers that were smuggled through the
//! feed-rs `Entry` body / content_type:
//! - `<div data-vote="…">` → [`EntryMeta::poll`]
//! - `<img data-rt="1">`   → [`EntryMeta::retweet_images`]
//! - `text/x-tg-html` MIME  → [`EntryMeta::prerendered_html`]
//!
//! The sidecar travels alongside entries via [`crate::fetcher::FetchResult::metas`]
//! and is persisted in the outbox `meta_json` column for retry fidelity.

use std::collections::HashMap;

/// Structured metadata for a single entry, populated by platform fetchers.
/// RSS sources leave this at defaults (all fields empty/None/false).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct EntryMeta {
    /// Body is pre-rendered Telegram HTML (weibo markdown posts).
    /// Replaces the `text/x-tg-html` content_type sentinel.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prerendered_html: bool,

    /// Weibo poll data extracted from vote_object.
    /// Replaces the `<div data-vote="JSON">` body injection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll: Option<PollData>,

    /// Retweet inline image URLs (decoded from weibo's "view image" inline
    /// marker) that should be sent as a separate message after the main media.
    /// Replaces the `<img data-rt="1">` body injection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retweet_images: Vec<String>,
}

/// Weibo poll payload (question + options + multi-select flag).
/// Truncation to Telegram limits happens at send time (push.rs), not here.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PollData {
    pub question: String,
    pub options: Vec<String>,
    pub is_multi_select: bool,
}

impl EntryMeta {
    /// True when all fields are at their defaults (no metadata to carry).
    pub fn is_empty(&self) -> bool {
        !self.prerendered_html && self.poll.is_none() && self.retweet_images.is_empty()
    }
}

/// Per-fetch metadata map: entry_id → EntryMeta.
/// Only entries with non-default metadata are stored.
pub type MetaMap = HashMap<String, EntryMeta>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_full_meta() {
        // Full meta (all three fields populated) survives a
        // serialize → deserialize round trip faithfully.
        let meta = EntryMeta {
            prerendered_html: true,
            poll: Some(PollData {
                question: "Have you ever moved house?".to_string(),
                options: vec!["A: Never".to_string(), "B: Yes".to_string()],
                is_multi_select: false,
            }),
            retweet_images: vec!["https://wx2.sinaimg.cn/large/abc.jpg".to_string()],
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: EntryMeta = serde_json::from_str(&json).unwrap();
        assert!(back.prerendered_html);
        assert_eq!(back.poll.as_ref().unwrap().question, "Have you ever moved house?");
        assert_eq!(back.poll.as_ref().unwrap().options.len(), 2);
        assert_eq!(back.retweet_images.len(), 1);
    }

    #[test]
    fn serde_roundtrip_default_meta() {
        // Default meta (all empty) serializes to {} and deserializes back to
        // defaults. skip_serializing_if keeps empty fields out of the JSON;
        // #[serde(default)] restores them.
        let meta = EntryMeta::default();
        let json = serde_json::to_string(&meta).unwrap();
        assert_eq!(json, "{}");
        let back: EntryMeta = serde_json::from_str(&json).unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn serde_empty_string_deserializes_to_none() {
        // When an outbox history row has an empty meta_json, the caller
        // should treat it as no meta (without calling from_str).
        // Here we verify that an empty JSON object deserializes to default meta.
        let back: EntryMeta = serde_json::from_str("{}").unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn is_empty_logic() {
        assert!(EntryMeta::default().is_empty());
        assert!(!EntryMeta { prerendered_html: true, ..Default::default() }.is_empty());
        assert!(!EntryMeta { retweet_images: vec!["x".to_string()], ..Default::default() }.is_empty());
        assert!(!EntryMeta {
            poll: Some(PollData { question: "q".into(), options: vec!["a".into(), "b".into()], is_multi_select: false }),
            ..Default::default()
        }.is_empty());
    }
}
