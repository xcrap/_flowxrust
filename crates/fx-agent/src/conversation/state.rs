//! The UI model of record for one conversation.
//!
//! This is the per-thread state the conversation view reads: messages, the
//! streaming text, queued follow-ups, pending approvals and questions, usage
//! accumulators. It is deliberately free of IO — the engine pushes updates in,
//! and the view reads them out.
//!
//! Two invariants are load-bearing and are tested here:
//!
//! - **Live tails are bounded.** Fresh streamed conversations retain a bounded
//!   tail. Restored provider history has reader byte limits instead and remains
//!   complete when a new turn is appended.
//! - **Cancelling is terminal.** Once a turn is cancelling, only `idle` or
//!   `failed` may replace that phase, so a late completion cannot report success
//!   for work the user already stopped.

use std::collections::VecDeque;

use fx_core::{AgentAccess, ConversationMessage, MessageRole, Timestamp};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::provider::{ProviderEvent, RiskLevel, Usage};

/// How many messages a conversation retains.
pub const MAX_RETAINED_MESSAGES: usize = 250;
/// How many pending approvals are tracked before the oldest is forgotten.
pub const MAX_PENDING_APPROVALS: usize = 64;
/// How many completed tool ids are remembered for de-duplication.
pub const MAX_COMPLETED_TOOL_IDS: usize = 512;
/// How many runtime activity records are retained.
pub const MAX_RUNTIME_ACTIVITIES: usize = 200;

/// A successful tool result carrying this exact text is not worth a row.
///
/// The Swift drops it: a tool that succeeded and said only "Completed" would
/// otherwise add a message per call and bury the prose the user actually reads.
pub const TRIVIAL_TOOL_RESULT: &str = "Completed";
/// How many queued follow-ups are shown in the composer.
pub const VISIBLE_QUEUED_PROMPTS: usize = 3;
/// Longest preview of a queued prompt.
pub const QUEUED_PROMPT_PREVIEW_CHARS: usize = 80;

/// Where a turn is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Idle,
    Starting,
    Running,
    WaitingForApproval,
    WaitingForInput,
    Compacting,
    Cancelling,
    Failed,
}

impl Phase {
    /// Whether this phase refuses to be replaced by a later one.
    ///
    /// Cancelling is terminal: a stream that completes after the user pressed
    /// stop must not report success.
    pub fn is_terminal(self) -> bool {
        self == Self::Cancelling
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Starting => "Starting",
            Self::Running => "Working",
            Self::WaitingForApproval => "Waiting for approval",
            Self::WaitingForInput => "Waiting for an answer",
            Self::Compacting => "Compacting context",
            Self::Cancelling => "Stopping",
            Self::Failed => "Failed",
        }
    }
}

/// A tool call awaiting a decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingApproval {
    pub id: String,
    pub tool_name: String,
    pub input: serde_json::Value,
    #[serde(default)]
    pub risk: RiskLevelSerde,
}

/// Serializable form of [`RiskLevel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RiskLevelSerde {
    Safe,
    #[default]
    Moderate,
    Dangerous,
}

impl From<RiskLevel> for RiskLevelSerde {
    fn from(value: RiskLevel) -> Self {
        match value {
            RiskLevel::Safe => Self::Safe,
            RiskLevel::Moderate => Self::Moderate,
            RiskLevel::Dangerous => Self::Dangerous,
        }
    }
}

/// A structured question awaiting an answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingQuestion {
    pub id: String,
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub is_other: bool,
}

/// One selectable answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

/// A prompt queued behind the running turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedPrompt {
    pub id: Uuid,
    pub text: String,
    /// Attachment count, so the composer can say "3 images".
    pub attachment_count: usize,
}

impl QueuedPrompt {
    /// A one-line preview for the composer's queue strip.
    pub fn preview(&self) -> String {
        let mut text: String = self
            .text
            .chars()
            .take(QUEUED_PROMPT_PREVIEW_CHARS)
            .collect();
        if self.text.chars().count() > QUEUED_PROMPT_PREVIEW_CHARS {
            text.push('…');
        }
        if self.attachment_count > 0 {
            text.push_str(&format!(" (+{} image)", self.attachment_count));
        }
        text
    }
}

/// Something worth showing above the transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeActivity {
    pub id: Uuid,
    pub timestamp: Timestamp,
    pub summary: String,
    #[serde(default)]
    pub detail: Option<String>,
    /// A session-level record is shown once, not repeated.
    #[serde(default)]
    pub is_session: bool,
}

/// A prompt that failed to send, kept so the user can retry it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnsentPrompt {
    pub text: String,
    pub attachment_count: usize,
    /// The session it was meant for. A retry only makes sense against the same
    /// one, so a mismatch invalidates the retry rather than sending elsewhere.
    pub session_id: Option<String>,
}

/// Token totals for the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
    /// Tokens in the current context window.
    #[serde(default)]
    pub current_context_tokens: u64,
    /// The window the provider reported, if known.
    #[serde(default)]
    pub reported_context_window: Option<u64>,
}

impl UsageTotals {
    fn add(&mut self, usage: Usage) {
        self.input_tokens += usage.input_tokens;
        self.output_tokens += usage.output_tokens;
        self.cached_input_tokens += usage.cached_input_tokens;
        self.reasoning_output_tokens += usage.reasoning_output_tokens;
        self.total_tokens += usage.total_tokens;
    }

    /// Context fill as a fraction, or `None` when the window is unknown.
    pub fn context_fraction(&self) -> Option<f32> {
        let window = self.reported_context_window.filter(|w| *w > 0)?;
        Some((self.current_context_tokens as f32 / window as f32).clamp(0.0, 1.0))
    }
}

