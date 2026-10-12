use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Timestamp, now};

/// Serialize bytes as base64 rather than a JSON array of integers.
///
/// Conversation files carry image bytes. A decimal array would be roughly six
/// times larger and much slower to parse.
pub mod base64_bytes {
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            bytes,
        ))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, s.as_bytes())
            .map_err(D::Error::custom)
    }
}

/// An image the user attached to a prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: Uuid,
    #[serde(with = "base64_bytes")]
    pub data: Vec<u8>,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    pub filename: String,
}

impl Attachment {
    pub fn new(data: Vec<u8>, mime_type: impl Into<String>, filename: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            data,
            mime_type: mime_type.into(),
            filename: filename.into(),
        }
    }

    pub fn is_image(&self) -> bool {
        self.mime_type.starts_with("image/")
    }

    pub fn is_pdf(&self) -> bool {
        self.mime_type == "application/pdf"
    }

    /// Uniform type identifiers FlowX accepts as image input.
    pub const SUPPORTED_IMAGE_TYPES: [&'static str; 9] = [
        "public.jpeg",
        "public.png",
        "public.gif",
        "public.webp",
        "public.heic",
        "public.heif",
        "public.tiff",
        "com.microsoft.bmp",
        "public.bmp",
    ];

    /// Types the composer advertises.
    ///
    /// Provider turn protocols accept images only, so this deliberately
    /// excludes PDF even though [`Attachment::is_pdf`] still recognizes one.
    /// Advertising a type that would be dropped before reaching the model is
    /// worse than not offering it.
    pub fn supported_types() -> Vec<&'static str> {
        Self::SUPPORTED_IMAGE_TYPES.to_vec()
    }

    pub fn mime_type_for_ut_type(ut_type: &str) -> &'static str {
        match ut_type {
            "public.jpeg" => "image/jpeg",
            "public.png" => "image/png",
            "public.gif" => "image/gif",
            "public.webp" => "image/webp",
            "public.heic" | "public.heif" => "image/heic",
            "public.tiff" => "image/tiff",
            "public.bmp" | "com.microsoft.bmp" => "image/bmp",
            "com.adobe.pdf" => "application/pdf",
            _ => "application/octet-stream",
        }
    }

    pub fn mime_type_for_extension(ext: &str) -> &'static str {
        match ext.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "heic" | "heif" => "image/heic",
            "tiff" | "tif" => "image/tiff",
            "bmp" => "image/bmp",
            "pdf" => "application/pdf",
            _ => "application/octet-stream",
        }
    }

    /// Split a filename into its stem and lowercase extension.
    pub fn split_extension(filename: &str) -> (String, String) {
        match filename.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() && !ext.contains('/') => {
                (stem.to_string(), ext.to_ascii_lowercase())
            }
            _ => (filename.to_string(), String::new()),
        }
    }
}

/// Who produced a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

/// A pointer to an image stored on disk rather than inline.
///
/// Message payloads can carry hundreds of images; keeping bytes in a sidecar
/// keeps the conversation JSON small enough to load quickly.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConversationImageAssetReference {
    #[serde(rename = "projectID")]
    pub project_id: Uuid,
    #[serde(rename = "agentID")]
    pub agent_id: Uuid,
    #[serde(rename = "messageID")]
    pub message_id: Uuid,
    #[serde(rename = "contentIndex")]
    pub content_index: usize,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    #[serde(rename = "byteCount")]
    pub byte_count: usize,
}

impl ConversationImageAssetReference {
    /// The on-disk filename for this reference.
    pub fn filename(&self) -> String {
        format!("{}-{}.asset", self.message_id, self.content_index)
    }
}

/// One piece of a message.
///
/// Serialized with an internal `type` tag and flat fields, which keeps the
/// shape readable and matches what the original wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum MessageContent {
    Text {
        text: String,
    },
    Code {
        language: String,
        code: String,
    },
    Image {
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    ImageAsset(ConversationImageAssetReference),
    ToolUse {
        id: String,
        name: String,
        input: String,
    },
    ToolResult {
        id: String,
        content: String,
        #[serde(rename = "isError", default)]
        is_error: bool,
    },
}

impl MessageContent {
    /// The text this block contributes, if any.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            Self::Code { code, .. } => Some(code),
            _ => None,
        }
    }

    /// Whether this block carries image bytes, inline or by reference.
    pub fn is_image(&self) -> bool {
        matches!(self, Self::Image { .. } | Self::ImageAsset(_))
    }

    /// Whether this block is a tool invocation.
    pub fn is_tool_use(&self) -> bool {
        matches!(self, Self::ToolUse { .. })
    }

    /// Whether this block is a tool result.
    pub fn is_tool_result(&self) -> bool {
        matches!(self, Self::ToolResult { .. })
    }
}

/// A single message in a transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub id: Uuid,
    pub role: MessageRole,
    pub content: Vec<MessageContent>,
    pub timestamp: Timestamp,
}

impl ConversationMessage {
    pub fn new(role: MessageRole, content: Vec<MessageContent>) -> Self {
        Self {
            id: Uuid::new_v4(),
            role,
            content,
            timestamp: now(),
        }
    }

    pub fn text(role: MessageRole, text: impl Into<String>) -> Self {
        Self::new(role, vec![MessageContent::Text { text: text.into() }])
    }

    /// Every text block joined, which is what the collapsed sidebar preview and
    /// the search index both read.
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(MessageContent::as_text)
            .collect()
    }

    pub fn contains_image(&self) -> bool {
        self.content.iter().any(|c| {
            if c.is_image() {
                return true;
            }
            // A provider-native transcript records images as a placeholder line
            // rather than as a content block, so text counts too.
            if let Some(text) = c.as_text() {
                return text.lines().any(is_image_placeholder);
            }
            false
        })
    }
}

