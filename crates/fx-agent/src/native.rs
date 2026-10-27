//! Provider-owned history used by the workspace's read-only synchronization.

use fx_core::{ConversationMessage, NativeDefaults};

use crate::ThreadSummary;

#[derive(Debug, Clone, PartialEq)]
pub struct NativeThreadSummary {
    pub summary: ThreadSummary,
    pub defaults: NativeDefaults,
    /// Changes whenever provider metadata or transcript contents change.
    pub revision: String,
    /// What the thread is blocked on, when the provider reports a process
    /// FlowX does not run waiting on the user: an approval, a question, an
    /// MCP server's request. FlowX cannot answer it — only the process that
    /// asked can hear the answer — so it is shown, not offered.
    pub waiting_elsewhere: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NativeThread {
    pub entry: NativeThreadSummary,
    pub messages: Vec<ConversationMessage>,
    pub current_context_tokens: Option<u64>,
    /// A size or protocol limit must be surfaced, never silently called full history.
    pub history_truncated: bool,
}

#[derive(Debug, Clone, Default)]
pub struct NativeThreadIndex {
    pub threads: Vec<NativeThreadSummary>,
    pub warnings: Vec<String>,
}
