//! The provider interface.
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use fx_core::Timestamp;

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("{0}")]
    Unavailable(String),
    #[error("the turn was cancelled")]
    Cancelled,
    #[error("{0}")]
    Protocol(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// A model a provider offers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub context_window: u64,
    #[serde(default)]
    pub max_context_window: u64,
    #[serde(default)]
    pub supported_efforts: Vec<String>,
    #[serde(default)]
    pub default_effort: Option<String>,
    #[serde(default)]
    pub supports_vision: bool,
    #[serde(default)]
    pub is_default: bool,
}

/// Health of a provider's CLI on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ProviderHealth {
    #[default]
    Unknown,
    Available {
        path: String,
        version: String,
    },
    Quarantined {
        path: String,
    },
    NotFound,
}

impl ProviderHealth {
    pub fn is_usable(&self) -> bool {
        matches!(self, Self::Available { .. })
    }

    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Available { path, .. } | Self::Quarantined { path } => Some(path),
            _ => None,
        }
    }
}

/// Identity of a provider-native thread.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ThreadIdentity {
    pub provider_id: String,
    pub session_id: String,
}

/// A provider thread, as listed.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadSummary {
    pub identity: ThreadIdentity,
    pub title: String,
    pub working_directory: String,
    pub updated_at: Timestamp,
    pub is_archived: bool,
}

/// One thing that happened during a turn.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ProviderEvent {
    Lifecycle(Lifecycle),
    TextDelta(String),
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input: String,
    },
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
    Approval(ApprovalRequest),
    UserInput(UserInputRequest),
    Usage(Usage),
    ContextUsage {
        used: u64,
        window: u64,
    },
    ModelChanged {
        model: String,
        reason: String,
    },
    Error(String),
    Done {
        stop_reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Lifecycle {
    TurnStarted,
    Compacting,
    Compacted,
    Preparing,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    pub id: String,
    pub tool_name: String,
    pub input: Value,
    pub risk: RiskLevel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RiskLevel {
    Safe,
    #[default]
    Moderate,
    Dangerous,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserInputRequest {
    pub id: String,
    pub questions: Vec<Question>,
}

/// A response to one request from a running provider turn.
///
/// `id` is the enclosing ApprovalRequest/UserInputRequest id. Question answers
/// pair each nested `Question::id` with its chosen label or free-form text.
/// Transport-specific wire ids never escape into UI callbacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserResponse {
    Approval {
        id: String,
        approved: bool,
    },
    Questions {
        id: String,
        answers: Vec<(String, String)>,
    },
    CancelQuestions {
        id: String,
    },
}

impl UserResponse {
    pub fn id(&self) -> &str {
        match self {
            Self::Approval { id, .. }
            | Self::Questions { id, .. }
            | Self::CancelQuestions { id } => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<Option_>,
    pub is_other: bool,
    pub is_secret: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Option_ {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
}

/// The most questions Codex asks in one turn.
pub const MAX_QUESTIONS: usize = 3;
/// The most options any one question offers.
pub const MAX_OPTIONS: usize = 10;

/// Where a running turn's events go.
#[async_trait]
pub trait StreamSink: Send {
    async fn emit(&self, event: ProviderEvent);
}

/// One agent runtime.
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    fn health(&self) -> ProviderHealth;
    fn models(&self) -> Vec<Model>;
    async fn list_threads(
        &self,
        working_directory: &str,
    ) -> Result<Vec<ThreadSummary>, ProviderError>;
    async fn read_thread(&self, identity: &ThreadIdentity) -> Result<String, ProviderError>;
}
