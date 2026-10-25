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
use std::sync::Arc;
use std::time::{Duration, Instant};

use fx_core::{AgentAccess, ConversationMessage, MessageRole, Timestamp};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::provider::{ProviderEvent, RiskLevel, TaskStatus, TaskUpdate, Usage};

/// How many messages a conversation retains.
pub const MAX_RETAINED_MESSAGES: usize = 250;
/// How many background tasks one conversation shows.
pub const MAX_BACKGROUND_TASKS: usize = 64;
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

/// Work a turn started in the background: a subagent, or a command it ran
/// with `run_in_background`.
///
/// Kept for the latest turn that started any, running and finished alike, so
/// the transcript can show what came back and what did not. Live only: a task
/// belongs to the process that ran it, so nothing here is saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundTask {
    pub id: String,
    pub tool_use_id: Option<String>,
    pub description: String,
    /// What runs it, in the provider's words: `local_bash`, `local_agent`.
    pub kind: Option<String>,
    pub subagent_type: Option<String>,
    pub status: TaskStatus,
    pub summary: Option<String>,
    pub last_tool: Option<String>,
    pub tool_uses: Option<u64>,
    pub tokens: Option<u64>,
    /// How long the provider last said it had been running.
    pub reported_duration: Option<Duration>,
    /// When FlowX heard it start, which is what a running task's clock reads.
    pub started_at: Instant,
    pub finished_at: Option<Instant>,
}

impl BackgroundTask {
    fn new(update: &TaskUpdate, now: Instant) -> Self {
        Self {
            id: update.id.clone(),
            tool_use_id: None,
            description: String::new(),
            kind: None,
            subagent_type: None,
            status: TaskStatus::Running,
            summary: None,
            last_tool: None,
            tool_uses: None,
            tokens: None,
            reported_duration: None,
            started_at: now,
            finished_at: None,
        }
    }

    /// Fold one report in, keeping whatever it did not say.
    fn merge(&mut self, update: TaskUpdate, now: Instant) {
        if let Some(id) = update.tool_use_id {
            self.tool_use_id = Some(id);
        }
        if let Some(description) = update.description {
            self.description = description;
        }
        if let Some(kind) = update.kind {
            self.kind = Some(kind);
        }
        if let Some(subagent_type) = update.subagent_type {
            self.subagent_type = Some(subagent_type);
        }
        if let Some(summary) = update.summary {
            self.summary = Some(summary);
        }
        if let Some(tool) = update.last_tool {
            self.last_tool = Some(tool);
        }
        if let Some(count) = update.tool_uses {
            self.tool_uses = Some(count);
        }
        if let Some(tokens) = update.tokens {
            self.tokens = Some(tokens);
        }
        if let Some(duration) = update.duration_ms {
            self.reported_duration = Some(Duration::from_millis(duration));
        }
        // A finished task stays finished: a late progress frame is not a
        // second life.
        if self.status.is_running() && !update.status.is_running() {
            self.status = update.status;
            self.finished_at = Some(now);
        }
    }