/// Whether a line is one of the provider's stand-ins for an attached image.
pub fn is_image_placeholder(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed == "[Image]" || trimmed.starts_with("Attached image:")
}

/// A transcript and the thread it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: Uuid,
    #[serde(rename = "agentID")]
    pub agent_id: Uuid,
    #[serde(default)]
    pub messages: Vec<ConversationMessage>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl Conversation {
    pub fn new(agent_id: Uuid) -> Self {
        let now = now();
        Self {
            id: Uuid::new_v4(),
            agent_id,
            messages: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    pub fn push(&mut self, message: ConversationMessage) {
        self.updated_at = now();
        self.messages.push(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_content_round_trips_through_the_type_tag() {
        let cases = vec![
            MessageContent::Text {
                text: "hello".into(),
            },
            MessageContent::Code {
                language: "rust".into(),
                code: "fn main() {}".into(),
            },
            MessageContent::Image {
                data: vec![1, 2, 3, 4],
                mime_type: "image/png".into(),
            },
            MessageContent::ImageAsset(ConversationImageAssetReference {
                project_id: Uuid::nil(),
                agent_id: Uuid::nil(),
                message_id: Uuid::nil(),
                content_index: 3,
                mime_type: "image/jpeg".into(),
                byte_count: 1024,
            }),
            MessageContent::ToolUse {
                id: "t1".into(),
                name: "shell".into(),
                input: "{}".into(),
            },
            MessageContent::ToolResult {
                id: "t1".into(),
                content: "done".into(),
                is_error: false,
            },
        ];
        for content in cases {
            let json = serde_json::to_string(&content).unwrap();
            assert!(json.contains("\"type\""), "no type tag in {json}");
            let back: MessageContent = serde_json::from_str(&json).unwrap();
            assert_eq!(content, back, "round trip changed {content:?}");
        }
    }

    #[test]
    fn an_unknown_content_type_is_rejected_rather_than_ignored() {
        let json = r#"{"type":"telepathy","text":"hi"}"#;
        assert!(serde_json::from_str::<MessageContent>(json).is_err());
    }

    #[test]
    fn image_bytes_survive_the_round_trip() {
        // Bytes that are not valid UTF-8, to prove this is not stringly typed.
        let bytes: Vec<u8> = (0u8..=255).collect();
        let content = MessageContent::Image {
            data: bytes.clone(),
            mime_type: "image/png".into(),
        };
        let json = serde_json::to_string(&content).unwrap();
        let back: MessageContent = serde_json::from_str(&json).unwrap();
        match back {
            MessageContent::Image { data, .. } => assert_eq!(data, bytes),
            other => panic!("decoded as {other:?}"),
        }
    }

    #[test]
    fn text_content_joins_every_text_block() {
        let m = ConversationMessage::new(
            MessageRole::Assistant,
            vec![
                MessageContent::Text {
                    text: "one ".into(),
                },
                MessageContent::Text { text: "two".into() },
                MessageContent::ToolUse {
                    id: "t".into(),
                    name: "n".into(),
                    input: "i".into(),
                },
            ],
        );
        assert_eq!(m.text_content(), "one two");
    }

    #[test]
    fn an_image_block_counts_as_an_image() {
        let m = ConversationMessage::new(
            MessageRole::User,
            vec![MessageContent::Image {
                data: vec![0],
                mime_type: "image/png".into(),
            }],
        );
        assert!(m.contains_image());
    }

    #[test]
    fn a_provider_placeholder_line_counts_as_an_image() {
        for body in ["[Image]", "Attached image: a.png", "  [Image]  "] {
            let m = ConversationMessage::text(MessageRole::User, body);
            assert!(m.contains_image(), "{body:?} was not recognized");
        }
    }

    #[test]
    fn ordinary_text_does_not_count_as_an_image() {
        for body in ["[Images]", "Attached images: x", "see [Image] below", ""] {
            let m = ConversationMessage::text(MessageRole::User, body);
            assert!(!m.contains_image(), "{body:?} was misread as an image");
        }
    }

    #[test]
    fn a_multi_line_prompt_counts_if_any_line_is_a_placeholder() {
        let m = ConversationMessage::text(MessageRole::User, "look at this\n[Image]\nthanks");
        assert!(m.contains_image());
    }

    #[test]
    fn mime_types_resolve_from_both_ut_types_and_extensions() {
        assert_eq!(
            Attachment::mime_type_for_ut_type("public.jpeg"),
            "image/jpeg"
        );
        assert_eq!(Attachment::mime_type_for_extension("JPG"), "image/jpeg");
        assert_eq!(Attachment::mime_type_for_extension("heif"), "image/heic");
        assert_eq!(
            Attachment::mime_type_for_extension("wat"),
            "application/octet-stream"
        );
    }

    #[test]
    fn extensions_split_into_stem_and_lowercase_suffix() {
        assert_eq!(
            Attachment::split_extension("Photo.JPG"),
            ("Photo".into(), "jpg".into())
        );
        assert_eq!(
            Attachment::split_extension("noext"),
            ("noext".into(), String::new())
        );
        assert_eq!(
            Attachment::split_extension(".hidden"),
            (".hidden".into(), String::new())
        );
    }

    #[test]
    fn the_advertised_types_are_images_only() {
        for ut in Attachment::supported_types() {
            let mime = Attachment::mime_type_for_ut_type(ut);
            assert!(
                mime.starts_with("image/"),
                "{ut} advertises as {mime}, which the providers would drop"
            );
        }
    }
}