/// The per-thread conversation state.
#[derive(Debug, Clone, Default)]
pub struct Conversation {
    pub id: Uuid,
    pub agent_id: Uuid,
    messages: VecDeque<ConversationMessage>,
    /// Provider history is already bounded by the reader's byte limits. Keep
    /// all of it, including after a new locally streamed turn is appended.
    retain_full_history: bool,
    /// Incremented on every message change, so a view can tell "same length,
    /// different content" apart.
    message_revision: u64,
    streaming_text: String,
    streaming_revision: u64,
    phase: Phase,
    error: Option<String>,
    queued: VecDeque<QueuedPrompt>,
    pending_approvals: Vec<PendingApproval>,
    pending_questions: Vec<PendingQuestion>,
    usage: UsageTotals,
    unsent: Option<UnsentPrompt>,
    /// Ids of tool calls already reported, so a repeated notification does not
    /// duplicate a row.
    completed_tool_ids: VecDeque<String>,
    activities: VecDeque<RuntimeActivity>,
    /// The provider's session, once known.
    session_id: Option<String>,
    /// Bumped whenever anything a view renders changes.
    revision: u64,
}

impl Conversation {
    pub fn new(agent_id: Uuid) -> Self {
        Self {
            id: Uuid::new_v4(),
            agent_id,
            ..Default::default()
        }
    }

    // ── Messages ───────────────────────────────────────────────────────────

    pub fn messages(&self) -> impl ExactSizeIterator<Item = &ConversationMessage> {
        self.messages.iter()
    }

    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    /// Bumped on every message mutation.
    pub fn message_revision(&self) -> u64 {
        self.message_revision
    }

    pub fn append(&mut self, message: ConversationMessage) {
        self.messages.push_back(message);
        self.trim();
        self.message_revision += 1;
        self.touch();
    }

    /// Drop the oldest messages past the retention cap.
    fn trim(&mut self) {
        if self.retain_full_history {
            return;
        }
        while self.messages.len() > MAX_RETAINED_MESSAGES {
            self.messages.pop_front();
        }
    }

    /// Replace the transcript wholesale, as a provider refresh does.
    ///
    /// An unsent prompt survives: it lives outside the provider-owned
    /// transcript so a refresh cannot erase something the user is trying to
    /// send.
    pub fn replace_messages(&mut self, messages: Vec<ConversationMessage>) {
        self.messages = messages.into();
        self.trim();
        self.message_revision += 1;
        self.touch();
    }

    /// Restore complete provider or disk history without the live-tail cap.
    /// Drafts, usage, and session identity remain untouched.
    pub fn replace_full_history(&mut self, messages: Vec<ConversationMessage>) {
        self.retain_full_history = true;
        self.replace_messages(messages);
    }

    /// The most recent message with a body of text, for the sidebar preview.
    pub fn last_text(&self) -> Option<String> {
        self.messages
            .iter()
            .rev()
            .map(|m| m.text_content())
            .find(|t| !t.trim().is_empty())
    }

