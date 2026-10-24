//! Domain models shared by every layer of the app.
//!
//! These types are the vocabulary the whole workspace speaks. They carry no
//! behaviour beyond what is genuinely part of the value — the policies live in
//! [`crate::policy`], because keeping them separate is what lets those policies
//! be tested without constructing an app.

pub mod agent;
pub mod conversation;
pub mod git;
pub mod goal;
pub mod project;
pub mod tool_approval;

pub use agent::{
    ALL_EFFORTS, Agent, AgentAccess, AgentConfiguration, AgentExecutionState, AppDefaults,
    DEFAULT_CODEX_MODEL_ID, DEFAULT_EFFORT, DEFAULT_PROVIDER_ID, NativeDefaults,
    ResolvedConfiguration,
};
pub use conversation::{
    Attachment, Conversation, ConversationImageAssetReference, ConversationMessage, MessageContent,
    MessageRole,
};
pub use git::{GitFileStatus, GitRepositoryInfo};
pub use goal::{ConversationGoal, GoalStatus};
pub use project::Project;
pub use tool_approval::{
    ApprovalStatus, ToolApprovalRequest, ToolRiskLevel, UserInputOption, UserInputQuestion,
    UserInputRequest, UserInputResponse,
};

/// RFC 3339 timestamps, the format both the on-disk format and the provider
/// protocols use.
///
/// Kept as an alias so a decision to switch representations is a one-line
/// change rather than a sweep through every call site.
pub type Timestamp = chrono::DateTime<chrono::Utc>;

/// The current time.
///
/// chrono puts `now()` on `Utc` rather than on `DateTime<Tz>`, so this is the
/// one place that knows it.
pub fn now() -> Timestamp {
    chrono::Utc::now()
}
