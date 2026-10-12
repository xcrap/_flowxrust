//! Correlating FlowX's local prompts with the provider's copies.
//!
//! When a thread belongs to a provider, FlowX has two records of the same
//! conversation: its own message list, and whatever the provider wrote to disk.
//! The provider's copy identifies an attached image by a placeholder line, not
//! by a reference. To reattach the local bytes, both sides need a stable key —
//! so prompts are fingerprinted and occurrences are counted.
//!
//! Occurrences are counted **newest to oldest**, which is what makes keys
//! stable when a long provider transcript is bounded. Providers bound a
//! conversation by dropping its oldest turns and keeping the newest, so the
//! retained messages keep the occurrence numbers they were assigned.
//!
//! The reverse is not true: dropping messages from the *newest* end would
//! renumber everything below it. Nothing bounds a transcript that way, so the
//! asymmetry is acceptable — but it is the reason [`image_keys_by_message_id`]
//! counts in this direction rather than simply enumerating from the start.

use std::collections::HashMap;

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::models::{ConversationMessage, MessageRole};

/// A prompt fingerprint plus which copy of a repeated prompt this is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImageCorrelationKey {
    pub prompt_digest: String,
    /// 0 for the most recent occurrence, 1 for the one before it, and so on.
    pub reverse_occurrence: usize,
}

pub struct ConversationImageCorrelation;

impl ConversationImageCorrelation {
    /// Fingerprint every user prompt.
    pub fn keys_by_message_id(
        messages: &[ConversationMessage],
    ) -> HashMap<Uuid, ImageCorrelationKey> {
        Self::keys(messages, false)
    }

    /// Fingerprint only prompts that actually carry an image.
    ///
    /// Without this filter, a later text-only prompt with identical wording to
    /// an earlier image prompt would claim occurrence 0 and inherit the older
    /// prompt's asset.
    pub fn image_keys_by_message_id(
        messages: &[ConversationMessage],
    ) -> HashMap<Uuid, ImageCorrelationKey> {
        Self::keys(messages, true)
    }

    /// Whether a message carries an image, inline, by reference, or as a
    /// provider placeholder line.
    pub fn contains_image(message: &ConversationMessage) -> bool {
        message.contains_image()
    }

    fn keys(
        messages: &[ConversationMessage],
        images_only: bool,
    ) -> HashMap<Uuid, ImageCorrelationKey> {
        let mut occurrences: HashMap<String, usize> = HashMap::new();
        let mut result = HashMap::new();

        for message in messages
            .iter()
            .rev()
            .filter(|m| m.role == MessageRole::User)
        {
            if images_only && !Self::contains_image(message) {
                continue;
            }
            let digest = Self::prompt_digest(message);
            let occurrence = occurrences.get(&digest).copied().unwrap_or(0);
            result.insert(
                message.id,
                ImageCorrelationKey {
                    prompt_digest: digest,
                    reverse_occurrence: occurrence,
                },
            );
            occurrences.insert(Self::prompt_digest(message), occurrence + 1);
        }

        result
    }