    /// How long the task has run, or ran.
    ///
    /// A running task reads the clock. A finished one prefers the provider's
    /// own figure, which does not include the time its report took to arrive.
    pub fn elapsed(&self, now: Instant) -> Duration {
        let observed = self
            .finished_at
            .unwrap_or(now)
            .saturating_duration_since(self.started_at);
        match (self.finished_at, self.reported_duration) {
            (Some(_), Some(reported)) => reported,
            (None, Some(reported)) => observed.max(reported),
            _ => observed,
        }
    }
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
    /// Saturating, because every count here comes from a provider's payload,
    /// and an overflow in a debug build is a panic on the render path.
    fn add(&mut self, usage: Usage) {
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(usage.cached_input_tokens);
        self.reasoning_output_tokens = self
            .reasoning_output_tokens
            .saturating_add(usage.reasoning_output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(usage.total_tokens);
    }

    /// Take a provider's cumulative totals as they are.
    ///
    /// Codex reports the thread's running total on every update. Adding each
    /// update counted the same tokens again every time it reported.
    fn replace(&mut self, usage: Usage) {
        self.input_tokens = usage.input_tokens;
        self.output_tokens = usage.output_tokens;
        self.cached_input_tokens = usage.cached_input_tokens;
        self.reasoning_output_tokens = usage.reasoning_output_tokens;
        self.total_tokens = usage.total_tokens;
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
    /// Shared rather than owned, so a view can keep a pass's messages after
    /// the lock is released without copying them. A message is never changed
    /// once it is here; only appended, trimmed, or replaced wholesale.
    messages: VecDeque<Arc<ConversationMessage>>,
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
    /// Background work the latest turn started, oldest first.
    tasks: Vec<BackgroundTask>,
    /// Bumped whenever a task is added or reports anything.
    tasks_revision: u64,
    /// The provider's session, once known.
    session_id: Option<String>,
    /// When the current or most recent turn began, for its elapsed time.
    turn_started_at: Option<Timestamp>,
    /// When the turn last moved on to something new: a call, a result, or a
    /// reply starting to stream. A tool call's own start is its message's
    /// timestamp; this covers the stretches between calls.
    step_started_at: Option<Timestamp>,
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
        self.messages.iter().map(|message| &**message)
    }

    /// The messages as shared handles, for a reader that keeps them past the
    /// lock: a view that renders from a snapshot holds these instead of copying
    /// every tool output and image a turn carries.
    pub fn shared_messages(&self) -> impl ExactSizeIterator<Item = &Arc<ConversationMessage>> {
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
        self.messages.push_back(Arc::new(message));
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
        self.messages = messages.into_iter().map(Arc::new).collect();
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
            if !self.is_busy() && !matches!(phase, Phase::Idle | Phase::Failed) {
                self.start_turn_clock();
            }
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

    /// The queued prompt at this position, counting from the front.
    pub fn queued_at(&self, index: usize) -> Option<&QueuedPrompt> {
        self.queued.get(index)
    }

    /// Replace a queued prompt's text in place, keeping its place and its id.
    ///
    /// Trimmed, and refused when nothing is left: an edit that empties a
    /// queued prompt is a removal, which is a different control.
    pub fn update_queued(&mut self, id: Uuid, text: &str) -> bool {
        let text = text.trim();
        if text.is_empty() {
            return false;
        }
        let Some(queued) = self.queued.iter_mut().find(|queued| queued.id == id) else {
            return false;
        };
        if queued.text != text {
            queued.text = text.to_string();
            self.touch();
        }
        true
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

    /// Replace the totals with a provider's own running count.
    pub fn record_cumulative_usage(&mut self, usage: Usage) {
        self.usage.replace(usage);
        self.touch();
    }

    /// Record how full the context is. A window of zero means "not said":
    /// Claude's replies carry the fill but leave the window to `result`, and
    /// taking the zero replaced the known window, so the meter vanished on
    /// every reply.
    pub fn record_context(&mut self, used: u64, window: u64) {
        self.usage.current_context_tokens = used;
        if window > 0 {
            self.usage.reported_context_window = Some(window);
        }
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

    /// Forget the provider session, so the next turn opens a fresh one.
    ///
    /// The transcript stays: restarting a session that stopped answering is a
    /// statement about the provider's side of the thread, not about its history.
    pub fn clear_session_id(&mut self) {
        if self.session_id.take().is_some() {
            self.touch();
        }
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

    // ── Turn progress ───────────────────────────────────────────────────────

    /// When the current or most recent turn began.
    ///
    /// Set when the conversation goes from settled to busy, which is the same
    /// moment for every provider, and kept after the turn ends.
    pub fn turn_started_at(&self) -> Option<Timestamp> {
        self.turn_started_at
    }

    /// When the turn last moved on to something new.
    pub fn step_started_at(&self) -> Option<Timestamp> {
        self.step_started_at
    }

    fn start_turn_clock(&mut self) {
        let now = fx_core::models::now();
        self.turn_started_at = Some(now);
        self.step_started_at = Some(now);
    }

    /// Remember when the turn moved on, before the event is applied.
    fn note_step(&mut self, event: &ProviderEvent) {
        let moved_on = match event {
            ProviderEvent::ToolUse { .. } | ProviderEvent::ToolResult { .. } => true,
            // Only the first delta of a reply starts a step; the rest continue it.
            ProviderEvent::TextDelta(_) => self.streaming_text.is_empty(),
            _ => false,
        };
        if moved_on {
            self.step_started_at = Some(fx_core::models::now());
        }
    }

    /// Record a plan that arrived as a notification as the call it stands for.
    ///
    /// Codex's model calls `update_plan`, but the server reports it only as
    /// `turn/plan/updated`. Recording the call puts the plan in the transcript
    /// where it happened, exactly where Claude's `TodoWrite` already is, so the
    /// view reads one kind of plan and a finished turn keeps its own. A repeat
    /// of the plan the turn already holds adds nothing.
    fn record_plan(&mut self, plan: &fx_core::policy::TurnPlan) {
        let input = plan.to_tool_input();
        let unchanged = self
            .messages
            .iter()
            .rev()
            .take_while(|message| message.role != MessageRole::User)
            .flat_map(|message| message.content.iter().rev())
            .find_map(|block| match block {
                fx_core::MessageContent::ToolUse { name, input, .. }
                    if fx_core::policy::TurnPlan::is_plan_tool(name) =>
                {
                    Some(input)
                }
                _ => None,
            })
            .is_some_and(|latest| *latest == input);
        if unchanged {
            return;
        }
        self.append(ConversationMessage::new(
            MessageRole::Assistant,
            vec![fx_core::MessageContent::ToolUse {
                id: format!("plan-{}", Uuid::new_v4()),
                name: fx_core::policy::turn_plan::PLAN_TOOL_NAME.to_string(),
                input,
            }],
        ));
    }

    // ── Background tasks ────────────────────────────────────────────────────

    /// The latest turn's background tasks, oldest first.
    pub fn tasks(&self) -> &[BackgroundTask] {
        &self.tasks
    }

    /// Bumped whenever a task is added or reports anything, which is what a
    /// view keys its re-measuring on. A running task's clock does not move it.
    pub fn tasks_revision(&self) -> u64 {
        self.tasks_revision
    }

    pub fn has_running_tasks(&self) -> bool {
        self.tasks.iter().any(|task| task.status.is_running())
    }

    pub fn running_task_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|task| task.status.is_running())
            .count()
    }

    /// Fold in one report about a task.
    ///
    /// A task is added only by a report that it is running. An ending for a
    /// task never seen starting belongs to another process, and showing it
    /// would claim work this turn never did.
    pub fn apply_task(&mut self, update: TaskUpdate) {
        let now = Instant::now();
        match self.tasks.iter_mut().find(|task| task.id == update.id) {
            Some(task) => task.merge(update, now),
            None if update.status.is_running() && !update.id.is_empty() => {
                let mut task = BackgroundTask::new(&update, now);
                task.merge(update, now);
                self.tasks.push(task);
                while self.tasks.len() > MAX_BACKGROUND_TASKS {
                    // The oldest finished task goes first; a running one is
                    // only dropped when nothing else can be.
                    let index = self
                        .tasks
                        .iter()
                        .position(|task| !task.status.is_running())
                        .unwrap_or(0);
                    self.tasks.remove(index);
                }
            }
            None => return,
        }
        self.tasks_revision += 1;
        self.touch();
    }

    /// Mark every task still running as stopped, with the reason.
    ///
    /// For a turn that ended while they ran: stopped, failed, or given up on.
    /// Their process is gone, so a spinner left on them would never stop.
    pub fn stop_running_tasks(&mut self, reason: &str) {
        let now = Instant::now();
        let mut changed = false;
        for task in self
            .tasks
            .iter_mut()
            .filter(|task| task.status.is_running())
        {
            task.status = TaskStatus::Stopped;
            task.finished_at = Some(now);
            if !reason.is_empty() {
                task.summary = Some(reason.to_string());
            }
            changed = true;
        }
        if changed {
            self.tasks_revision += 1;
            self.touch();
        }
    }

    /// Forget the previous turn's tasks, as a new turn starts.
    pub fn clear_tasks(&mut self) {
        if !self.tasks.is_empty() {
            self.tasks.clear();
            self.tasks_revision += 1;
            self.touch();
        }
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
        self.note_step(&event);
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
            // The streamed message is whole. Committing here is what keeps two
            // consecutive messages from running together into one row.
            ProviderEvent::TextEnd => self.commit_streaming(),
            ProviderEvent::Text(text) => {
                // A message that did not stream is its own row. It used to fill
                // the buffer only when the buffer was empty, so the second of two
                // such messages was dropped.
                if !text.is_empty() {
                    self.commit_streaming();
                    self.append(ConversationMessage::text(
                        fx_core::MessageRole::Assistant,
                        text,
                    ));
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
                // Not claimed here: the claim is what tells a view the call has
                // finished, and claiming it as it starts made every running call
                // read as done. The result claims it, and gates nothing.
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
            ProviderEvent::PlanUpdated(plan) => self.record_plan(&plan),
            ProviderEvent::Usage(usage) => self.record_usage(usage),
            ProviderEvent::CumulativeUsage(usage) => self.record_cumulative_usage(usage),
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
                // A background task still running will answer as a turn of its
                // own, so the thread is still working rather than ready.
                if self.has_running_tasks() {
                    if !self.is_waiting_for_user() {
                        self.set_phase(Phase::Running);
                    }
                } else if self.phase != Phase::WaitingForApproval
                    && self.phase != Phase::WaitingForInput
                {
                    self.set_phase(Phase::Idle);
                }
            }
            ProviderEvent::Task(update) => self.apply_task(update),
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

    fn texts(c: &Conversation) -> Vec<String> {
        c.messages()
            .map(ConversationMessage::text_content)
            .collect()
    }

    #[test]
    fn consecutive_messages_stay_separate_rows_whether_or_not_they_streamed() {
        // Two streamed messages with no tool call between them used to merge
        // with no separator.
        let mut c = Conversation::new(Uuid::nil());
        c.apply(ProviderEvent::TextDelta("First.".into()));
        c.apply(ProviderEvent::TextEnd);
        c.apply(ProviderEvent::TextDelta("Second.".into()));
        c.apply(ProviderEvent::TextEnd);
        assert_eq!(texts(&c), ["First.", "Second."]);

        // And a second message that did not stream used to be dropped, because
        // the buffer still held the first.
        let mut c = Conversation::new(Uuid::nil());
        c.apply(ProviderEvent::TextDelta("First.".into()));
        c.apply(ProviderEvent::Text("Second.".into()));
        c.apply(ProviderEvent::Text("Third.".into()));
        c.apply(ProviderEvent::Text(String::new()));
        assert_eq!(texts(&c), ["First.", "Second.", "Third."]);
        assert!(!c.is_streaming());
    }

    #[test]
    fn a_reply_that_does_not_say_the_window_keeps_the_known_one() {
        let mut c = Conversation::new(Uuid::nil());
        c.apply(ProviderEvent::ContextUsage {
            used: 1_000,
            window: 200_000,
        });
        c.apply(ProviderEvent::ContextUsage {
            used: 2_000,
            window: 0,
        });
        assert_eq!(c.usage().current_context_tokens, 2_000);
        assert_eq!(c.usage().reported_context_window, Some(200_000));
        assert_eq!(c.usage().context_fraction(), Some(0.01));
    }

    #[test]
    fn cumulative_usage_replaces_and_turn_usage_saturates() {
        let mut c = Conversation::new(Uuid::nil());
        for total in [100, 150] {
            c.apply(ProviderEvent::CumulativeUsage(Usage {
                input_tokens: total,
                total_tokens: total,
                ..Usage::default()
            }));
        }
        assert_eq!(c.usage().total_tokens, 150, "a running total was added up");

        c.apply(ProviderEvent::Usage(Usage {
            input_tokens: u64::MAX,
            total_tokens: u64::MAX,
            ..Usage::default()
        }));
        assert_eq!(c.usage().input_tokens, u64::MAX);
        assert_eq!(c.usage().total_tokens, u64::MAX);
    }

    #[test]
    fn a_finished_compaction_leaves_the_compacting_phase() {
        let mut c = Conversation::new(Uuid::nil());
        c.apply(ProviderEvent::Lifecycle(
            crate::provider::Lifecycle::TurnStarted,
        ));
        c.apply(ProviderEvent::Lifecycle(
            crate::provider::Lifecycle::Compacting,
        ));
        assert_eq!(c.phase(), Phase::Compacting);
        c.apply(ProviderEvent::Lifecycle(
            crate::provider::Lifecycle::Compacted,
        ));
        assert_eq!(c.phase(), Phase::Running);
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
    fn editing_a_queued_prompt_keeps_its_place_and_refuses_an_empty_edit() {
        let mut c = Conversation::new(Uuid::nil());
        let ids: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
        for (index, id) in ids.iter().enumerate() {
            c.enqueue(QueuedPrompt {
                id: *id,
                text: format!("q{index}"),
                attachment_count: 0,
            });
        }
        let before = c.revision();
        assert!(c.update_queued(ids[1], "  rewritten  "));
        assert!(c.revision() > before, "an edit must repaint the strip");
        assert_eq!(c.queued_at(1).unwrap().text, "rewritten");
        assert_eq!(c.queued_position(ids[1]), Some(1));
        assert!(!c.update_queued(ids[1], " \n "));
        assert_eq!(c.queued_at(1).unwrap().text, "rewritten");
        assert!(!c.update_queued(Uuid::new_v4(), "nobody"));
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
    fn shared_messages_are_handed_out_without_copying() {
        // A view keeps these past the lock. If appending or reading moved them,
        // every pass would be back to copying the whole transcript.
        let mut c = Conversation::new(Uuid::nil());
        c.append(user_message("first"));
        let before: Vec<Arc<ConversationMessage>> = c.shared_messages().cloned().collect();
        c.append(assistant_message("second"));
        let after: Vec<Arc<ConversationMessage>> = c.shared_messages().cloned().collect();
        assert!(Arc::ptr_eq(&before[0], &after[0]));
        assert_eq!(after.len(), 2);
        assert_eq!(c.messages().len(), 2);
    }

    #[test]
    fn clearing_the_session_keeps_the_transcript_and_reports_the_change() {
        let mut c = Conversation::new(Uuid::nil());
        c.append(user_message("hello"));
        c.set_session_id("s1");
        let before = c.revision();
        c.clear_session_id();
        assert!(c.session_id().is_none());
        assert_eq!(c.message_count(), 1);
        assert!(c.revision() > before, "a view would not learn of the reset");
        let after = c.revision();
        c.clear_session_id();
        assert_eq!(c.revision(), after, "clearing nothing is not a change");
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
    fn a_call_is_finished_when_its_result_arrives_not_when_it_starts() {
        // A view reads the claim as "this call is done". Claiming the call as
        // it started made every running call look finished, so nothing could
        // say which one was still going.
        let mut c = Conversation::new(Uuid::nil());
        c.apply(ProviderEvent::ToolUse {
            id: "t1".into(),
            name: "Bash".into(),
            input: "{}".into(),
        });
        assert!(!c.has_completed_tool("t1"), "a running call read as done");
        // A result with nothing to say is not a row, but it does finish the call.
        c.apply(ProviderEvent::ToolResult {
            id: "t1".into(),
            content: TRIVIAL_TOOL_RESULT.into(),
            is_error: false,
        });
        assert!(c.has_completed_tool("t1"));
        assert_eq!(c.message_count(), 1);
    }

    #[test]
    fn a_turn_clock_starts_when_work_starts_and_survives_waiting_on_the_reader() {
        let mut c = Conversation::new(Uuid::nil());
        assert_eq!(c.turn_started_at(), None, "nothing has run yet");
        c.set_phase(Phase::Starting);
        let started = c.turn_started_at().expect("a busy turn has a start");
        assert_eq!(c.step_started_at(), Some(started));

        // Waiting on an approval is still the same turn.
        c.set_phase(Phase::WaitingForApproval);
        c.set_phase(Phase::Running);
        assert_eq!(c.turn_started_at(), Some(started));

        // The record outlives the turn, and the next turn starts its own.
        c.set_phase(Phase::Idle);
        assert_eq!(c.turn_started_at(), Some(started));
        std::thread::sleep(std::time::Duration::from_millis(2));
        c.apply(ProviderEvent::Lifecycle(
            crate::provider::Lifecycle::TurnStarted,
        ));
        assert!(c.turn_started_at().unwrap() > started);
    }

    #[test]
    fn the_step_clock_moves_on_calls_results_and_the_start_of_a_reply() {
        let mut c = Conversation::new(Uuid::nil());
        c.apply(ProviderEvent::Lifecycle(
            crate::provider::Lifecycle::TurnStarted,
        ));
        let mut last = c.step_started_at().unwrap();
        let mut moved = |c: &mut Conversation, event: ProviderEvent| {
            std::thread::sleep(std::time::Duration::from_millis(2));
            c.apply(event);
            let now = c.step_started_at().unwrap();
            let moved = now > last;
            last = now;
            moved
        };
        assert!(moved(
            &mut c,
            ProviderEvent::ToolUse {
                id: "t1".into(),
                name: "Bash".into(),
                input: "{}".into(),
            }
        ));
        assert!(moved(
            &mut c,
            ProviderEvent::ToolResult {
                id: "t1".into(),
                content: "Completed".into(),
                is_error: false,
            }
        ));
        assert!(moved(&mut c, ProviderEvent::TextDelta("Found".into())));
        assert!(
            !moved(&mut c, ProviderEvent::TextDelta(" it".into())),
            "a later delta continues the same reply"
        );
    }

    #[test]
    fn a_plan_update_is_recorded_as_the_call_it_stands_for_once() {
        use fx_core::policy::{PlanStatus, PlanStep, TurnPlan};
        let plan = |status| TurnPlan {
            explanation: None,
            steps: vec![PlanStep {
                text: "Fix the bug".into(),
                active_text: None,
                status,
            }],
        };
        let mut c = Conversation::new(Uuid::nil());
        c.append(user_message("fix it"));
        c.apply(ProviderEvent::PlanUpdated(plan(PlanStatus::InProgress)));
        c.apply(ProviderEvent::PlanUpdated(plan(PlanStatus::InProgress)));
        assert_eq!(c.message_count(), 2, "a repeated plan added a second row");
        c.apply(ProviderEvent::PlanUpdated(plan(PlanStatus::Completed)));
        assert_eq!(c.message_count(), 3);

        let recorded = c.messages().last().unwrap();
        let [fx_core::MessageContent::ToolUse { name, input, .. }] = recorded.content.as_slice()
        else {
            panic!("expected a tool call, got {recorded:?}");
        };
        assert_eq!(
            TurnPlan::from_tool_call(name, input),
            Some(plan(PlanStatus::Completed))
        );
        assert!(!c.is_streaming());
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

    fn task(id: &str, status: TaskStatus) -> TaskUpdate {
        TaskUpdate {
            id: id.into(),
            status,
            ..TaskUpdate::default()
        }
    }

    #[test]
    fn a_task_report_fills_in_only_what_it_says() {
        let mut c = Conversation::new(Uuid::nil());
        c.apply(ProviderEvent::Task(TaskUpdate {
            description: Some("review the transport".into()),
            subagent_type: Some("general-purpose".into()),
            ..task("t1", TaskStatus::Running)
        }));
        c.apply(ProviderEvent::Task(TaskUpdate {
            last_tool: Some("Grep".into()),
            tool_uses: Some(3),
            ..task("t1", TaskStatus::Running)
        }));
        let [task_seen] = c.tasks() else {
            panic!("{:?}", c.tasks())
        };
        assert_eq!(task_seen.description, "review the transport");
        assert_eq!(task_seen.subagent_type.as_deref(), Some("general-purpose"));
        assert_eq!(task_seen.last_tool.as_deref(), Some("Grep"));
        assert_eq!(task_seen.tool_uses, Some(3));
        assert!(c.has_running_tasks());

        let revision = c.tasks_revision();
        c.apply(ProviderEvent::Task(TaskUpdate {
            summary: Some("found two bugs".into()),
            duration_ms: Some(4_000),
            ..task("t1", TaskStatus::Completed)
        }));
        assert!(c.tasks_revision() > revision);
        assert_eq!(c.tasks()[0].status, TaskStatus::Completed);
        assert_eq!(c.tasks()[0].summary.as_deref(), Some("found two bugs"));
        assert_eq!(
            c.tasks()[0].elapsed(Instant::now()),
            Duration::from_secs(4),
            "a finished task reads the provider's own duration"
        );
        // Finished stays finished.
        c.apply(ProviderEvent::Task(task("t1", TaskStatus::Running)));
        assert!(!c.has_running_tasks());
    }

    #[test]
    fn a_task_never_seen_starting_is_not_shown() {
        let mut c = Conversation::new(Uuid::nil());
        c.apply(ProviderEvent::Task(task("lost", TaskStatus::Stopped)));
        assert!(c.tasks().is_empty());
    }

    #[test]
    fn a_result_with_a_task_running_leaves_the_thread_working() {
        // The thread read "Ready" while six reviews were still running.
        let mut c = Conversation::new(Uuid::nil());
        c.apply(ProviderEvent::Lifecycle(
            crate::provider::Lifecycle::TurnStarted,
        ));
        c.apply(ProviderEvent::Task(task("t1", TaskStatus::Running)));
        c.apply(ProviderEvent::TextDelta("started".into()));
        c.apply(ProviderEvent::Done {
            stop_reason: "end_turn".into(),
        });
        assert_eq!(c.phase(), Phase::Running);
        assert_eq!(c.last_text().as_deref(), Some("started"));

        c.apply(ProviderEvent::Task(task("t1", TaskStatus::Completed)));
        c.apply(ProviderEvent::Done {
            stop_reason: "end_turn".into(),
        });
        assert_eq!(c.phase(), Phase::Idle);
    }

    #[test]
    fn stopping_running_tasks_says_why_and_leaves_finished_ones_alone() {
        let mut c = Conversation::new(Uuid::nil());
        c.apply(ProviderEvent::Task(task("done", TaskStatus::Running)));
        c.apply(ProviderEvent::Task(TaskUpdate {
            summary: Some("ok".into()),
            ..task("done", TaskStatus::Completed)
        }));
        c.apply(ProviderEvent::Task(task("live", TaskStatus::Running)));
        c.stop_running_tasks("Stopped with the turn.");
        assert_eq!(c.running_task_count(), 0);
        assert_eq!(c.tasks()[0].summary.as_deref(), Some("ok"));
        assert_eq!(c.tasks()[1].status, TaskStatus::Stopped);
        assert_eq!(
            c.tasks()[1].summary.as_deref(),
            Some("Stopped with the turn.")
        );
        c.clear_tasks();
        assert!(c.tasks().is_empty());
    }
}
