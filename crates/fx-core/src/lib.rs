//! `fx-core` — the domain layer.
//!
//! Domain models, pure decision procedures, git output parsing, runtime
//! discovery, and image preparation. This crate deliberately does **not** depend
//! on GPUI: anything that needs to draw belongs in `fx-design` or `fx-app`, and
//! keeping the dependency out is what lets all of this be tested without a
//! window.

pub mod git;
pub mod models;
pub mod policy;
pub mod services;

pub use models::{
    Agent, AgentAccess, AgentConfiguration, AgentExecutionState, AgentMode, AppDefaults,
    ApprovalStatus, Attachment, Conversation, ConversationImageAssetReference, ConversationMessage,
    GitFileStatus, GitRepositoryInfo, MessageContent, MessageRole, NativeDefaults, Project,
    ResolvedConfiguration, Timestamp, ToolApprovalRequest, ToolRiskLevel, UserInputOption,
    UserInputQuestion, UserInputRequest, UserInputResponse,
};

/// Provider and model identifiers used when a thread states none.
pub use models::{ALL_EFFORTS, DEFAULT_CODEX_MODEL_ID, DEFAULT_EFFORT, DEFAULT_PROVIDER_ID};