    pub fn last_user_prompt(&self) -> Option<String> {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User)
            .map(|m| m.text_content())
    }

    // ── Streaming ───────────────────────────────────────────────────────────

    pub fn streaming_text(&self) -> &str {
        &self.streaming_text
    }

    pub fn streaming_revision(&self) -> u64 {
        self.streaming_revision
    }

    pub fn is_streaming(&self) -> bool {
        !self.streaming_text.is_empty()
    }

    /// Begin streaming, discarding any leftover text from a previous turn.
    pub fn begin_streaming(&mut self) {
        self.streaming_text.clear();
        self.streaming_revision += 1;
        self.touch();
    }

    pub fn append_streaming(&mut self, delta: &str) {
        self.streaming_text.push_str(delta);
        self.streaming_revision += 1;
        self.touch();
    }

    /// Commit whatever has streamed into one message, and start a new segment.
    ///
    /// This is the whole of the interleaving mechanism: text accumulates here,
    /// and a tool call forces a commit so the prose before it and the prose
    /// after it become separate messages. Without that, one turn would be one
    /// message with the calls buried in the middle of it.
    pub fn commit_streaming(&mut self) {
        if self.streaming_text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.streaming_text);
        self.append(ConversationMessage::text(
            fx_core::MessageRole::Assistant,
            text,
        ));
        self.streaming_revision += 1;
    }

    pub fn clear_streaming(&mut self) {
        if !self.streaming_text.is_empty() {
            self.streaming_text.clear();
            self.streaming_revision += 1;
            self.touch();
        }
    }

    // ── Phase ──────────────────────────────────────────────────────────────

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Whether a turn is in flight, including while it waits on the user.
    pub fn is_busy(&self) -> bool {
        !matches!(self.phase, Phase::Idle | Phase::Failed)
    }

    /// Move to a new phase, respecting the cancelling rule.
    pub fn set_phase(&mut self, phase: Phase) -> bool {
        if self.phase.is_terminal() && !matches!(phase, Phase::Idle | Phase::Failed) {
            // A late completion must not resurrect a cancelled turn.
            return false;
        }
        if self.phase != phase {
            self.phase = phase;
            self.touch();
        }
        true
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn set_error(&mut self, message: impl Into<String>) {
        self.error = Some(message.into());
        self.set_phase(Phase::Failed);
        self.touch();
    }

    pub fn dismiss_error(&mut self) {
        if self.error.take().is_some() {
            self.set_phase(Phase::Idle);
            self.touch();
        }
    }

    // ── Queue ──────────────────────────────────────────────────────────────

    pub fn queued(&self) -> impl ExactSizeIterator<Item = &QueuedPrompt> {
        self.queued.iter()
    }

    /// The next `VISIBLE_QUEUED_PROMPTS` previews, oldest first.
    pub fn queued_previews(&self) -> Vec<String> {
        self.queued
            .iter()
            .take(VISIBLE_QUEUED_PROMPTS)
            .map(QueuedPrompt::preview)
            .collect()
    }

    pub fn queued_count(&self) -> usize {
        self.queued.len()
    }

    pub fn enqueue(&mut self, prompt: QueuedPrompt) {
        self.queued.push_back(prompt);
        self.touch();
    }

    /// Take the oldest queued prompt, for sending when the turn ends.
    pub fn dequeue(&mut self) -> Option<QueuedPrompt> {
        let popped = self.queued.pop_front();
        if popped.is_some() {
            self.touch();
        }
        popped
    }

    /// Remove a queued prompt by position, as a context-menu action does.
    pub fn remove_queued_at(&mut self, index: usize) -> Option<QueuedPrompt> {
        if index >= self.queued.len() {
            return None;
        }
        let removed = self.queued.remove(index);
        self.touch();
        removed
    }

    pub fn queued_position(&self, id: Uuid) -> Option<usize> {
        self.queued.iter().position(|q| q.id == id)
    }

    // ── Approvals and questions ─────────────────────────────────────────────

    pub fn pending_approvals(&self) -> &[PendingApproval] {
        &self.pending_approvals
    }

    pub fn add_approval(&mut self, approval: PendingApproval) {
        if self.pending_approvals.iter().any(|a| a.id == approval.id) {
            // The same request arriving twice must not stack up.
            return;
        }
        self.pending_approvals.push(approval);
        while self.pending_approvals.len() > MAX_PENDING_APPROVALS {
            self.pending_approvals.remove(0);
        }
        self.set_phase(Phase::WaitingForApproval);
        self.touch();
    }

    pub fn remove_approval(&mut self, id: &str) -> Option<PendingApproval> {
        let index = self.pending_approvals.iter().position(|a| a.id == id)?;
        let removed = self.pending_approvals.remove(index);
        // Only go back to running when nothing else is waiting. Claude opens an
        // approval and a question at the same time; answering the approval and
        // then reporting "running" hides the question, so the composer opens and
        // the question can never be answered.
        if self.pending_approvals.is_empty() {
            self.set_phase(if self.pending_questions.is_empty() {
                Phase::Running
            } else {
                Phase::WaitingForInput
            });
        }
        self.touch();
        Some(removed)
    }

    pub fn pending_questions(&self) -> &[PendingQuestion] {
        &self.pending_questions
    }

    pub fn add_question(&mut self, question: PendingQuestion) {
        if self.pending_questions.iter().any(|q| q.id == question.id) {
            return;
        }
        self.pending_questions.push(question);
        self.set_phase(Phase::WaitingForInput);
        self.touch();
    }

    pub fn remove_question(&mut self, id: &str) {
        self.pending_questions.retain(|q| q.id != id);
        if self.pending_questions.is_empty() && self.phase == Phase::WaitingForInput {
            self.set_phase(Phase::Running);
        }
        self.touch();
    }

    // ── Usage ──────────────────────────────────────────────────────────────

    pub fn usage(&self) -> UsageTotals {
        self.usage
    }

    /// Restore the accumulated totals without counting them as a new turn.
    pub fn restore_usage(&mut self, usage: UsageTotals) {
        self.usage = usage;
        self.touch();
    }

    pub fn record_usage(&mut self, usage: Usage) {
        self.usage.add(usage);
        self.touch();
    }

    pub fn record_context(&mut self, used: u64, window: u64) {
        self.usage.current_context_tokens = used;
        self.usage.reported_context_window = Some(window);
        self.touch();
    }

    // ── Unsent prompt ──────────────────────────────────────────────────────

    pub fn unsent(&self) -> Option<&UnsentPrompt> {
        self.unsent.as_ref()
    }

    /// Remember a prompt that failed to send.
    pub fn hold_unsent(&mut self, prompt: UnsentPrompt) {
        self.unsent = Some(prompt);
        self.touch();
    }

    /// Forget a prompt only after the provider has accepted it.
    pub fn clear_unsent(&mut self) {
        if self.unsent.take().is_some() {
            self.touch();
        }
    }

    /// Take the unsent prompt, if it still belongs to the current session.
    ///
    /// Comparing the session is what stops a retry from being sent to a
    /// different thread than the one it was written for.
    pub fn take_unsent(&mut self) -> Option<UnsentPrompt> {
        let unsent = self.unsent.take()?;
        if unsent.session_id == self.session_id {
            self.touch();
            Some(unsent)
        } else {
            self.touch();
            None
        }
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn set_session_id(&mut self, session_id: impl Into<String>) {
        self.session_id = Some(session_id.into());
        self.touch();
    }

    // ── Activities ──────────────────────────────────────────────────────────

    /// Record something worth showing above the transcript.
    ///
    /// A session-level record is kept only once; repeating it every turn would
    /// make the strip unreadable.
    pub fn record_activity(&mut self, summary: impl Into<String>, is_session: bool) {
        let summary = summary.into();
        if is_session
            && self
                .activities
                .iter()
                .any(|a| a.is_session && a.summary == summary)
        {
            return;
        }
        self.activities.push_back(RuntimeActivity {
            id: Uuid::new_v4(),
            timestamp: fx_core::models::now(),
            summary,
            detail: None,
            is_session,
        });
        while self.activities.len() > MAX_RUNTIME_ACTIVITIES {
            self.activities.pop_front();
        }
        self.touch();
    }

    pub fn activities(&self) -> impl ExactSizeIterator<Item = &RuntimeActivity> {
        self.activities.iter()
    }

    // ── Tool de-duplication ─────────────────────────────────────────────────

    /// Whether a tool id has not been reported yet, recording it if so.
    ///
    /// Providers can replay an `item/completed` after a resume, which would
    /// otherwise append the same result twice.
    pub fn claim_tool_id(&mut self, id: &str) -> bool {
        if self
            .completed_tool_ids
            .iter()
            .any(|existing| existing == id)
        {
            return false;
        }
        self.completed_tool_ids.push_back(id.to_string());
        while self.completed_tool_ids.len() > MAX_COMPLETED_TOOL_IDS {
            self.completed_tool_ids.pop_front();
        }
        true
    }

    pub fn has_completed_tool(&self, id: &str) -> bool {
        self.completed_tool_ids
            .iter()
            .any(|existing| existing == id)
    }

    // ── Events ─────────────────────────────────────────────────────────────

    /// Apply one provider event.
    pub fn apply(&mut self, event: ProviderEvent) {
        match event {
            ProviderEvent::Lifecycle(lifecycle) => {
                use crate::provider::Lifecycle as L;
                match lifecycle {
                    L::TurnStarted => {
                        self.begin_streaming();
                        self.set_phase(Phase::Running);
                    }
                    L::Compacting => {
                        self.set_phase(Phase::Compacting);
                    }
                    L::Compacted => {
                        self.set_phase(Phase::Running);
                        self.record_activity("Context compacted", true);
                    }
                    L::Preparing => {
                        self.set_phase(Phase::Starting);
                    }
                }
            }
            ProviderEvent::TextDelta(delta) => self.append_streaming(&delta),
            ProviderEvent::Text(text) => {
                // Streamed text arrives as deltas; a completed item with text is
                // the fallback for a provider that does not stream.
                if self.streaming_text.is_empty() {
                    self.append_streaming(&text);
                }
            }
            ProviderEvent::ToolUse { id, name, input } => {
                // A tool call splits the narrative. "Let me check X" is one
                // message, the call is another, and "Found it" a third — that is
                // how a turn interleaves prose and calls without every message
                // having to know about the others.
                self.commit_streaming();
                self.append(ConversationMessage::new(
                    fx_core::MessageRole::Assistant,
                    vec![fx_core::MessageContent::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    }],
                ));
                // Claimed for the activity row's spinner, *not* as an append
                // guard. The result arrives with the same id, so a gate here
                // would reject every legitimate first result.
                self.claim_tool_id(&id);
                self.record_activity(format!("Running {name}"), false);
                self.set_phase(Phase::Running);
            }
            ProviderEvent::ToolResult {
                id,
                content,
                is_error,
            } => {
                self.claim_tool_id(&id);
                // A successful result with nothing to say is not recorded. The
                // Swift drops these deliberately: a `Bash` that returned empty,
                // or the literal "Completed", would otherwise add a row per tool
                // call and bury the prose. Errors are always recorded.
                let trimmed = content.trim();
                let worth_recording =
                    is_error || (!trimmed.is_empty() && trimmed != TRIVIAL_TOOL_RESULT);
                if worth_recording {
                    self.append(ConversationMessage::new(
                        fx_core::MessageRole::Tool,
                        vec![fx_core::MessageContent::ToolResult {
                            id: id.clone(),
                            content: trimmed.to_string(),
                            is_error,
                        }],
                    ));
                }
                self.set_phase(Phase::Running);
            }
            ProviderEvent::Approval(approval) => self.add_approval(PendingApproval {
                id: approval.id,
                tool_name: approval.tool_name,
                input: approval.input,
                risk: approval.risk.into(),
            }),
            ProviderEvent::UserInput(request) => {
                self.add_question(PendingQuestion {
                    id: request.id,
                    question: request
                        .questions
                        .first()
                        .map(|q| q.question.clone())
                        .unwrap_or_default(),
                    header: request
                        .questions
                        .first()
                        .map(|q| q.header.clone())
                        .unwrap_or_default(),
                    options: request
                        .questions
                        .first()
                        .map(|q| {
                            q.options
                                .iter()
                                .map(|o| QuestionOption {
                                    label: o.label.clone(),
                                    description: o.description.clone(),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    is_other: request.questions.first().is_some_and(|q| q.is_other),
                });
            }
            ProviderEvent::Usage(usage) => self.record_usage(usage),
            ProviderEvent::ContextUsage { used, window } => self.record_context(used, window),
            ProviderEvent::ModelChanged { model, reason } => {
                self.record_activity(format!("Switched to {model}"), false);
                if !reason.is_empty() {
                    self.record_activity(reason, true);
                }
            }
            ProviderEvent::Error(message) => self.set_error(message),
            ProviderEvent::Done { .. } => {
                // Commit, not clear. Clearing discards the turn's last prose, so
                // the final answer never reaches the transcript at all.
                self.commit_streaming();
                if self.phase != Phase::WaitingForApproval && self.phase != Phase::WaitingForInput {
                    self.set_phase(Phase::Idle);
                }
            }
        }
    }

    /// A monotonic token for "anything a view renders changed".
    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn touch(&mut self) {
        self.revision += 1;
    }

    /// Resolve the effective configuration for display.
    pub fn effective_access(&self, app_default: AgentAccess) -> AgentAccess {
        app_default
    }

    /// Whether this conversation is waiting on the user specifically.
    pub fn is_waiting_for_user(&self) -> bool {
        matches!(
            self.phase,
            Phase::WaitingForApproval | Phase::WaitingForInput
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_message(text: &str) -> ConversationMessage {
        ConversationMessage::text(MessageRole::User, text)
    }

    fn assistant_message(text: &str) -> ConversationMessage {
        ConversationMessage::text(MessageRole::Assistant, text)
    }

    #[test]
    fn a_new_conversation_is_empty_and_idle() {
        let c = Conversation::new(Uuid::nil());
        assert_eq!(c.message_count(), 0);
        assert_eq!(c.phase(), Phase::Idle);
        assert!(!c.is_busy());
        assert!(!c.is_streaming());
        assert!(c.error().is_none());
    }

    #[test]
    fn messages_accumulate_and_bump_the_revision() {
        let mut c = Conversation::new(Uuid::nil());
        let before = c.message_revision();
        c.append(user_message("hello"));
        assert_eq!(c.message_count(), 1);
        assert!(
            c.message_revision() > before,
            "a view could not tell it changed"
        );
    }

    #[test]
    fn the_transcript_is_capped_and_drops_the_oldest_first() {
        let mut c = Conversation::new(Uuid::nil());
        for i in 0..MAX_RETAINED_MESSAGES + 40 {
            c.append(user_message(&format!("m{i}")));
        }
        assert_eq!(c.message_count(), MAX_RETAINED_MESSAGES);
        // The oldest went; the newest stayed.
        let texts: Vec<String> = c.messages().map(|m| m.text_content()).collect();
        assert_eq!(
            texts.last().unwrap(),
            &format!("m{}", MAX_RETAINED_MESSAGES + 39)
        );
        assert!(
            !texts.iter().any(|t| t == "m0"),
            "the oldest message survived"
        );
    }

    #[test]
    fn full_native_history_survives_continuation_and_retains_unsent_work() {
        let mut conversation = Conversation::new(Uuid::nil());
        conversation.hold_unsent(UnsentPrompt {
            text: "Do not lose this".into(),
            attachment_count: 0,
            session_id: None,
        });
        conversation.replace_full_history(
            (0..900)
                .map(|index| user_message(&format!("m{index}")))
                .collect(),
        );
        conversation.append(assistant_message("New turn"));
        assert_eq!(conversation.message_count(), 901);
        assert_eq!(conversation.messages().next().unwrap().text_content(), "m0");
        assert_eq!(conversation.unsent().unwrap().text, "Do not lose this");
    }

    #[test]
    fn cancelling_refuses_to_be_overwritten() {
        // The load-bearing invariant: a stream that finishes after the user
        // pressed stop must not report success.
        let mut c = Conversation::new(Uuid::nil());
        c.set_phase(Phase::Cancelling);
        assert!(
            !c.set_phase(Phase::Running),
            "a late turn revived a cancelled one"
        );
    }

    #[test]
    fn a_new_turn_may_follow_a_settled_cancel() {
        let mut c = Conversation::new(Uuid::nil());
        c.set_phase(Phase::Cancelling);
        c.set_phase(Phase::Idle);
        assert!(
            c.set_phase(Phase::Running),
            "a new turn may follow a cancel"
        );
    }

    #[test]
    fn a_completion_while_waiting_on_the_user_does_not_clear_the_wait() {
        let mut c = Conversation::new(Uuid::nil());
        c.add_approval(PendingApproval {
            id: "a1".into(),
            tool_name: "shell".into(),
            input: serde_json::Value::Null,
            risk: RiskLevelSerde::Moderate,
        });
        c.apply(ProviderEvent::Done {
            stop_reason: "completed".into(),
        });
        assert_eq!(
            c.phase(),
            Phase::WaitingForApproval,
            "the user still has to answer"
        );
    }

    #[test]
    fn streaming_text_accumulates_and_clears() {
        let mut c = Conversation::new(Uuid::nil());
        c.apply(ProviderEvent::TextDelta("Hello ".into()));
        c.apply(ProviderEvent::TextDelta("world".into()));
        assert_eq!(c.streaming_text(), "Hello world");
        assert!(c.is_streaming());

        c.clear_streaming();
        assert_eq!(c.streaming_text(), "");
        assert!(!c.is_streaming());
    }

    #[test]
    fn beginning_a_new_turn_discards_the_previous_streams_leftovers() {
        let mut c = Conversation::new(Uuid::nil());
        c.append_streaming("half a reply");
        c.begin_streaming();
        assert_eq!(c.streaming_text(), "", "text leaked into the next turn");
    }

    #[test]
    fn a_completed_text_item_only_fills_in_when_nothing_streamed() {
        // A provider that does not stream still produces text.
        let mut c = Conversation::new(Uuid::nil());
        c.apply(ProviderEvent::Text("only here".into()));
        assert_eq!(c.streaming_text(), "only here");

        // One that streams must not have its text duplicated by a final item.
        let mut c = Conversation::new(Uuid::nil());
        c.append_streaming("streamed ");
        c.apply(ProviderEvent::Text("streamed ".into()));
        assert_eq!(c.streaming_text(), "streamed ");
    }

    #[test]
    fn a_replayed_tool_result_is_not_counted_twice() {
        let mut c = Conversation::new(Uuid::nil());
        assert!(c.claim_tool_id("t1"));
        assert!(!c.claim_tool_id("t1"), "a replayed id was claimed again");
        assert!(c.has_completed_tool("t1"));
    }

    #[test]
    fn the_tool_id_memory_is_bounded() {
        let mut c = Conversation::new(Uuid::nil());
        for i in 0..MAX_COMPLETED_TOOL_IDS + 100 {
            c.claim_tool_id(&format!("t{i}"));
        }
        // The oldest were evicted, so the newest is still known and the oldest
        // is not.
        assert!(c.has_completed_tool(&format!("t{}", MAX_COMPLETED_TOOL_IDS + 99)));
        assert!(!c.has_completed_tool("t0"));
    }

    #[test]
    fn the_same_approval_arriving_twice_stacks_once() {
        let mut c = Conversation::new(Uuid::nil());
        let approval = PendingApproval {
            id: "a1".into(),
            tool_name: "shell".into(),
            input: serde_json::Value::Null,
            risk: RiskLevelSerde::Dangerous,
        };
        c.add_approval(approval.clone());
        c.add_approval(approval);
        assert_eq!(c.pending_approvals().len(), 1);
        assert_eq!(c.phase(), Phase::WaitingForApproval);
    }

    #[test]
    fn the_final_prose_of_a_turn_is_kept() {
        // `Done` used to clear the streaming buffer rather than commit it, so
        // the turn's last — often only — sentence never reached the transcript.
        let mut c = Conversation::new(Uuid::nil());
        c.apply(ProviderEvent::Lifecycle(
            crate::provider::Lifecycle::TurnStarted,
        ));
        c.apply(ProviderEvent::TextDelta("The answer.".into()));
        assert_eq!(c.message_count(), 0, "deltas are not messages on their own");

        c.apply(ProviderEvent::Done {
            stop_reason: "end_turn".into(),
        });
        assert_eq!(
            c.message_count(),
            1,
            "the turn's text was discarded instead of committed"
        );
        assert_eq!(c.messages().next().unwrap().text_content(), "The answer.");
    }

    #[test]
    fn a_turn_that_ended_with_no_prose_adds_no_empty_message() {
        let mut c = Conversation::new(Uuid::nil());
        c.apply(ProviderEvent::Done {
            stop_reason: "end_turn".into(),
        });
        assert_eq!(c.message_count(), 0);
    }

    #[test]
    fn a_tool_call_becomes_a_transcript_row() {
        // The transcript carries tool calls, not just the activity strip. The
        // activity row is a spinner; this is the record.
        let mut c = Conversation::new(Uuid::nil());
        c.apply(ProviderEvent::ToolUse {
            id: "t1".into(),
            name: "Read".into(),
            input: "{\"path\":\"/tmp/x\"}".into(),
        });
        assert_eq!(c.message_count(), 1);
        let message = c.messages().next().unwrap();
        assert_eq!(message.role, fx_core::MessageRole::Assistant);
        assert!(matches!(
            message.content.as_slice(),
            [fx_core::MessageContent::ToolUse { id, name, .. }]
                if id == "t1" && name == "Read"
        ));
    }

    #[test]
    fn a_tool_result_becomes_a_tool_row_not_a_user_row() {
        // `Tool` role, not `User`. Easy to get wrong, and a `user` row here would
        // read as the user having said something they did not.
        let mut c = Conversation::new(Uuid::nil());
        c.apply(ProviderEvent::ToolResult {
            id: "t1".into(),
            content: "file contents".into(),
            is_error: false,
        });
        let message = c.messages().next().unwrap();
        assert_eq!(message.role, fx_core::MessageRole::Tool);
        assert!(matches!(
            message.content.as_slice(),
            [fx_core::MessageContent::ToolResult { content, .. }] if content == "file contents"
        ));
    }

    #[test]
    fn a_tool_call_splits_the_prose_around_it() {
        // This is the whole interleaving mechanism. Without committing the
        // streamed text, one turn would be a single message with the call buried
        // inside it and no way to render the prose in the right order.
        let mut c = Conversation::new(Uuid::nil());
        c.apply(ProviderEvent::Lifecycle(
            crate::provider::Lifecycle::TurnStarted,
        ));
        c.apply(ProviderEvent::TextDelta("Let me check.".into()));
        c.apply(ProviderEvent::ToolUse {
            id: "t1".into(),
            name: "Read".into(),
            input: "{}".into(),
        });
        c.apply(ProviderEvent::TextDelta("Found it.".into()));
        c.commit_streaming();

        let texts: Vec<String> = c
            .messages()
            .map(|m| m.text_content())
            .filter(|t| !t.is_empty())
            .collect();
        assert_eq!(
            texts,
            vec!["Let me check.".to_string(), "Found it.".to_string()]
        );
        assert_eq!(c.message_count(), 3, "prose, call, prose");
    }

    #[test]
    fn a_trivial_successful_result_is_not_recorded() {
        // The Swift drops these deliberately: one row per tool call would bury
        // the prose the user came to read.
        for content in ["Completed", "   Completed  ", ""] {
            let mut c = Conversation::new(Uuid::nil());
            c.apply(ProviderEvent::ToolResult {
                id: "t1".into(),
                content: content.into(),
                is_error: false,
            });
            assert_eq!(c.message_count(), 0, "{content:?} produced a row");
        }
    }

    #[test]
    fn a_failed_result_is_recorded_even_when_it_says_nothing() {
        // Silence from a failure is exactly what the user needs to see.
        let mut c = Conversation::new(Uuid::nil());
        c.apply(ProviderEvent::ToolResult {
            id: "t1".into(),
            content: "Completed".into(),
            is_error: true,
        });
        assert_eq!(c.message_count(), 1);
    }

    #[test]
    fn a_meaningful_result_is_recorded() {
        let mut c = Conversation::new(Uuid::nil());
        c.apply(ProviderEvent::ToolResult {
            id: "t1".into(),
            content: "12 lines".into(),
            is_error: false,
        });
        assert_eq!(c.message_count(), 1);
    }

    #[test]
    fn a_tool_id_repeating_does_not_lose_the_result() {
        // `claim_tool_id` is shared by both events, and the result carries the
        // *same* id as the call. Gating the append on it would reject every
        // legitimate first result.
        let mut c = Conversation::new(Uuid::nil());
        c.apply(ProviderEvent::ToolUse {
            id: "t1".into(),
            name: "Bash".into(),
            input: "{}".into(),
        });
        c.apply(ProviderEvent::ToolResult {
            id: "t1".into(),
            content: "done".into(),
            is_error: false,
        });
        assert_eq!(c.message_count(), 2, "the result row was dropped");
    }

    #[test]
    fn a_question_still_pending_keeps_the_conversation_waiting() {
        // Claude opens an approval and a question together. Reporting "running"
        // when the approval is answered hides the question: the composer opens
        // and the question can never be answered, so the turn is dead.
        let mut c = Conversation::new(Uuid::nil());
        c.add_approval(PendingApproval {
            id: "a1".into(),
            tool_name: "shell".into(),
            input: serde_json::Value::Null,
            risk: RiskLevelSerde::Moderate,
        });
        c.add_question(PendingQuestion {
            id: "q1".into(),
            question: "Which database?".into(),
            header: "Scope".into(),
            options: Vec::new(),
            is_other: true,
        });
        assert_eq!(c.pending_questions().len(), 1);

        c.remove_approval("a1");
        assert_eq!(
            c.phase(),
            Phase::WaitingForInput,
            "a pending question was hidden by the approval being answered"
        );
        assert_eq!(c.pending_questions().len(), 1, "the question was lost");
    }

    #[test]
    fn answering_the_last_approval_returns_to_running() {
        let mut c = Conversation::new(Uuid::nil());
        c.add_approval(PendingApproval {
            id: "a1".into(),
            tool_name: "shell".into(),
            input: serde_json::Value::Null,
            risk: RiskLevelSerde::Moderate,
        });
        assert!(c.remove_approval("a1").is_some());
        assert!(c.pending_approvals().is_empty());
        assert_eq!(c.phase(), Phase::Running);
    }

    #[test]
    fn removing_an_absent_approval_is_harmless() {
        let mut c = Conversation::new(Uuid::nil());
        assert!(c.remove_approval("nope").is_none());
    }

    #[test]
    fn a_question_keeps_only_the_first_prompts_choices() {
        // The tray shows one question at a time; the rest are answered in turn.
        let request = crate::provider::UserInputRequest {
            id: "r1".into(),
            questions: vec![
                crate::provider::Question {
                    id: "q1".into(),
                    header: "Scope".into(),
                    question: "Which files?".into(),
                    options: vec![
                        crate::provider::Option_ {
                            label: "All".into(),
                            description: String::new(),
                        },
                        crate::provider::Option_ {
                            label: "Tests".into(),
                            description: String::new(),
                        },
                    ],
                    is_other: false,
                    is_secret: false,
                },
                crate::provider::Question {
                    id: "q2".into(),
                    header: String::new(),
                    question: "And?".into(),
                    options: Vec::new(),
                    is_other: true,
                    is_secret: false,
                },
            ],
        };
        let mut c = Conversation::new(Uuid::nil());
        c.apply(ProviderEvent::UserInput(request));
        assert_eq!(c.pending_questions().len(), 1);
        assert_eq!(c.pending_questions()[0].question, "Which files?");
        assert_eq!(c.pending_questions()[0].options.len(), 2);
        assert!(c.is_waiting_for_user());
    }

    #[test]
    fn queued_prompts_are_oldest_first_and_preview_truncated() {
        let mut c = Conversation::new(Uuid::nil());
        c.enqueue(QueuedPrompt {
            id: Uuid::nil(),
            text: "first".into(),
            attachment_count: 0,
        });
        c.enqueue(QueuedPrompt {
            id: Uuid::nil(),
            text: "a".repeat(QUEUED_PROMPT_PREVIEW_CHARS + 30),
            attachment_count: 2,
        });
        let previews = c.queued_previews();
        assert_eq!(previews[0], "first");
        assert!(previews[1].ends_with("… (+2 image)"), "{}", previews[1]);
        assert!(
            previews[1].chars().count() < QUEUED_PROMPT_PREVIEW_CHARS + 40,
            "the preview was not truncated"
        );
    }

    #[test]
    fn only_three_queued_previews_are_shown() {
        let mut c = Conversation::new(Uuid::nil());
        for i in 0..6 {
            c.enqueue(QueuedPrompt {
                id: Uuid::nil(),
                text: format!("q{i}"),
                attachment_count: 0,
            });
        }
        assert_eq!(c.queued_count(), 6, "the queue itself is not truncated");
        assert_eq!(c.queued_previews().len(), VISIBLE_QUEUED_PROMPTS);
    }

    #[test]
    fn dequeuing_takes_the_oldest() {
        let mut c = Conversation::new(Uuid::nil());
        for i in 0..3 {
            c.enqueue(QueuedPrompt {
                id: Uuid::nil(),
                text: format!("q{i}"),
                attachment_count: 0,
            });
        }
        assert_eq!(c.dequeue().unwrap().text, "q0");
        assert_eq!(c.dequeue().unwrap().text, "q1");
    }

    #[test]
    fn removing_a_queued_prompt_by_position_is_bounds_checked() {
        let mut c = Conversation::new(Uuid::nil());
        c.enqueue(QueuedPrompt {
            id: Uuid::nil(),
            text: "q0".into(),
            attachment_count: 0,
        });
        assert!(c.remove_queued_at(5).is_none());
        assert_eq!(c.remove_queued_at(0).unwrap().text, "q0");
    }

    #[test]
    fn an_unsent_prompt_is_only_recovered_for_the_same_session() {
        // A retry must never be sent to a thread it was not written for.
        let mut c = Conversation::new(Uuid::nil());
        c.set_session_id("s1");
        c.hold_unsent(UnsentPrompt {
            text: "important".into(),
            attachment_count: 0,
            session_id: Some("s1".into()),
        });

        let recovered = c.take_unsent().expect("same session");
        assert_eq!(recovered.text, "important");
    }

    #[test]
    fn an_unsent_prompt_from_another_session_is_dropped() {
        let mut c = Conversation::new(Uuid::nil());
        c.set_session_id("s2");
        c.hold_unsent(UnsentPrompt {
            text: "for s1".into(),
            attachment_count: 0,
            session_id: Some("s1".into()),
        });
        assert!(
            c.take_unsent().is_none(),
            "a retry was offered against the wrong thread"
        );
        assert!(c.unsent().is_none());
    }

    #[test]
    fn a_refresh_cannot_erase_an_unsent_prompt() {
        let mut c = Conversation::new(Uuid::nil());
        c.hold_unsent(UnsentPrompt {
            text: "still typing".into(),
            attachment_count: 1,
            session_id: None,
        });
        c.replace_messages(vec![assistant_message("refreshed")]);
        assert!(c.unsent().is_some(), "a provider refresh erased the draft");
    }

    #[test]
    fn usage_accumulates_across_turns() {
        let mut c = Conversation::new(Uuid::nil());
        c.record_usage(Usage {
            input_tokens: 10,
            output_tokens: 5,
            cached_input_tokens: 2,
            reasoning_output_tokens: 1,
            total_tokens: 18,
        });
        c.record_usage(Usage {
            input_tokens: 20,
            output_tokens: 5,
            cached_input_tokens: 0,
            reasoning_output_tokens: 0,
            total_tokens: 25,
        });
        let u = c.usage();
        assert_eq!(u.input_tokens, 30);
        assert_eq!(u.output_tokens, 10);
        assert_eq!(u.total_tokens, 43);
    }

    #[test]
    fn context_fill_is_unknown_until_the_window_is_reported() {
        let mut c = Conversation::new(Uuid::nil());
        assert_eq!(c.usage().context_fraction(), None);
        c.record_context(50_000, 200_000);
        assert_eq!(c.usage().context_fraction(), Some(0.25));
    }

    #[test]
    fn context_fill_is_clamped() {
        let mut c = Conversation::new(Uuid::nil());
        c.record_context(500_000, 200_000);
        assert_eq!(c.usage().context_fraction(), Some(1.0));
    }

    #[test]
    fn a_session_activity_is_recorded_once() {
        let mut c = Conversation::new(Uuid::nil());
        c.record_activity("Context compacted", true);
        c.record_activity("Context compacted", true);
        assert_eq!(
            c.activities().len(),
            1,
            "a repeated session record stacked up"
        );
    }

    #[test]
    fn a_repeated_non_session_activity_is_kept() {
        // Per-turn notes are the point of the strip; only session-level records
        // are de-duplicated.
        let mut c = Conversation::new(Uuid::nil());
        c.record_activity("Switched to gpt-5.6-terra", false);
        c.record_activity("Switched to gpt-5.6-terra", false);
        assert_eq!(c.activities().len(), 2);
    }

    #[test]
    fn activities_are_capped() {
        let mut c = Conversation::new(Uuid::nil());
        for i in 0..MAX_RUNTIME_ACTIVITIES + 50 {
            c.record_activity(format!("a{i}"), false);
        }
        assert_eq!(c.activities().len(), MAX_RUNTIME_ACTIVITIES);
    }

    #[test]
    fn an_error_moves_to_failed_and_dismissing_returns_to_idle() {
        let mut c = Conversation::new(Uuid::nil());
        c.set_error("rate limited");
        assert_eq!(c.error(), Some("rate limited"));
        assert_eq!(c.phase(), Phase::Failed);
        c.dismiss_error();
        assert!(c.error().is_none());
        assert_eq!(c.phase(), Phase::Idle);
    }

    #[test]
    fn the_last_text_finds_the_most_recent_non_empty_message() {
        let mut c = Conversation::new(Uuid::nil());
        c.append(user_message("first"));
        c.append(assistant_message("answer"));
        c.append(user_message("   "));
        assert_eq!(c.last_text().as_deref(), Some("answer"));
    }

    #[test]
    fn the_last_user_prompt_only_looks_at_user_messages() {
        let mut c = Conversation::new(Uuid::nil());
        c.append(user_message("my question"));
        c.append(assistant_message("my answer"));
        assert_eq!(c.last_user_prompt().as_deref(), Some("my question"));
    }

    #[test]
    fn every_phase_has_a_human_readable_label() {
        for phase in [
            Phase::Idle,
            Phase::Starting,
            Phase::Running,
            Phase::WaitingForApproval,
            Phase::WaitingForInput,
            Phase::Compacting,
            Phase::Cancelling,
            Phase::Failed,
        ] {
            assert!(!phase.label().is_empty(), "{phase:?} has no label");
        }
    }
}
