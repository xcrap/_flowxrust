//! Provider-owned history used by the workspace's read-only synchronization.

use fx_core::{ConversationMessage, NativeDefaults};

use crate::ThreadSummary;

#[derive(Debug, Clone, PartialEq)]
pub struct NativeThreadSummary {
    pub summary: ThreadSummary,
    pub defaults: NativeDefaults,
    /// Changes whenever provider metadata or transcript contents change.
    pub revision: String,
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
