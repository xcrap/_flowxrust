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
    /// The message the deltas were building is complete; the next delta
    /// starts a new one.
    TextEnd,
    /// A whole message that did not stream.
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
    /// The agent rewrote its task list. Codex reports its `update_plan` calls
    /// this way rather than as a tool item; Claude's `TodoWrite` arrives as an
    /// ordinary tool call instead.
    PlanUpdated(fx_core::policy::TurnPlan),
    /// Usage for one turn, added to the thread's totals.
    Usage(Usage),
    /// The thread's running totals as the provider counts them, which replace
    /// rather than add to what the conversation holds.
    CumulativeUsage(Usage),
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
    /// A background task the turn started — a subagent, or a command run in
    /// the background — started, reported progress, or finished.
    Task(TaskUpdate),
    /// The provider took input FlowX wrote into the running turn: the message
    /// FlowX sent under this id, or, from a provider that does not say which,
    /// everything written so far.
    ///
    /// Taken, not accepted. Both providers accept a steer at once and read it
    /// at their next request, after whatever they are saying or running.
    InputTaken {
        id: Option<String>,
    },
}

/// One report about a background task.
///
/// Partial by design: `task_started` names the task, `task_progress` counts its
/// work, and `task_notification` says how it ended. Each field is `None` where
/// that report did not carry it, and the receiver keeps what it already knew.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskUpdate {
    /// The provider's own id for the task.
    pub id: String,
    /// The tool call that started it.
    pub tool_use_id: Option<String>,
    pub description: Option<String>,
    /// What runs it, in the provider's words: `local_bash`, `local_agent`.
    pub kind: Option<String>,
    /// The subagent type, for a task that is an agent.
    pub subagent_type: Option<String>,
    pub status: TaskStatus,
    pub summary: Option<String>,
    /// The last tool the task used.
    pub last_tool: Option<String>,
    pub tool_uses: Option<u64>,
    pub tokens: Option<u64>,
    /// How long the task had been running when this was reported.
    pub duration_ms: Option<u64>,
}

/// Where a background task is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskStatus {
    #[default]
    Running,
    Completed,
    Failed,
    /// Killed or stopped before it finished, by the user, the provider, or
    /// FlowX giving up on it.
    Stopped,
}

impl TaskStatus {
    pub fn is_running(self) -> bool {
        self == Self::Running
    }

    /// The provider's word for how a task ended. Anything other than success
    /// or failure is a stop: `killed`, `stopped`, and whatever a later version
    /// adds, none of which is a result to report as one.
    pub fn from_notification(status: &str) -> Self {
        match status {
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            _ => Self::Stopped,
        }
    }
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

#[derive(Debug, Clone, PartialEq, Default)]
pub struct UserInputRequest {
    pub id: String,
    /// Empty for a request that asks only for consent: an MCP server's
    /// confirmation, or a link it asks the reader to open.
    pub questions: Vec<Question>,
    /// Who is asking, when it is not the agent: an MCP server's name.
    pub source: Option<String>,
    /// What the asker said about the request as a whole.
    pub message: Option<String>,
    /// A web page the asker wants opened. Answering is consenting to open it.
    pub link: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Question {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<Option_>,
    /// Whether a typed answer is taken. Without it, and with options, the
    /// answer is one of the options.
    pub is_other: bool,
    pub is_secret: bool,
    /// Whether it may be left blank.
    pub optional: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
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