    /// The fingerprint of a message's text.
    pub fn prompt_digest(message: &ConversationMessage) -> String {
        let normalized = Self::normalized_prompt(message);
        let digest = Sha256::digest(normalized.as_bytes());
        // Lowercase hex, two characters per byte.
        let mut out = String::with_capacity(digest.len() * 2);
        for byte in digest {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }

    /// Reduce a message's text to a comparable form.
    ///
    /// Image placeholder lines are dropped — they are an artifact of how the
    /// provider recorded an attachment, not part of what the user wrote — and
    /// all whitespace is collapsed so rewrapping does not change the digest.
    pub fn normalized_prompt(message: &ConversationMessage) -> String {
        let text = message
            .content
            .iter()
            .filter_map(|c| c.as_text())
            .collect::<Vec<_>>()
            .join("\n");

        text.lines()
            .filter(|line| !crate::models::conversation::is_image_placeholder(line))
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MessageContent;

    fn user(text: &str) -> ConversationMessage {
        ConversationMessage::text(MessageRole::User, text)
    }

    fn user_with_image(text: &str) -> ConversationMessage {
        ConversationMessage::new(
            MessageRole::User,
            vec![
                MessageContent::Text { text: text.into() },
                MessageContent::Image {
                    data: vec![1, 2, 3],
                    mime_type: "image/png".into(),
                },
            ],
        )
    }

    #[test]
    fn the_digest_is_stable_across_calls() {
        let m = user("fix the parser");
        assert_eq!(
            ConversationImageCorrelation::prompt_digest(&m),
            ConversationImageCorrelation::prompt_digest(&m)
        );
    }

    #[test]
    fn the_digest_is_lowercase_hex_of_the_expected_length() {
        let digest = ConversationImageCorrelation::prompt_digest(&user("hello"));
        assert_eq!(digest.len(), 64, "SHA-256 is 32 bytes, 64 hex chars");
        assert!(
            digest
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "not lowercase hex: {digest}"
        );
    }

    #[test]
    fn a_known_digest_matches_sha256_of_the_normalized_text() {
        // The empty string's SHA-256, a fixed point to catch accidental changes
        // to the hashing scheme.
        let m = ConversationMessage::new(MessageRole::User, vec![]);
        assert_eq!(
            ConversationImageCorrelation::prompt_digest(&m),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn whitespace_differences_do_not_change_the_digest() {
        let a = user("fix   the parser");
        let b = user("fix the\nparser");
        assert_eq!(
            ConversationImageCorrelation::prompt_digest(&a),
            ConversationImageCorrelation::prompt_digest(&b),
            "rewrapping changed the digest"
        );
    }

    #[test]
    fn different_text_produces_different_digests() {
        assert_ne!(
            ConversationImageCorrelation::prompt_digest(&user("fix the parser")),
            ConversationImageCorrelation::prompt_digest(&user("fix the layout"))
        );
    }

    #[test]
    fn image_placeholder_lines_are_excluded_from_the_digest() {
        // A provider-recorded image must not change what the user wrote.
        let plain = user("look at this");
        let with_placeholder = user("look at this\n[Image]");
        assert_eq!(
            ConversationImageCorrelation::prompt_digest(&plain),
            ConversationImageCorrelation::prompt_digest(&with_placeholder)
        );
    }

    #[test]
    fn an_attached_image_banner_is_excluded_from_the_digest() {
        let plain = user("look at this");
        let bannered = user("look at this\nAttached image: /tmp/a.png");
        assert_eq!(
            ConversationImageCorrelation::prompt_digest(&plain),
            ConversationImageCorrelation::prompt_digest(&bannered)
        );
    }

    #[test]
    fn occurrences_count_newest_to_oldest() {
        let mut first = user_with_image("same words");
        let mut second = user_with_image("same words");
        let mut third = user_with_image("same words");
        // Give them known ids in chronological order.
        first.id = Uuid::from_u128(1);
        second.id = Uuid::from_u128(2);
        third.id = Uuid::from_u128(3);

        let messages = vec![first, second, third];
        let keys = ConversationImageCorrelation::image_keys_by_message_id(&messages);

        assert_eq!(keys[&Uuid::from_u128(3)].reverse_occurrence, 0, "newest");
        assert_eq!(keys[&Uuid::from_u128(2)].reverse_occurrence, 1);
        assert_eq!(keys[&Uuid::from_u128(1)].reverse_occurrence, 2, "oldest");

        let digest = &keys[&Uuid::from_u128(1)].prompt_digest;
        assert!(keys.values().all(|k| &k.prompt_digest == digest));
    }

    #[test]
    fn a_text_only_prompt_does_not_shift_an_image_prompts_occurrence() {
        // The scenario the image-only variant exists for: a later text-only
        // prompt with identical wording must not claim occurrence 0 and inherit
        // the older prompt's asset.
        let mut image_prompt = user_with_image("deploy the app");
        image_prompt.id = Uuid::from_u128(10);
        let mut later_text = user("deploy the app");
        later_text.id = Uuid::from_u128(11);

        let messages = vec![image_prompt, later_text];

        let all = ConversationImageCorrelation::keys_by_message_id(&messages);
        let images = ConversationImageCorrelation::image_keys_by_message_id(&messages);

        assert_eq!(all.len(), 2, "the unfiltered variant sees both");
        assert_eq!(
            images.len(),
            1,
            "the filtered variant skips the text prompt"
        );
        assert_eq!(images[&Uuid::from_u128(10)].reverse_occurrence, 0);
        assert_eq!(all[&Uuid::from_u128(11)].reverse_occurrence, 0);
    }

    #[test]
    fn only_user_messages_are_fingerprinted() {
        let assistant = ConversationMessage::text(MessageRole::Assistant, "same words");
        let user_message = user("same words");
        let keys = ConversationImageCorrelation::keys_by_message_id(&[
            assistant.clone(),
            user_message.clone(),
        ]);
        assert_eq!(keys.len(), 1);
        assert!(keys.contains_key(&user_message.id));
    }

    #[test]
    fn keys_are_stable_when_the_oldest_messages_are_dropped() {
        // This is how a provider transcript is bounded: the oldest turns fall
        // off the front and the newest are kept. Every retained message must
        // keep the occurrence number it was assigned.
        let mut messages: Vec<_> = (0..5).map(|_| user_with_image("repeated prompt")).collect();
        for (i, m) in messages.iter_mut().enumerate() {
            m.id = Uuid::from_u128(i as u128);
        }

        let full = ConversationImageCorrelation::image_keys_by_message_id(&messages);
        let trimmed = ConversationImageCorrelation::image_keys_by_message_id(&messages[1..]);

        assert_eq!(trimmed.len(), 4);
        for id in trimmed.keys() {
            assert_eq!(full[id], trimmed[id], "message {id} shifted when trimmed");
        }
    }

    #[test]
    fn dropping_the_newest_messages_does_renumber_the_rest() {
        // Documented consequence of counting newest-first. No transcript is
        // bounded this way, so nothing depends on it, but the test pins the
        // behavior rather than leaving it to be discovered.
        let mut messages: Vec<_> = (0..3).map(|_| user_with_image("repeated prompt")).collect();
        for (i, m) in messages.iter_mut().enumerate() {
            m.id = Uuid::from_u128(i as u128);
        }

        let full = ConversationImageCorrelation::image_keys_by_message_id(&messages);
        let shortened = ConversationImageCorrelation::image_keys_by_message_id(&messages[0..2]);

        assert_ne!(
            full[&Uuid::from_u128(1)].reverse_occurrence,
            shortened[&Uuid::from_u128(1)].reverse_occurrence
        );
    }

    #[test]
    fn normalization_joins_multiple_text_blocks_with_newlines() {
        let m = ConversationMessage::new(
            MessageRole::User,
            vec![
                MessageContent::Text {
                    text: "first".into(),
                },
                MessageContent::Text {
                    text: "second".into(),
                },
            ],
        );
        assert_eq!(
            ConversationImageCorrelation::normalized_prompt(&m),
            "first second"
        );
    }

    #[test]
    fn contains_image_accepts_inline_blocks_and_placeholders() {
        assert!(ConversationImageCorrelation::contains_image(
            &user_with_image("x")
        ));
        assert!(ConversationImageCorrelation::contains_image(&user(
            "[Image]"
        )));
        assert!(!ConversationImageCorrelation::contains_image(&user(
            "plain"
        )));
    }
}
