//! Case-insensitive, all-word matching over the human-readable conversation.
//! Image bytes and provider bookkeeping never become searchable text.

use super::transcript_presentation::user_prompt;
use crate::{ConversationMessage, MessageContent, MessageRole};

#[derive(Clone, Debug)]
pub struct MessageQuery {
    words: Vec<String>,
}

impl MessageQuery {
    pub fn new(query: &str) -> Self {
        Self {
            words: query.split_whitespace().map(str::to_lowercase).collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    pub fn snippet(&self, message: &ConversationMessage) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        // These are the messages the transcript presents as prose. Commands
        // and their raw results have their own disclosures and are not matches.
        let text = message
            .content
            .iter()
            .filter_map(|block| match block {
                MessageContent::Text { text } if message.role == MessageRole::User => {
                    Some(user_prompt(text).text.to_owned())
                }
                MessageContent::Text { text } => Some(text.clone()),
                MessageContent::Code { code, .. } => Some(code.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let folded = text.to_lowercase();
        if !self.words.iter().all(|word| folded.contains(word)) {
            return None;
        }
        let offset = folded.find(&self.words[0])?;
        // Lowercasing can expand a Unicode character. Map the folded byte
        // offset back to a source character before taking the excerpt.
        let mut bytes = 0;
        let mut index: usize = 0;
        for character in text.chars() {
            if bytes >= offset {
                break;
            }
            bytes += character.to_lowercase().map(char::len_utf8).sum::<usize>();
            index += 1;
        }
        let start = index.saturating_sub(45);
        let mut excerpt = text
            .chars()
            .skip(start)
            .take(180)
            .map(|c| if c.is_whitespace() { ' ' } else { c })
            .collect::<String>();
        if start > 0 {
            excerpt.insert(0, '…');
        }
        if text.chars().count() > start + 180 {
            excerpt.push('…');
        }
        Some(excerpt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn words_match_in_any_order_with_unicode_case_and_bounded_context() {
        let message = ConversationMessage::new(
            MessageRole::User,
            vec![MessageContent::Text {
                text: format!("{}İSTANBUL fixes AÇORES 🐳", "prefix ".repeat(80)),
            }],
        );
        let found = MessageQuery::new("açores istanbul").snippet(&message);
        // Dotted I is not the same Unicode spelling as plain i.
        assert!(found.is_none());
        let found = MessageQuery::new("açores fixes").snippet(&message).unwrap();
        assert!(found.contains("AÇORES 🐳"));
        assert!(found.chars().count() <= 182);
        assert!(MessageQuery::new("missing").snippet(&message).is_none());
        assert!(MessageQuery::new(" ").snippet(&message).is_none());
    }
}
