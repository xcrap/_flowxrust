//! Claude Code's stream-JSON protocol.
//!
//! Where Codex speaks JSON-RPC — request ids, responses, correlated answers —
//! Claude writes one JSON object per line and mostly *narrates*. There is no id
//! to correlate: the stream is a sequence of events that must be read in order,
//! and the hard parts are all about state across those lines rather than within
//! one.
//!
//! Four pieces of that state carry the whole protocol:
//!
//! - **Text arrives twice.** Claude streams `content_block_delta` frames and
//!   then repeats the assembled text in a final `assistant` frame. Emitting both
//!   would double every message, so the parser remembers that it saw a delta and
//!   suppresses the echo.
//! - **Tool calls are reported twice for the same reason**, and the dedup is by
//!   tool-use id rather than by position.
//! - **`result` arrives once per model turn**, and a turn that reported it must
//!   ignore a second one rather than double-counting usage. A process can run
//!   several turns, though: a background task's notification and a steer each
//!   start one, opened by `system/init`, which is what re-arms the count.
//! - **Control requests block.** Claude waits for a `control_response` on every
//!   one, so an unrecognised subtype that is silently ignored hangs the turn
//!   forever. Anything unrecognised gets an explicit refusal instead.
//!
//! Background tasks are the fifth. `task_started`, `task_progress` and
//! `task_notification` are `system` frames, so a subagent's `parent_tool_use_id`
//! never drops them. Only tasks started in *this* process are reported: a
//! resumed session announces the tasks the previous process lost as `stopped`
//! notifications, and those are not work anybody is waiting for. Claude then
//! answers each such batch with an `init` and a zero-turn `result` whose origin
//! is `task-notification` — before it reads the prompt — and neither of those is
//! the turn the user asked for.
//!
//! Not every ending is answered. A task that completed or failed tells the
//! model, which answers as a turn of its own, unless a turn is running and
//! makes another request, which carries the notification instead. A stopped
//! task is not answered after the turn: Claude marks a shell or a monitor told
//! as it kills it, and an agent the turn stopped is told within that turn. One
//! a subagent owns tells that subagent. Waiting for an answer to any of those
//! held the input open with nothing left to come.

use std::collections::VecDeque;

use serde_json::{Value, json};

use crate::provider::{
    ApprovalRequest, Lifecycle, ProviderEvent, Question, RiskLevel, TaskStatus, TaskUpdate, Usage,
    UserInputRequest,
};

/// The most questions taken from one `AskUserQuestion` payload.
pub const MAX_QUESTIONS: usize = 4;
/// The most options any one question carries.
pub const MAX_OPTIONS: usize = 10;
/// The most parameters shown for one approval.
pub const MAX_PARAMETERS: usize = 20;
/// The most tool calls whose ids are remembered for deduplication.
pub const MAX_TRACKED_TOOL_IDS: usize = 512;
/// The most tasks one process tracks. Past this the oldest is forgotten, and a
/// notification for it is then ignored like one from a previous process.
pub const MAX_TRACKED_TASKS: usize = 256;
/// Reads Claude's stream, producing provider events.
#[derive(Debug, Default)]
pub struct ClaudeStreamParser {
    /// Provider-native identity reported by init/result, needed to resume.
    session_id: Option<String>,
    /// Set once a `text_delta` frame arrives, so the `assistant` echo is dropped.
    saw_text_delta: bool,
    /// Tool-use ids already reported, because they arrive twice.
    /// Tool ids already reported, oldest first.
    ///
    /// A `VecDeque` rather than a `HashSet` because the bound evicts *oldest*
    /// entries. A set that clears wholesale at the limit re-enables deduplication
    /// for everything it had already seen, so the `assistant` echo of an
    /// already-reported tool is emitted a second time — the exact bug the
    /// tracking exists to prevent.
    seen_tool_ids: VecDeque<String>,
    /// Set once `result` arrives, so a second one is ignored.
    did_finish: bool,
    /// How much of the context window the latest reply used, read from its
    /// `usage`. Reported as it arrives, and again with the window at `result`.
    context_tokens: Option<u64>,
    /// The process's running total from the last `result` with a
    /// `modelUsage`. That breakdown covers the whole process, so a follow-up
    /// turn's `result` repeats everything before it; only the difference is
    /// this turn's. Kept across [`Self::reset`], which is per turn.
    reported_model_usage: Option<Usage>,
    /// Set by the first frame that answers the prompt, which is the proof
    /// Claude read it. Thinking, usage and lifecycle frames count even though
    /// none of them is shown, because a prompt Claude has read must not be
    /// offered back for sending again.
    prompt_consumed: bool,
    /// Control requests waiting on this app's answer, keyed by the id the
    /// parser hands out.
    pending: Vec<PendingControl>,
    /// How many control requests this turn has produced, which makes every id
    /// unique for the turn.
    ///
    /// Numbered rather than derived from the pending count, so an id stays valid
    /// after a *different* request is answered: two Claude questions open at
    /// once must not both be called `claude-input-0`.
    sequence: usize,
    /// A refusal this parser could not avoid, written back by the session.
    ///
    /// Held rather than written directly because the parser has no process; the
    /// transport writes it.
    last_refusal: Option<Value>,
    /// Tasks this process started and has not heard the last of, oldest first.
    tasks: VecDeque<TrackedTask>,
    /// How many tasks this process has seen start, so the transport can tell
    /// a line that started one.
    tasks_started: u64,
    /// A background task's notification has arrived and Claude has not yet
    /// answered it. The answer is a turn of its own, and may ask for a
    /// permission, so the input stays open until it comes: closed at the
    /// `result` before it, the permission's answer could not be written.
    follow_up_owed: bool,
    /// Whether this CLI reports what happens to each message FlowX sends, as
    /// `command_lifecycle` frames. Every message carries an id, so the
    /// prompt's own frames say so before the turn's first request. Kept
    /// across turns: it is the CLI's, not the turn's.
    reports_reads: bool,
    /// A notification that is owed an answer arrived while a turn was
    /// running. The turn's next request carries it, and then it is answered
    /// there; if the turn makes none, it is answered as a turn of its own
    /// after this one's `result`. Owed outright, a notification the turn had
    /// already taken held the input open for a follow-up that never came.
    notified_in_turn: bool,
    /// How many of the user's messages Claude still had queued at the last
    /// `result`. Each is a turn of its own that follows without more input,
    /// and may ask for a permission.
    queued_turns: u64,
    /// This process has answered a prompt. A zero-turn acknowledgement after
    /// that is the end of a task this process started; before it, one of a
    /// task the last process lost.
    answered: bool,
    /// Whether `AskUserQuestion` is answered automatically.
    ///
    /// Off by default: a question is a decision for the user, and auto-answering
    /// it would have the agent ask itself things and guess.
    pub auto_approve: bool,
}

/// One task the parser has seen start.
#[derive(Debug, Clone)]
struct TrackedTask {
    /// What `task_started` said, so a call that moves to the background later
    /// can be reported with its description.
    started: TaskUpdate,
    /// Running behind the turn rather than inside the call that started it.
    /// Only these are reported or waited for: a foreground subagent is already
    /// its own tool row, and its call does not return until it is done.
    background: bool,
    /// Started by a subagent, which is told when it ends. The main session
    /// gives no answer for it.
    owned_by_subagent: bool,
    /// How it ended, once a terminal status arrived. The notification that
    /// follows still carries the summary, so the task is kept until it does.
    ended: Option<TaskStatus>,
}

impl TrackedTask {
    /// Running in the background, which is what holds the input open.
    fn is_running(&self) -> bool {
        self.background && self.ended.is_none()
    }

    /// Ended in a way Claude answers, with its notification still to come.
    ///
    /// The patch arrives a line before the notification. Read as the end of
    /// the wait, it closed the input at the patch, and the follow-up's
    /// permissions then had no way back to Claude.
    fn awaits_notification(&self) -> bool {
        self.background && self.ended.is_some_and(|status| self.answered_when(status))
    }

    /// Whether Claude answers an ending like this as a turn of its own.
    fn answered_when(&self, status: TaskStatus) -> bool {
        !self.owned_by_subagent && matches!(status, TaskStatus::Completed | TaskStatus::Failed)
    }
}

/// What a pending control is waiting for.
///
/// Readable so a session layer can decide what it can serve and refuse the rest,
/// rather than trying to answer a question as though it were an approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    /// A tool approval.
    Approval,
    /// Answers to questions.
    UserInput,
}

/// A control request Claude is blocked on.
///
/// Each carries the id the *app* handed out, which is what a later
/// `approve`/`answer_questions` call matches on. Claude's own request id is
/// carried too, because that is what a response has to quote back.
#[derive(Debug, Clone, PartialEq)]
pub enum PendingControl {
    /// Waiting for the user to allow or deny a tool.
    Approval {
        /// The id this app exposes.
        id: String,
        /// Claude's request id, which the response must quote.
        request_id: String,
        tool_use_id: String,
    },
    /// Waiting for answers to questions Claude asked.
    UserInput {
        id: String,
        request_id: String,
        tool_use_id: String,
        /// Claude's own question text, keyed by the ids handed to the app, so an
        /// answer can be mapped back to the question it belongs to.
        question_text_by_id: Vec<(String, String)>,
        /// The tool input Claude supplied, so `updatedInput` can echo it back.
        input: Value,
    },
}

impl PendingControl {
    /// The id this app exposed for this request.
    pub fn id(&self) -> &str {
        match self {
            Self::Approval { id, .. } | Self::UserInput { id, .. } => id,
        }
    }
}

impl ClaudeStreamParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read one line, producing whatever it implies.
    pub fn events(&mut self, line: &str) -> Vec<ProviderEvent> {
        let Ok(envelope) = serde_json::from_str::<Value>(line) else {
            // A line that is not JSON is not fatal. Claude's stderr carries
            // warnings on the same stream on some versions, and a turn in
            // progress must survive them.
            tracing::debug!("a Claude stream line was not JSON");
            return Vec::new();
        };
        let Some(kind) = envelope.get("type").and_then(Value::as_str) else {
            return Vec::new();
        };
        if let Some(id) = envelope
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            self.session_id = Some(id.to_owned());
        }

        if answers_prompt(kind, &envelope) {
            self.prompt_consumed = true;
        }
        // A subagent's narration carries the id of the tool call that started
        // it. Its text is not the reply and its tools are not the turn's. Let
        // through, its words filled the main answer and its calls were listed
        // as the turn's own; what it found comes back as that call's result.
        if matches!(kind, "stream_event" | "assistant" | "user")
            && envelope
                .get("parent_tool_use_id")
                .is_some_and(|parent| !parent.is_null())
        {
            return Vec::new();
        }

        match kind {
            "control_request" => self.control_request(&envelope),
            "system" => self.system(&envelope),
            "stream_event" => self.stream_event(&envelope),
            "assistant" => self.assistant(&envelope),
            "user" => self.user(&envelope),
            "result" => self.result(&envelope),
            "command_lifecycle" => self.command_lifecycle(&envelope),
            _ => Vec::new(),
        }
    }

    /// What happened to a message FlowX sent under its own id.
    ///
    /// `queued` when Claude reads it off stdin, `started` when it drains into
    /// a turn, then how that turn ended. Started is when a steer is read: at
    /// the next request, after the calls running when it was written, which
    /// is where Claude's history records it. Claude Code 2.1.283 sends these
    /// only for a message that carries a `uuid`.
    fn command_lifecycle(&mut self, envelope: &Value) -> Vec<ProviderEvent> {
        self.reports_reads = true;
        if envelope.get("state").and_then(Value::as_str) != Some("started") {
            return Vec::new();
        }
        non_empty(envelope.get("command_uuid"))
            .map(|id| ProviderEvent::InputTaken { id: Some(id) })
            .into_iter()
            .collect()
    }

    /// A control request: a tool Claude wants to run, or a question it asks.
    ///
    /// Returns an event *and* the refusal FlowX must write back, because the two
    /// have to stay in step — emitting the event without the refusal would leave
    /// Claude blocked forever.
    fn control_request(&mut self, envelope: &Value) -> Vec<ProviderEvent> {
        let Some(request_id) = non_empty(envelope.get("request_id")) else {
            return Vec::new();
        };
        let request = envelope.get("request").cloned().unwrap_or(Value::Null);

        let (Some(subtype), Some(tool_name)) = (
            non_empty(request.get("subtype")),
            non_empty(request.get("tool_name")),
        ) else {
            // Claude blocks on every control request. One this parser cannot
            // read must still be answered, or the turn hangs with no error.
            // `last_refusal` is what a session writes back.
            let subtype = non_empty(request.get("subtype")).unwrap_or_else(|| "unknown".into());
            self.last_refusal = Some(refuse_control(&request_id, &subtype));
            return Vec::new();
        };
        if subtype != "can_use_tool" {
            self.last_refusal = Some(refuse_control(&request_id, &subtype));
            return Vec::new();
        }

        let input = request.get("input").cloned().unwrap_or(json!({}));
        let tool_use_id = non_empty(request.get("tool_use_id"))
            .unwrap_or_else(|| format!("claude-tool-{request_id}"));

        if tool_name == "AskUserQuestion"
            && let Some(raw) = input.get("questions").and_then(Value::as_array)
        {
            let questions = decode_questions(raw);
            if questions.is_empty() {
                // Nothing decodable, but Claude is still blocked. The event
                // alone is not enough — a question left unanswered stalls the
                // turn with no error anywhere — so the refusal goes to the
                // transport to be written back.
                self.last_refusal = Some(control_response(
                    &request_id,
                    &permission_decision(
                        false,
                        &tool_use_id,
                        None,
                        "FlowX could not decode Claude's question payload.",
                    ),
                ));
                return vec![ProviderEvent::ToolResult {
                    id: tool_use_id,
                    content: "FlowX could not decode Claude's question payload.".into(),
                    is_error: true,
                }];
            }
            let question_text_by_id = questions
                .iter()
                .map(|question| (question.id.clone(), question.question.clone()))
                .collect();
            let id = self.next_id("input");
            self.pending.push(PendingControl::UserInput {
                id: id.clone(),
                request_id: request_id.clone(),
                tool_use_id,
                question_text_by_id,
                input: input.clone(),
            });
            return vec![ProviderEvent::UserInput(UserInputRequest { id, questions })];
        }

        if self.auto_approve {
            // Nothing to ask the user; the refusal and the approval are recorded
            // together so the response can be written immediately.
            let id = self.next_id("approval");
            self.pending.push(PendingControl::Approval {
                id: id.clone(),
                request_id: request_id.clone(),
                tool_use_id: tool_use_id.clone(),
            });
            return Vec::new();
        }

        let id = self.next_id("approval");
        self.pending.push(PendingControl::Approval {
            id: id.clone(),
            request_id: request_id.clone(),
            tool_use_id,
        });
        vec![ProviderEvent::Approval(ApprovalRequest {
            id,
            tool_name: tool_name.clone(),
            input: input.clone(),
            risk: risk_for(&tool_name, &input),
        })]
    }

    /// Lifecycle, session-init and task frames.
    fn system(&mut self, envelope: &Value) -> Vec<ProviderEvent> {
        let subtype = envelope.get("subtype").and_then(Value::as_str);
        match subtype {
            // No subtype is the older spelling of `init`.
            None | Some("init") => {
                // Every model turn opens with `init`, the follow-up a task's
                // notification starts and a steer included. The turn before
                // has reported its `result`, so this one's must be counted,
                // and its text has not streamed yet.
                self.did_finish = false;
                self.saw_text_delta = false;
                vec![ProviderEvent::Lifecycle(Lifecycle::TurnStarted)]
            }
            Some("status")
                if envelope.get("status").and_then(Value::as_str) == Some("compacting") =>
            {
                vec![ProviderEvent::Lifecycle(Lifecycle::Compacting)]
            }
            // A request is going out. Claude gathers what it has queued just
            // before, so a notification that arrived earlier in the turn goes
            // with it and is answered in this turn, and a steer written since
            // the last request is read here. A CLI that names each message it
            // takes says which; for one that does not, this is the best there
            // is.
            Some("status")
                if envelope.get("status").and_then(Value::as_str) == Some("requesting") =>
            {
                self.notified_in_turn = false;
                if self.reports_reads {
                    Vec::new()
                } else {
                    vec![ProviderEvent::InputTaken { id: None }]
                }
            }
            Some("compact_boundary") => vec![ProviderEvent::Lifecycle(Lifecycle::Compacted)],
            Some("task_started") => self.task_started(envelope),
            Some("task_progress") => self.task_progress(envelope),
            Some("task_updated") => self.task_updated(envelope),
            Some("task_notification") => self.task_notification(envelope),
            _ => Vec::new(),
        }
    }

    /// A task began. Ambient work — memory consolidation, a passive monitor —
    /// is the CLI's own business and nobody waits on it.
    fn task_started(&mut self, envelope: &Value) -> Vec<ProviderEvent> {
        let Some(id) = non_empty(envelope.get("task_id")) else {
            return Vec::new();
        };
        if is_ambient(envelope) {
            return Vec::new();
        }
        let started = TaskUpdate {
            id: id.clone(),
            tool_use_id: non_empty(envelope.get("tool_use_id")),
            description: non_empty(envelope.get("description")).map(|text| bounded(&text, 512)),
            kind: non_empty(envelope.get("task_type")),
            subagent_type: non_empty(envelope.get("subagent_type")),
            status: TaskStatus::Running,
            ..TaskUpdate::default()
        };
        // Only an explicit `false` is a foreground call. Remote agents,
        // workflows and monitors do not carry the flag at all, and they run
        // behind the turn by nature.
        let background = envelope.get("is_backgrounded").and_then(Value::as_bool) != Some(false);
        self.tasks.retain(|task| task.started.id != id);
        self.tasks.push_back(TrackedTask {
            started: started.clone(),
            background,
            owned_by_subagent: envelope.get("owned_by_subagent").and_then(Value::as_bool)
                == Some(true),
            ended: None,
        });
        self.tasks_started += 1;
        while self.tasks.len() > MAX_TRACKED_TASKS {
            self.tasks.pop_front();
        }
        if background {
            vec![ProviderEvent::Task(started)]
        } else {
            Vec::new()
        }
    }

    /// Work a running task has done so far.
    fn task_progress(&mut self, envelope: &Value) -> Vec<ProviderEvent> {
        let Some(task) = self.tracked(envelope) else {
            return Vec::new();
        };
        if !task.is_running() {
            return Vec::new();
        }
        let mut update = self.task_update(envelope, TaskStatus::Running);
        update.last_tool = non_empty(envelope.get("last_tool_name"));
        vec![ProviderEvent::Task(update)]
    }

    /// A change to a task's record: moved to the background, or ended. The
    /// notification usually follows at once, but the patch is the earlier word
    /// and the one a stopped task may be left with.
    fn task_updated(&mut self, envelope: &Value) -> Vec<ProviderEvent> {
        let Some(id) = non_empty(envelope.get("task_id")) else {
            return Vec::new();
        };
        let Some(patch) = envelope.get("patch") else {
            return Vec::new();
        };
        let Some(task) = self.tasks.iter_mut().find(|task| task.started.id == id) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        if !task.background && patch.get("is_backgrounded").and_then(Value::as_bool) == Some(true) {
            task.background = true;
            events.push(ProviderEvent::Task(task.started.clone()));
        }
        if let Some(status) = patch
            .get("status")
            .and_then(Value::as_str)
            .filter(|status| *status != "running" && *status != "pending")
            && task.ended.is_none()
        {
            let status = TaskStatus::from_notification(status);
            task.ended = Some(status);
            if task.background {
                events.push(ProviderEvent::Task(TaskUpdate {
                    id,
                    status,
                    ..TaskUpdate::default()
                }));
            }
        }
        events
    }

    /// How a task ended, with its summary. The last word about it.
    fn task_notification(&mut self, envelope: &Value) -> Vec<ProviderEvent> {
        let Some(task) = self.tracked(envelope) else {
            // Not started in this process: a resumed session reports the tasks
            // the last one lost, and nothing here is waiting for them.
            return Vec::new();
        };
        let status = envelope
            .get("status")
            .and_then(Value::as_str)
            .map(TaskStatus::from_notification)
            .unwrap_or(TaskStatus::Stopped);
        let background = task.background;
        let answered = task.answered_when(status);
        let id = task.started.id.clone();
        self.tasks.retain(|task| task.started.id != id);
        if !background {
            return Vec::new();
        }
        // Owed only for an ending Claude tells the model about. Between turns
        // the answer is a turn of its own; during one, it is that turn's
        // unless the turn makes no further request.
        if answered {
            if self.did_finish {
                self.follow_up_owed = true;
            } else {
                self.notified_in_turn = true;
            }
        }
        let mut update = self.task_update(envelope, status);
        // A failure may explain itself only in `reason`.
        if update.summary.is_none() {
            update.summary = non_empty(envelope.get("reason")).map(|text| bounded(&text, 2_048));
        }
        vec![ProviderEvent::Task(update)]
    }

    /// The task a frame names, if this process started it.
    fn tracked(&self, envelope: &Value) -> Option<&TrackedTask> {
        let id = non_empty(envelope.get("task_id"))?;
        self.tasks.iter().find(|task| task.started.id == id)
    }

    /// The fields progress and notification frames share.
    fn task_update(&self, envelope: &Value, status: TaskStatus) -> TaskUpdate {
        let usage = envelope.get("usage");
        TaskUpdate {
            id: non_empty(envelope.get("task_id")).unwrap_or_default(),
            tool_use_id: non_empty(envelope.get("tool_use_id")),
            description: non_empty(envelope.get("description")).map(|text| bounded(&text, 512)),
            subagent_type: non_empty(envelope.get("subagent_type")),
            status,
            summary: non_empty(envelope.get("summary")).map(|text| bounded(&text, 2_048)),
            tool_uses: usage.and_then(|usage| counted(usage.get("tool_uses"))),
            tokens: usage.and_then(|usage| counted(usage.get("total_tokens"))),
            duration_ms: usage.and_then(|usage| counted(usage.get("duration_ms"))),
            ..TaskUpdate::default()
        }
    }

    /// Whether a background task this process started is still running.
    ///
    /// This is what keeps stdin open past a `result`: Claude reports the task's
    /// end as a new turn, and only an open input can still be steered.
    pub fn has_running_tasks(&self) -> bool {
        self.tasks.iter().any(TrackedTask::is_running)
    }

    /// Whether the input has to stay open past a `result`: a background task
    /// is running, one has ended and Claude still owes its answer, or Claude
    /// holds a message of the user's it has yet to answer. Each of those is a
    /// turn still to come, and its permissions are answered through the input.
    pub fn holds_input_open(&self) -> bool {
        self.has_running_tasks()
            || self.tasks.iter().any(TrackedTask::awaits_notification)
            || self.follow_up_owed
            || self.queued_turns > 0
    }

    /// How many tasks this process has seen start. A change is a line that
    /// started one, which is when its process can first be found.
    pub fn tasks_started(&self) -> u64 {
        self.tasks_started
    }

    /// A streamed frame. Only text deltas are meaningful here.
    fn stream_event(&mut self, envelope: &Value) -> Vec<ProviderEvent> {
        let Some(event) = envelope.get("event") else {
            return Vec::new();
        };
        if event.get("type").and_then(Value::as_str) != Some("content_block_delta") {
            return Vec::new();
        }
        let Some(delta) = event.get("delta") else {
            return Vec::new();
        };
        if delta.get("type").and_then(Value::as_str) != Some("text_delta") {
            return Vec::new();
        }
        let Some(text) = delta.get("text").and_then(Value::as_str) else {
            return Vec::new();
        };
        // Recorded before returning, because the matching `assistant` frame is
        // already on its way and will arrive in the same stream.
        self.saw_text_delta = true;
        vec![ProviderEvent::TextDelta(text.to_string())]
    }

    /// The assembled assistant message.
    ///
    /// Its text is dropped when a streamed version was already seen, which is the
    /// common case; a non-streaming run has no deltas and keeps it.
    fn assistant(&mut self, envelope: &Value) -> Vec<ProviderEvent> {
        let Some(blocks) = envelope
            .pointer("/message/content")
            .and_then(Value::as_array)
        else {
            return Vec::new();
        };
        let mut events = Vec::new();
        // Each reply says how large the conversation it answered was. Without
        // this, Claude never reported context at all, and its threads' meter
        // could only ever say it did not know. The window is not known until
        // `result`; zero says so, and the thread's model fills it in.
        if let Some(used) = envelope
            .pointer("/message/usage")
            .and_then(super::native::context_usage)
            && self.context_tokens != Some(used)
        {
            self.context_tokens = Some(used);
            events.push(ProviderEvent::ContextUsage { used, window: 0 });
        }
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") if !self.saw_text_delta => {
                    if let Some(text) = block.get("text").and_then(Value::as_str)
                        && !text.is_empty()
                    {
                        events.push(ProviderEvent::Text(text.to_string()));
                    }
                }
                Some("tool_use") => {
                    let id = non_empty(block.get("id"))
                        .unwrap_or_else(|| format!("claude-tool-{}", self.sequence));
                    // Claude reports a call in both the stream and the assembled
                    // message; the id is the only thing that tells them apart.
                    if !self.remember_tool(&id) {
                        continue;
                    }
                    events.push(ProviderEvent::ToolUse {
                        id,
                        name: block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("Tool")
                            .to_string(),
                        input: json_string(block.get("input")).unwrap_or_else(|| "{}".into()),
                    });
                }
                _ => {}
            }
        }
        events
    }

    /// Tool results, echoed back after Claude ran them.
    fn user(&mut self, envelope: &Value) -> Vec<ProviderEvent> {
        let Some(blocks) = envelope
            .pointer("/message/content")
            .and_then(Value::as_array)
        else {
            return Vec::new();
        };
        blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
            .map(|block| ProviderEvent::ToolResult {
                id: block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                content: content_string(block.get("content")),
                is_error: block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
            .collect()
    }

    /// The end of a turn: usage, then completion.
    fn result(&mut self, envelope: &Value) -> Vec<ProviderEvent> {
        if answers_notification(envelope) {
            self.follow_up_owed = false;
        }
        // A notification this turn never sent on is answered by the next.
        if std::mem::take(&mut self.notified_in_turn) {
            self.follow_up_owed = true;
        }
        // Every `result` says how many of the user's messages are still
        // queued, so the latest one is the count. Absent on older versions,
        // which is none.
        self.queued_turns = envelope
            .get("queued_turn_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if acknowledges_notification(envelope) {
            // Nothing was asked and nothing answered. Before this process has
            // answered anything, that is a task the last process lost, and the
            // turn the user is waiting for has not started yet. After, it is
            // the end of a task this one started: with nothing left running it
            // is the end of the wait, and staying silent left the input open
            // and the turn spinning until Stop.
            if !self.answered || self.has_running_tasks() {
                return Vec::new();
            }
            self.did_finish = true;
            return vec![ProviderEvent::Done {
                stop_reason: envelope
                    .get("subtype")
                    .and_then(Value::as_str)
                    .unwrap_or("end_turn")
                    .to_string(),
            }];
        }
        // A second `result` would double-count a turn's cost.
        if self.did_finish {
            return Vec::new();
        }
        self.did_finish = true;
        self.answered = true;

        if envelope.get("is_error").and_then(Value::as_bool) == Some(true) {
            return vec![ProviderEvent::Error(
                envelope
                    .get("result")
                    .and_then(Value::as_str)
                    .unwrap_or("Claude Code returned an error.")
                    .to_string(),
            )];
        }

        let usage = match model_usage_of(envelope) {
            Some(total) => {
                let previous = self.reported_model_usage.replace(total);
                since(total, previous)
            }
            None => usage_of(envelope),
        };
        let mut events = Vec::new();
        if let (Some(used), Some(window)) = (self.context_tokens, context_window_of(envelope)) {
            events.push(ProviderEvent::ContextUsage { used, window });
        }
        events.extend([
            ProviderEvent::Usage(usage),
            ProviderEvent::Done {
                stop_reason: envelope
                    .get("subtype")
                    .and_then(Value::as_str)
                    .unwrap_or("end_turn")
                    .to_string(),
            },
        ]);
        events
    }

    /// Whether this turn has reported completion.
    pub fn did_finish(&self) -> bool {
        self.did_finish
    }

    /// Whether Claude has shown it read the prompt.
    pub fn prompt_consumed(&self) -> bool {
        self.prompt_consumed
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// The next id for a control request.
    fn next_id(&mut self, kind: &str) -> String {
        self.sequence += 1;
        format!("claude-{kind}-{}", self.sequence)
    }

    /// Forget per-turn state, keeping the configured policy.
    ///
    /// Called between turns. Carrying `saw_text_delta` into the next turn would
    /// swallow that turn's only text.
    pub fn reset(&mut self) {
        self.saw_text_delta = false;
        self.seen_tool_ids.clear();
        self.did_finish = false;
        self.prompt_consumed = false;
        self.pending.clear();
        self.sequence = 0;
        self.last_refusal = None;
        self.tasks.clear();
        self.follow_up_owed = false;
        self.notified_in_turn = false;
        self.queued_turns = 0;
    }

    /// Take a refusal produced by the last line, if any.
    ///
    /// Taken rather than peeked so a caller draining lines cannot miss one: an
    /// unclaimed refusal is a turn that hangs.
    pub fn take_refusal(&mut self) -> Option<Value> {
        self.last_refusal.take()
    }

    /// Answer a control request, returning the message to write back.
    ///
    /// Returns `None` for an id this parser does not hold, which happens when a
    /// second answer arrives for the same question.
    pub fn approve(&mut self, id: &str, approved: bool) -> Option<Value> {
        // The kind is checked *before* anything is removed. Removing first would
        // consume a question when handed its id, and Claude would then wait
        // forever on a reply that can never be given.
        let is_approval = self.pending.iter().any(|pending| {
            pending_id(pending) == id && matches!(pending, PendingControl::Approval { .. })
        });
        if !is_approval {
            return None;
        }
        let index = self
            .pending
            .iter()
            .position(|pending| pending_id(pending) == id)?;
        let pending = self.pending.remove(index);
        let PendingControl::Approval {
            request_id,
            tool_use_id,
            ..
        } = pending
        else {
            return None;
        };
        Some(control_response(
            &request_id,
            &permission_decision(approved, &tool_use_id, None, ""),
        ))
    }

    /// Answer Claude's questions.
    ///
    /// `answers` pairs the app's question ids with the chosen labels, which is
    /// what lets a multi-select or a typed answer be mapped back to Claude's own
    /// question text rather than to a positional index.
    pub fn answer_questions(&mut self, id: &str, answers: &[(String, String)]) -> Option<Value> {
        let is_question = self.pending.iter().any(|pending| {
            pending_id(pending) == id && matches!(pending, PendingControl::UserInput { .. })
        });
        if !is_question {
            return None;
        }
        let index = self
            .pending
            .iter()
            .position(|pending| pending_id(pending) == id)?;
        let pending = self.pending.remove(index);
        let PendingControl::UserInput {
            request_id,
            tool_use_id,
            question_text_by_id,
            input,
            ..
        } = pending
        else {
            return None;
        };

        let mut updated = input.clone();
        if let Some(object) = updated.as_object_mut() {
            let responses: Vec<Value> = question_text_by_id
                .iter()
                .map(|(question_id, question)| {
                    let answer = answers
                        .iter()
                        .find(|(id, _)| id == question_id)
                        .map(|(_, answer)| answer.clone())
                        .unwrap_or_default();
                    json!({ "question": question, "answer": answer })
                })
                .collect();
            object.insert("answers".into(), Value::Array(responses));
        }

        Some(control_response(
            &request_id,
            &permission_decision(true, &tool_use_id, Some(&updated), ""),
        ))
    }

    /// Decline a question, which is how a turn that cannot be answered ends.
    pub fn decline_questions(&mut self, id: &str) -> Option<Value> {
        let is_question = self.pending.iter().any(|pending| {
            pending_id(pending) == id && matches!(pending, PendingControl::UserInput { .. })
        });
        if !is_question {
            return None;
        }
        let index = self
            .pending
            .iter()
            .position(|pending| pending_id(pending) == id)?;
        let pending = self.pending.remove(index);
        let PendingControl::UserInput {
            request_id,
            tool_use_id,
            ..
        } = pending
        else {
            return None;
        };
        Some(control_response(
            &request_id,
            &permission_decision(
                false,
                &tool_use_id,
                None,
                "FlowX has no way to show this question.",
            ),
        ))
    }

    /// What one pending control is, or `None` when the id is not held.
    pub fn kind_of(&self, id: &str) -> Option<PendingKind> {
        self.pending.iter().find_map(|pending| match pending {
            PendingControl::Approval { id: held, .. } if held == id => Some(PendingKind::Approval),
            PendingControl::UserInput { id: held, .. } if held == id => {
                Some(PendingKind::UserInput)
            }
            _ => None,
        })
    }

    /// Every pending control with its kind, in the order Claude asked.
    pub fn pending_kinds(&self) -> Vec<(String, PendingKind)> {
        self.pending
            .iter()
            .map(|pending| {
                let kind = match pending {
                    PendingControl::Approval { .. } => PendingKind::Approval,
                    PendingControl::UserInput { .. } => PendingKind::UserInput,
                };
                (pending.id().to_string(), kind)
            })
            .collect()
    }

    /// Control requests still waiting, for a caller that needs to show them.
    pub fn pending_ids(&self) -> Vec<String> {
        self.pending
            .iter()
            .map(pending_id)
            .map(str::to_string)
            .collect()
    }

    /// Whether this turn saw a streamed text delta.
    pub fn saw_text_delta(&self) -> bool {
        self.saw_text_delta
    }

    /// Remember a tool id, bounded.
    ///
    /// `false` means it was already reported. The set is cleared rather than
    /// grown past its bound, because a turn long enough to hit the bound has
    /// already had every call the user saw.
    /// Record a tool id, reporting whether it is new.
    ///
    /// Bounded by dropping the oldest. That is safe because the echo this guards
    /// against arrives immediately after the streamed report, so an id only has
    /// to be remembered for a short window — not for the whole turn. Evicting the
    /// oldest keeps that window bounded without ever re-enabling deduplication
    /// for the ids that are still in it.
    fn remember_tool(&mut self, id: &str) -> bool {
        if self.seen_tool_ids.iter().any(|seen| seen == id) {
            return false;
        }
        self.seen_tool_ids.push_back(id.to_string());
        while self.seen_tool_ids.len() > MAX_TRACKED_TOOL_IDS {
            self.seen_tool_ids.pop_front();
        }
        true
    }
}

fn pending_id(pending: &PendingControl) -> &str {
    pending.id()
}

/// Decode Claude's `AskUserQuestion` payload.
fn decode_questions(raw: &[Value]) -> Vec<Question> {
    raw.iter()
        .take(MAX_QUESTIONS)
        .enumerate()
        .filter_map(|(index, question)| {
            let text = non_empty(question.get("question"))?;
            Some(Question {
                id: format!("question-{}", index + 1),
                header: bounded(
                    &non_empty(question.get("header")).unwrap_or_else(|| "Question".into()),
                    256,
                ),
                question: bounded(&text, 8_192),
                options: question
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|options| {
                        options
                            .iter()
                            .take(MAX_OPTIONS)
                            .filter_map(|option| {
                                let label = non_empty(option.get("label"))?;
                                Some(crate::provider::Option_ {
                                    label: bounded(&label, 256),
                                    description: bounded(
                                        &non_empty(option.get("description")).unwrap_or_default(),
                                        2_048,
                                    ),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                is_other: true,
                is_secret: false,
            })
        })
        .collect()
}

/// The per-model breakdown's total, when Claude provides one.
///
/// Preferred over `usage`, which reports only the last model when a turn used
/// more than one. It is the process's running total, not the turn's: see
/// [`since`].
fn model_usage_of(envelope: &Value) -> Option<Usage> {
    let models = envelope
        .get("modelUsage")
        .and_then(Value::as_object)
        .filter(|models| !models.is_empty())?;
    Some(
        models
            .iter()
            // Saturating: the counts are the provider's, and an overflow in a
            // debug build is a panic in the middle of a turn.
            .fold(Usage::default(), |mut total, (_, model)| {
                let input = count(model.get("inputTokens"));
                let output = count(model.get("outputTokens"));
                total.input_tokens = total.input_tokens.saturating_add(input);
                total.output_tokens = total.output_tokens.saturating_add(output);
                total.cached_input_tokens = total
                    .cached_input_tokens
                    .saturating_add(count(model.get("cacheReadInputTokens")));
                total.total_tokens = total
                    .total_tokens
                    .saturating_add(input.saturating_add(output));
                total
            }),
    )
}

/// This turn's share of a running total, given the total last reported.
///
/// A total smaller than the last one in any count is not a continuation of
/// it, so it is taken whole rather than read as a negative turn.
fn since(total: Usage, previous: Option<Usage>) -> Usage {
    let Some(previous) = previous else {
        return total;
    };
    let counts = |usage: &Usage| {
        [
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_input_tokens,
            usage.reasoning_output_tokens,
            usage.total_tokens,
        ]
    };
    if counts(&total)
        .iter()
        .zip(counts(&previous))
        .any(|(now, before)| *now < before)
    {
        return total;
    }
    Usage {
        input_tokens: total.input_tokens - previous.input_tokens,
        output_tokens: total.output_tokens - previous.output_tokens,
        cached_input_tokens: total.cached_input_tokens - previous.cached_input_tokens,
        reasoning_output_tokens: total.reasoning_output_tokens - previous.reasoning_output_tokens,
        total_tokens: total.total_tokens - previous.total_tokens,
    }
}

/// A turn's usage from the `usage` field, for a result with no breakdown.
fn usage_of(envelope: &Value) -> Usage {
    let usage = Usage {
        input_tokens: count(envelope.pointer("/usage/input_tokens")),
        output_tokens: count(envelope.pointer("/usage/output_tokens")),
        ..Usage::default()
    };
    Usage {
        total_tokens: usage.input_tokens.saturating_add(usage.output_tokens),
        ..usage
    }
}

/// The context window `result` reports, from `modelUsage`. The largest, when a
/// turn used several models: the conversation lives in the main one's.
fn context_window_of(envelope: &Value) -> Option<u64> {
    envelope
        .get("modelUsage")
        .and_then(Value::as_object)?
        .values()
        .map(|model| count(model.get("contextWindow")))
        .max()
        .filter(|window| *window > 0)
}

fn count(value: Option<&Value>) -> u64 {
    counted(value).unwrap_or(0)
}

/// A count, or `None` when the frame did not carry one.
fn counted(value: Option<&Value>) -> Option<u64> {
    value.and_then(|value| match value {
        Value::Number(n) => n.as_u64(),
        // Claude writes some counts as strings on older builds.
        Value::String(s) => s.parse().ok(),
        _ => None,
    })
}

/// Whether a frame proves Claude read the prompt FlowX sent.
///
/// Content does, and so do the lifecycle frames of a turn that is actually
/// running — a compaction, a request going out — even though none of them is
/// shown, because a prompt Claude has read must not be offered back for
/// sending again. `init` does not: a resumed session opens a turn for each
/// batch of notifications about tasks the previous process lost, and answers it
/// with a zero-turn `result`, all before the prompt is read. For the same
/// reason a `result` that answers a task notification proves nothing, and nor
/// do task, hook and housekeeping frames. A `control_response` answers FlowX's
/// own control requests, which come with or without a prompt.
fn answers_prompt(kind: &str, envelope: &Value) -> bool {
    match kind {
        "stream_event" | "assistant" | "user" | "control_request" => true,
        "result" => !answers_notification(envelope),
        "system" => matches!(
            envelope.get("subtype").and_then(Value::as_str),
            Some("status" | "compact_boundary")
        ),
        _ => false,
    }
}

/// Whether a `result` closes a turn a task notification started, rather than
/// one a user message did.
fn answers_notification(envelope: &Value) -> bool {
    envelope.pointer("/origin/kind").and_then(Value::as_str) == Some("task-notification")
}

/// Whether a `result` is Claude acknowledging a task notification without
/// asking the model anything: no turns, no usage, no answer. A resumed session
/// sends one for each batch of tasks the previous process lost.
fn acknowledges_notification(envelope: &Value) -> bool {
    answers_notification(envelope)
        && envelope.get("num_turns").and_then(Value::as_u64) == Some(0)
        && envelope.get("is_error").and_then(Value::as_bool) != Some(true)
}

/// Whether a task frame is the CLI's own background housekeeping.
fn is_ambient(envelope: &Value) -> bool {
    envelope.get("ambient").and_then(Value::as_bool) == Some(true)
}

/// The decision object Claude expects back for a tool call.
pub fn permission_decision(
    approved: bool,
    tool_use_id: &str,
    updated_input: Option<&Value>,
    denial_message: &str,
) -> Value {
    if approved {
        let mut decision = json!({
            "behavior": "allow",
            "toolUseID": tool_use_id,
            "decisionClassification": "user_temporary",
        });
        if let (Some(object), Some(input)) = (decision.as_object_mut(), updated_input) {
            object.insert("updatedInput".into(), input.clone());
        }
        return decision;
    }
    json!({
        "behavior": "deny",
        "message": denial_message,
        "toolUseID": tool_use_id,
        "decisionClassification": "user_reject",
    })
}

/// Wrap a decision in the envelope Claude reads.
pub fn control_response(request_id: &str, response: &Value) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": response,
        },
    })
}

/// Refuse a control request this parser cannot serve.
///
/// The alternative — ignoring it — leaves Claude blocked on a reply that will
/// never come, which reads to the user as a turn that hung.
pub fn refuse_control(request_id: &str, subtype: &str) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "error",
            "request_id": request_id,
            "error": format!("FlowX does not support the '{subtype}' control request."),
        },
    })
}

/// How risky a tool call is.
///
/// Claude names the tool and passes the input, so the judgement is a lookup
/// rather than a guess.
pub fn risk_for(tool_name: &str, input: &Value) -> RiskLevel {
    match tool_name {
        // Reads nothing outside the workspace and changes nothing.
        "Read" | "Glob" | "Grep" | "NotebookRead" | "WebFetch" | "TodoWrite" | "ExitPlanMode" => {
            RiskLevel::Safe
        }
        // Writes inside the workspace.
        "Edit" | "MultiEdit" | "NotebookEdit" | "Write" => RiskLevel::Moderate,
        // Runs a command, so the command decides.
        "Bash" => match input.get("command").and_then(Value::as_str) {
            Some(command) if is_read_only_command(command) => RiskLevel::Safe,
            _ => RiskLevel::Dangerous,
        },
        _ => RiskLevel::Moderate,
    }
}

/// The read-only check, shared with Codex so the two cannot disagree.
pub use crate::shell::is_read_only_command;

/// Whether a Claude tool call only reads, judged by Claude's own tool names.
///
/// The searches and reads are read-only by name; `Bash` is as read-only as its
/// command; everything else, edits included, is not.
pub fn is_read_only_tool(tool_name: &str, input: &Value) -> bool {
    match tool_name {
        "Read" | "Grep" | "Glob" | "LS" | "NotebookRead" => true,
        "Bash" => input
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(is_read_only_command),
        _ => false,
    }
}

/// A string field, trimmed, or `None` when it is absent or blank.
fn non_empty(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// A string, truncated on a character boundary.
///
/// Byte truncation would split a multi-byte character and produce invalid UTF-8,
/// so the cut is walked rather than sliced.
fn bounded(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_string();
    }
    let mut end = 0;
    for (index, character) in value.char_indices() {
        if index + character.len_utf8() > maximum {
            end = index;
            break;
        }
        end = index + character.len_utf8();
    }
    format!("{}…", &value[..end])
}

/// A value as JSON text, for display and for comparison.
///
/// `serde_json` preserves insertion order, and the input order is whatever the
/// provider sent, so the same tool call can serialize two different ways. Sorting
/// the keys makes the text a stable identity for the call, which is what lets a
/// dedup comparison mean anything.
fn json_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        value => Some(canonical_json(value)),
    }
}

/// Serialize with every object's keys in sorted order.
fn canonical_json(value: &Value) -> String {
    let mut buffer = Vec::new();
    write_canonical(value, &mut buffer);
    String::from_utf8(buffer).unwrap_or_default()
}

fn write_canonical(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push(b'{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(serde_json::to_string(key).unwrap_or_default().as_bytes());
                out.push(b':');
                write_canonical(&map[*key], out);
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        scalar => out.extend_from_slice(&serde_json::to_vec(scalar).unwrap_or_default()),
    }
}

/// The readable text of a tool result, which may be a string or a block list.
fn content_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(value) => json_string(Some(value)).unwrap_or_default(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(value: Value) -> String {
        value.to_string()
    }

    #[test]
    fn a_text_delta_is_emitted_once() {
        let mut parser = ClaudeStreamParser::new();
        let events = parser.events(&line(json!({
            "type": "stream_event",
            "event": {
                "type": "content_block_delta",
                "delta": { "type": "text_delta", "text": "hello" }
            }
        })));
        assert_eq!(events, vec![ProviderEvent::TextDelta("hello".into())]);
    }

    #[test]
    fn the_assistant_echo_is_dropped_when_the_text_was_streamed() {
        // The single most important behaviour here: without it every message
        // appears twice.
        let mut parser = ClaudeStreamParser::new();
        parser.events(&line(json!({
            "type": "stream_event",
            "event": {
                "type": "content_block_delta",
                "delta": { "type": "text_delta", "text": "hello" }
            }
        })));
        let events = parser.events(&line(json!({
            "type": "assistant",
            "message": { "content": [{ "type": "text", "text": "hello" }] }
        })));
        assert!(
            events.is_empty(),
            "the streamed text was echoed: {events:?}"
        );
    }

    #[test]
    fn an_unstreamed_assistant_message_is_kept() {
        // A non-streaming run has no deltas, so its text is the only copy.
        let mut parser = ClaudeStreamParser::new();
        let events = parser.events(&line(json!({
            "type": "assistant",
            "message": { "content": [{ "type": "text", "text": "whole answer" }] }
        })));
        assert_eq!(events, vec![ProviderEvent::Text("whole answer".into())]);
    }

    #[test]
    fn resetting_clears_the_echo_suppression() {
        // Otherwise the second turn's only text would be swallowed.
        let mut parser = ClaudeStreamParser::new();
        parser.events(&line(json!({
            "type": "stream_event",
            "event": {
                "type": "content_block_delta",
                "delta": { "type": "text_delta", "text": "first" }
            }
        })));
        assert!(parser.saw_text_delta());
        parser.reset();
        assert!(!parser.saw_text_delta());
        let events = parser.events(&line(json!({
            "type": "assistant",
            "message": { "content": [{ "type": "text", "text": "second" }] }
        })));
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn a_tool_call_reported_twice_is_emitted_once() {
        let frame = line(json!({
            "type": "assistant",
            "message": { "content": [{ "type": "tool_use", "id": "t1", "name": "Read",
                                       "input": { "path": "/tmp/x" } }] }
        }));
        let mut parser = ClaudeStreamParser::new();
        assert_eq!(parser.events(&frame).len(), 1);
        assert!(
            parser.events(&frame).is_empty(),
            "the repeated tool call was emitted twice"
        );
    }

    #[test]
    fn a_tool_id_is_never_re_emitted_after_the_window_slides() {
        // The bound evicts the oldest, it does not clear the lot. A set that
        // cleared wholesale re-enabled deduplication for every id it had already
        // seen, and the `assistant` echo of an old tool was emitted twice.
        let mut parser = ClaudeStreamParser::new();
        let first = r#"{"type":"assistant","message":{"content":[
            {"type":"tool_use","id":"t0","name":"Read"}]}}"#;
        assert_eq!(parser.events(first).len(), 1);

        // Push well past the window.
        for i in 1..=super::MAX_TRACKED_TOOL_IDS + 50 {
            parser.events(&format!(
                r#"{{"type":"assistant","message":{{"content":[
                    {{"type":"tool_use","id":"t{i}","name":"Read"}}]}}}}"#
            ));
        }

        // The oldest has fallen out of the window, which is the documented
        // tradeoff. What must not happen is the *recent* ids being re-emitted.
        let again = parser.events(&format!(
            r#"{{"type":"assistant","message":{{"content":[
                {{"type":"tool_use","id":"t{}","name":"Read"}}]}}}}"#,
            super::MAX_TRACKED_TOOL_IDS + 50
        ));
        assert!(
            again.is_empty(),
            "a tool inside the window was reported twice: {again:?}"
        );
    }

    #[test]
    fn distinct_tool_calls_both_appear() {
        let mut parser = ClaudeStreamParser::new();
        let first = parser.events(&line(json!({
            "type": "assistant",
            "message": { "content": [{ "type": "tool_use", "id": "t1", "name": "Read" }] }
        })));
        let second = parser.events(&line(json!({
            "type": "assistant",
            "message": { "content": [{ "type": "tool_use", "id": "t2", "name": "Write" }] }
        })));
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
    }

    #[test]
    fn a_tool_result_is_reported_with_its_id_and_error_flag() {
        let mut parser = ClaudeStreamParser::new();
        let events = parser.events(&line(json!({
            "type": "user",
            "message": { "content": [{ "type": "tool_result", "tool_use_id": "t1",
                                       "content": "done", "is_error": false }] }
        })));
        assert_eq!(
            events,
            vec![ProviderEvent::ToolResult {
                id: "t1".into(),
                content: "done".into(),
                is_error: false,
            }]
        );
    }

    #[test]
    fn a_tool_result_split_into_blocks_is_joined() {
        let mut parser = ClaudeStreamParser::new();
        let events = parser.events(&line(json!({
            "type": "user",
            "message": { "content": [{ "type": "tool_result", "tool_use_id": "t1",
                "content": [{ "type": "text", "text": "one" },
                            { "type": "text", "text": "two" }] }] }
        })));
        match &events[0] {
            ProviderEvent::ToolResult { content, .. } => assert_eq!(content, "one\ntwo"),
            other => panic!("expected a tool result, got {other:?}"),
        }
    }

    #[test]
    fn an_init_frame_starts_the_turn() {
        let mut parser = ClaudeStreamParser::new();
        let with_subtype = parser.events(&line(json!({
            "type": "system", "subtype": "init", "session_id": "s1"
        })));
        assert_eq!(
            with_subtype,
            vec![ProviderEvent::Lifecycle(Lifecycle::TurnStarted)]
        );

        // Older builds send no subtype at all.
        let older = ClaudeStreamParser::new().events(&line(json!({"type": "system"})));
        assert_eq!(
            older,
            vec![ProviderEvent::Lifecycle(Lifecycle::TurnStarted)]
        );
    }

    #[test]
    fn compaction_is_reported_as_lifecycle() {
        let mut parser = ClaudeStreamParser::new();
        let compacting = parser.events(&line(json!({
            "type": "system", "subtype": "status", "status": "compacting"
        })));
        assert_eq!(
            compacting,
            vec![ProviderEvent::Lifecycle(Lifecycle::Compacting)]
        );

        let compacted = parser.events(&line(json!({
            "type": "system", "subtype": "compact_boundary"
        })));
        assert_eq!(
            compacted,
            vec![ProviderEvent::Lifecycle(Lifecycle::Compacted)]
        );
    }

    #[test]
    fn a_result_reports_usage_then_completion() {
        let mut parser = ClaudeStreamParser::new();
        let events = parser.events(&line(json!({
            "type": "result",
            "subtype": "end_turn",
            "usage": { "input_tokens": 100, "output_tokens": 20 }
        })));
        assert_eq!(
            events,
            vec![
                ProviderEvent::Usage(Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    total_tokens: 120,
                    ..Usage::default()
                }),
                ProviderEvent::Done {
                    stop_reason: "end_turn".into()
                },
            ]
        );
        assert!(parser.did_finish());
    }

    #[test]
    fn a_follow_ups_result_counts_only_what_it_added_to_the_process_total() {
        // `modelUsage` is the process's running total. A task notification's
        // follow-up answers in the same process, so its `result` repeats the
        // first turn's tokens, which used to be added a second time.
        let mut parser = ClaudeStreamParser::new();
        let result = |input: u64, output: u64| {
            line(json!({
                "type": "result",
                "subtype": "success",
                "modelUsage": {
                    "claude-opus": { "inputTokens": input, "outputTokens": output }
                }
            }))
        };
        let usage = |events: Vec<ProviderEvent>| {
            events
                .into_iter()
                .find_map(|event| match event {
                    ProviderEvent::Usage(usage) => Some(usage),
                    _ => None,
                })
                .expect("a usage event")
        };
        let first = usage(parser.events(&result(100, 20)));
        assert_eq!((first.input_tokens, first.output_tokens), (100, 20));
        parser.events(&line(json!({ "type": "system", "subtype": "init" })));
        let second = usage(parser.events(&result(130, 25)));
        assert_eq!(
            (
                second.input_tokens,
                second.output_tokens,
                second.total_tokens
            ),
            (30, 5, 35)
        );
        // A turn reset between turns does not forget the process's total.
        parser.reset();
        let third = usage(parser.events(&result(140, 25)));
        assert_eq!((third.input_tokens, third.total_tokens), (10, 10));
    }

    #[test]
    fn a_reply_reports_how_much_context_it_answered_and_the_result_its_window() {
        // Claude never reported context, so a Claude thread's meter could only
        // ever say it did not know.
        let mut parser = ClaudeStreamParser::new();
        let reply = |text: &str| {
            line(json!({
                "type": "assistant",
                "message": {
                    "content": [{ "type": "text", "text": text }],
                    "usage": {
                        "input_tokens": 12,
                        "cache_creation_input_tokens": 3000,
                        "cache_read_input_tokens": 40000,
                        "output_tokens": 500
                    }
                }
            }))
        };
        let first = parser.events(&reply("Looking."));
        assert!(
            first.contains(&ProviderEvent::ContextUsage {
                used: 43_512,
                window: 0
            }),
            "{first:?}"
        );
        // The same reply split across content blocks is reported once.
        let echo = parser.events(&reply("Found it."));
        assert!(
            !echo
                .iter()
                .any(|event| matches!(event, ProviderEvent::ContextUsage { .. }))
        );

        let end = parser.events(&line(json!({
            "type": "result",
            "usage": { "input_tokens": 12, "output_tokens": 500 },
            "modelUsage": {
                "claude-opus": { "inputTokens": 12, "outputTokens": 500, "contextWindow": 200000 },
                "claude-haiku": { "inputTokens": 3, "outputTokens": 1, "contextWindow": 200000 }
            }
        })));
        assert_eq!(
            end[0],
            ProviderEvent::ContextUsage {
                used: 43_512,
                window: 200_000
            }
        );
        // A result with no window leaves the reply's figure standing.
        let mut quiet = ClaudeStreamParser::new();
        quiet.events(&reply("Hi."));
        let end = quiet.events(&line(json!({ "type": "result", "usage": {} })));
        assert!(
            !end.iter()
                .any(|event| matches!(event, ProviderEvent::ContextUsage { .. }))
        );
    }

    #[test]
    fn a_second_result_is_ignored() {
        // Otherwise a turn's cost is counted twice.
        let mut parser = ClaudeStreamParser::new();
        let frame = line(json!({
            "type": "result", "usage": { "input_tokens": 10, "output_tokens": 1 }
        }));
        assert!(!parser.events(&frame).is_empty());
        assert!(parser.events(&frame).is_empty());
    }

    #[test]
    fn per_model_usage_is_preferred_over_the_single_figure() {
        // `usage` reports only the last model, so a two-model turn would be
        // undercounted.
        let mut parser = ClaudeStreamParser::new();
        let events = parser.events(&line(json!({
            "type": "result",
            "usage": { "input_tokens": 50, "output_tokens": 5 },
            "modelUsage": {
                "model-a": { "inputTokens": 30, "outputTokens": 3 },
                "model-b": { "inputTokens": 20, "outputTokens": 2 }
            }
        })));
        match &events[0] {
            ProviderEvent::Usage(usage) => {
                assert_eq!(usage.input_tokens, 50, "only the last model was counted");
                assert_eq!(usage.output_tokens, 5);
            }
            other => panic!("expected usage, got {other:?}"),
        }
    }

    #[test]
    fn an_errored_result_is_reported_as_an_error_not_as_completion() {
        let mut parser = ClaudeStreamParser::new();
        let events = parser.events(&line(json!({
            "type": "result", "is_error": true, "result": "hit a limit"
        })));
        assert_eq!(events, vec![ProviderEvent::Error("hit a limit".into())]);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, ProviderEvent::Done { .. })),
            "a failed turn reported completion"
        );
    }

    #[test]
    fn a_line_that_is_not_json_is_survived() {
        let mut parser = ClaudeStreamParser::new();
        assert!(parser.events("not json at all").is_empty());
        assert!(parser.events("{").is_empty());
        assert!(parser.events("").is_empty());
    }

    #[test]
    fn an_unknown_type_is_ignored() {
        let mut parser = ClaudeStreamParser::new();
        assert!(
            parser
                .events(&line(json!({"type": "future_thing"})))
                .is_empty()
        );
        assert!(parser.events(&line(json!({"no": "type"}))).is_empty());
    }

    #[test]
    fn a_tool_call_becomes_an_approval_request() {
        let mut parser = ClaudeStreamParser::new();
        let events = parser.events(&line(json!({
            "type": "control_request",
            "request_id": "r1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Bash",
                "tool_use_id": "t1",
                "input": { "command": "rm -rf /" }
            }
        })));
        assert_eq!(events.len(), 1);
        match &events[0] {
            ProviderEvent::Approval(approval) => {
                assert_eq!(approval.tool_name, "Bash");
                assert_eq!(approval.risk, RiskLevel::Dangerous);
            }
            other => panic!("expected an approval, got {other:?}"),
        }
        assert_eq!(parser.pending_ids().len(), 1);
    }

    #[test]
    fn a_read_only_tool_is_safe_and_a_write_is_not() {
        let mut parser = ClaudeStreamParser::new();
        let read = parser.events(&line(json!({
            "type": "control_request", "request_id": "r1",
            "request": { "subtype": "can_use_tool", "tool_name": "Read", "tool_use_id": "t1",
                         "input": {} }
        })));
        match &read[0] {
            ProviderEvent::Approval(a) => assert_eq!(a.risk, RiskLevel::Safe),
            other => panic!("expected an approval, got {other:?}"),
        }

        let write = ClaudeStreamParser::new().events(&line(json!({
            "type": "control_request", "request_id": "r1",
            "request": { "subtype": "can_use_tool", "tool_name": "Write", "tool_use_id": "t1",
                         "input": {} }
        })));
        match &write[0] {
            ProviderEvent::Approval(a) => assert_eq!(a.risk, RiskLevel::Moderate),
            other => panic!("expected an approval, got {other:?}"),
        }
    }

    #[test]
    fn printing_the_environment_is_not_a_read_only_command() {
        // `env` and `printenv` with no argument print every variable, which on a
        // developer machine is where API keys are. The approval row would label
        // that safe, which is the whole point of the check being conservative.
        for command in ["env", "printenv", "env | grep KEY", "printenv > /tmp/leak"] {
            assert!(
                !is_read_only_command(command),
                "{command:?} was treated as read-only"
            );
            assert_eq!(
                risk_for("Bash", &json!({ "command": command })),
                RiskLevel::Dangerous,
                "{command:?} was not treated as dangerous"
            );
        }
    }

    #[test]
    fn a_command_that_only_reads_is_still_read_only() {
        // The check has to still say yes to something, or the list above proves
        // nothing about it being a judgement at all.
        for command in ["cat Cargo.toml", "ls -la", "git status"] {
            assert!(is_read_only_command(command), "{command:?} was refused");
        }
    }

    #[test]
    fn a_bash_tool_is_safe_only_for_a_reading_command() {
        assert_eq!(
            risk_for("Bash", &json!({"command": "cargo test"})),
            RiskLevel::Dangerous
        );
        assert_eq!(
            risk_for("Bash", &json!({"command": "cat Cargo.toml"})),
            RiskLevel::Safe
        );
        assert_eq!(
            risk_for("Bash", &json!({"command": "rm -rf build"})),
            RiskLevel::Dangerous
        );
    }

    #[test]
    fn a_chained_command_is_not_read_only() {
        // `cat x | sh` starts with a reader and is not one.
        assert!(!is_read_only_command("cat script | sh"));
        assert!(!is_read_only_command("ls; rm -rf /"));
        assert!(!is_read_only_command("cat x > /etc/passwd"));
        assert!(!is_read_only_command("cat x && rm y"));
        assert!(!is_read_only_command(""));
        assert!(is_read_only_command("git status"));
        // A reader that is a prefix of a different word must not match.
        assert!(!is_read_only_command("catalog"));
    }

    #[test]
    fn approving_produces_the_shape_claude_expects() {
        let mut parser = ClaudeStreamParser::new();
        parser.events(&line(json!({
            "type": "control_request", "request_id": "r1",
            "request": { "subtype": "can_use_tool", "tool_name": "Bash", "tool_use_id": "t1",
                         "input": { "command": "cargo test" } }
        })));
        let id = parser.pending_ids()[0].clone();
        let response = parser.approve(&id, true).expect("a response");

        assert_eq!(response["type"], "control_response");
        assert_eq!(response["response"]["subtype"], "success");
        assert_eq!(response["response"]["request_id"], "r1");
        let decision = &response["response"]["response"];
        assert_eq!(decision["behavior"], "allow");
        assert_eq!(decision["toolUseID"], "t1");
        assert_eq!(decision["decisionClassification"], "user_temporary");
        assert!(parser.pending_ids().is_empty());
    }

    #[test]
    fn denying_carries_a_message() {
        let mut parser = ClaudeStreamParser::new();
        parser.events(&line(json!({
            "type": "control_request", "request_id": "r1",
            "request": { "subtype": "can_use_tool", "tool_name": "Bash", "tool_use_id": "t1",
                         "input": {} }
        })));
        let id = parser.pending_ids()[0].clone();
        let response = parser.approve(&id, false).expect("a response");
        let decision = &response["response"]["response"];
        assert_eq!(decision["behavior"], "deny");
        assert_eq!(decision["decisionClassification"], "user_reject");
    }

    #[test]
    fn a_second_answer_to_the_same_request_is_ignored() {
        let mut parser = ClaudeStreamParser::new();
        parser.events(&line(json!({
            "type": "control_request", "request_id": "r1",
            "request": { "subtype": "can_use_tool", "tool_name": "Bash", "tool_use_id": "t1",
                         "input": {} }
        })));
        let id = parser.pending_ids()[0].clone();
        assert!(parser.approve(&id, true).is_some());
        assert!(parser.approve(&id, true).is_none(), "answered twice");
    }

    #[test]
    fn an_ask_user_question_becomes_a_question_request() {
        let mut parser = ClaudeStreamParser::new();
        let events = parser.events(&line(json!({
            "type": "control_request", "request_id": "r1",
            "request": {
                "subtype": "can_use_tool", "tool_name": "AskUserQuestion", "tool_use_id": "t1",
                "input": { "questions": [{
                    "question": "Which database?",
                    "header": "Database",
                    "options": [{ "label": "Postgres", "description": "primary store" },
                                { "label": "SQLite" }]
                }] }
            }
        })));
        match &events[0] {
            ProviderEvent::UserInput(request) => {
                assert_eq!(request.questions.len(), 1);
                let question = &request.questions[0];
                assert_eq!(question.question, "Which database?");
                assert_eq!(question.header, "Database");
                assert_eq!(question.options.len(), 2);
                assert_eq!(question.options[0].label, "Postgres");
            }
            other => panic!("expected a question request, got {other:?}"),
        }
    }

    #[test]
    fn answering_a_question_maps_back_to_claudes_own_text() {
        let mut parser = ClaudeStreamParser::new();
        parser.events(&line(json!({
            "type": "control_request", "request_id": "r1",
            "request": {
                "subtype": "can_use_tool", "tool_name": "AskUserQuestion", "tool_use_id": "t1",
                "input": { "questions": [
                    { "question": "Which database?" },
                    { "question": "Which region?" }
                ] }
            }
        })));
        let id = parser.pending_ids()[0].clone();
        let response = parser
            .answer_questions(
                &id,
                &[
                    ("question-1".into(), "Postgres".into()),
                    ("question-2".into(), "eu-west-1".into()),
                ],
            )
            .expect("a response");

        let input = &response["response"]["response"]["updatedInput"];
        let answers = input["answers"].as_array().expect("answers were set");
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0]["question"], "Which database?");
        assert_eq!(answers[0]["answer"], "Postgres");
        assert_eq!(answers[1]["answer"], "eu-west-1");
    }

    #[test]
    fn an_undecodable_question_payload_is_refused_rather_than_left_blocking() {
        // Claude blocks on every control request; leaving this unanswered hangs
        // the turn.
        let mut parser = ClaudeStreamParser::new();
        let events = parser.events(&line(json!({
            "type": "control_request", "request_id": "r1",
            "request": {
                "subtype": "can_use_tool", "tool_name": "AskUserQuestion", "tool_use_id": "t1",
                "input": { "questions": [] }
            }
        })));
        match &events[0] {
            ProviderEvent::ToolResult { is_error, .. } => assert!(*is_error),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn an_unsupported_control_subtype_can_be_refused_explicitly() {
        // The alternative to this is a turn that hangs waiting for a reply.
        let response = refuse_control("r9", "elicitation");
        assert_eq!(response["response"]["subtype"], "error");
        assert_eq!(response["response"]["request_id"], "r9");
        assert!(
            response["response"]["error"]
                .as_str()
                .unwrap()
                .contains("elicitation")
        );
    }

    #[test]
    fn an_unrecognised_control_subtype_produces_no_approval() {
        let mut parser = ClaudeStreamParser::new();
        let events = parser.events(&line(json!({
            "type": "control_request", "request_id": "r1",
            "request": { "subtype": "something_new", "tool_name": "X" }
        })));
        assert!(events.is_empty());
        assert!(parser.pending_ids().is_empty());
    }

    #[test]
    fn auto_approval_records_the_request_without_asking_the_user() {
        let mut parser = ClaudeStreamParser::new();
        parser.auto_approve = true;
        let events = parser.events(&line(json!({
            "type": "control_request", "request_id": "r1",
            "request": { "subtype": "can_use_tool", "tool_name": "Bash", "tool_use_id": "t1",
                         "input": {} }
        })));
        assert!(
            events.is_empty(),
            "the user was asked despite auto-approval"
        );
        let id = parser.pending_ids()[0].clone();
        let response = parser.approve(&id, true).expect("a response");
        assert_eq!(response["response"]["response"]["behavior"], "allow");
    }

    #[test]
    fn a_declined_question_does_not_claim_to_allow() {
        let mut parser = ClaudeStreamParser::new();
        parser.events(&line(json!({
            "type": "control_request", "request_id": "r1",
            "request": {
                "subtype": "can_use_tool", "tool_name": "AskUserQuestion", "tool_use_id": "t1",
                "input": { "questions": [{ "question": "Which?" }] }
            }
        })));
        let id = parser.pending_ids()[0].clone();
        let response = parser.decline_questions(&id).expect("a response");
        assert_eq!(response["response"]["response"]["behavior"], "deny");
    }

    #[test]
    fn text_is_truncated_on_a_character_boundary() {
        // Byte truncation would split a multi-byte character and produce invalid
        // UTF-8, which cannot even be put in a JSON string.
        let text = "é".repeat(10);
        let bounded_text = bounded(&text, 5);
        assert!(bounded_text.ends_with('…'));
        assert_eq!(bounded_text.chars().filter(|c| *c == 'é').count(), 2);
    }

    #[test]
    fn short_text_is_not_truncated() {
        assert_eq!(bounded("hello", 100), "hello");
        assert_eq!(bounded("hello", 5), "hello");
    }

    #[test]
    fn a_blank_field_is_treated_as_absent() {
        assert!(non_empty(Some(&json!("  "))).is_none());
        assert!(non_empty(Some(&json!(""))).is_none());
        assert_eq!(non_empty(Some(&json!(" x "))).as_deref(), Some("x"));
        assert!(non_empty(None).is_none());
    }

    #[test]
    fn too_many_questions_are_truncated() {
        let mut parser = ClaudeStreamParser::new();
        let questions: Vec<Value> = (0..10)
            .map(|i| json!({ "question": format!("Q{i}"), "options": [] }))
            .collect();
        let events = parser.events(&line(json!({
            "type": "control_request", "request_id": "r1",
            "request": { "subtype": "can_use_tool", "tool_name": "AskUserQuestion",
                         "tool_use_id": "t1", "input": { "questions": questions } }
        })));
        match &events[0] {
            ProviderEvent::UserInput(request) => {
                assert_eq!(request.questions.len(), MAX_QUESTIONS);
            }
            other => panic!("expected a question request, got {other:?}"),
        }
    }

    #[test]
    fn too_many_options_are_truncated() {
        let options: Vec<Value> = (0..20)
            .map(|i| json!({ "label": format!("O{i}") }))
            .collect();
        let mut parser = ClaudeStreamParser::new();
        let events = parser.events(&line(json!({
            "type": "control_request", "request_id": "r1",
            "request": { "subtype": "can_use_tool", "tool_name": "AskUserQuestion",
                         "tool_use_id": "t1",
                         "input": { "questions": [{ "question": "Q", "options": options }] } }
        })));
        match &events[0] {
            ProviderEvent::UserInput(request) => {
                assert_eq!(request.questions[0].options.len(), MAX_OPTIONS);
            }
            other => panic!("expected a question request, got {other:?}"),
        }
    }

    #[test]
    fn answering_a_question_as_an_approval_leaves_it_pending() {
        // The bug this guards: removing the entry before checking its kind meant
        // a wrong-kind answer consumed it, and Claude then blocked forever on a
        // reply that could never be given.
        let mut parser = ClaudeStreamParser::new();
        parser.events(&line(json!({
            "type": "control_request", "request_id": "r1",
            "request": {
                "subtype": "can_use_tool", "tool_name": "AskUserQuestion", "tool_use_id": "t1",
                "input": { "questions": [{ "question": "Which?" }] }
            }
        })));
        let id = parser.pending_ids()[0].clone();

        assert!(
            parser.approve(&id, true).is_none(),
            "a question was approved"
        );
        assert!(
            parser.decline_questions(&id).is_some(),
            "the question was lost"
        );
        assert!(parser.pending_ids().is_empty());
    }

    #[test]
    fn approving_a_question_id_leaves_the_approval_pending() {
        let mut parser = ClaudeStreamParser::new();
        parser.events(&line(json!({
            "type": "control_request", "request_id": "r1",
            "request": { "subtype": "can_use_tool", "tool_name": "Bash", "tool_use_id": "t1",
                         "input": {} }
        })));
        let id = parser.pending_ids()[0].clone();
        assert!(parser.answer_questions(&id, &[]).is_none());
        assert!(parser.approve(&id, true).is_some());
    }

    #[test]
    fn a_pending_controls_kind_is_readable() {
        let mut parser = ClaudeStreamParser::new();
        parser.events(&line(json!({
            "type": "control_request", "request_id": "r1",
            "request": { "subtype": "can_use_tool", "tool_name": "Bash", "tool_use_id": "t1",
                         "input": {} }
        })));
        parser.events(&line(json!({
            "type": "control_request", "request_id": "r2",
            "request": {
                "subtype": "can_use_tool", "tool_name": "AskUserQuestion", "tool_use_id": "t2",
                "input": { "questions": [{ "question": "Which?" }] }
            }
        })));
        let kinds = parser.pending_kinds();
        assert_eq!(kinds.len(), 2);
        assert_eq!(parser.kind_of(&kinds[0].0), Some(PendingKind::Approval));
        assert_eq!(parser.kind_of(&kinds[1].0), Some(PendingKind::UserInput));
        assert_eq!(parser.kind_of("nonsense"), None);
    }

    #[test]
    fn an_unsupported_control_subtype_is_refused_rather_than_left_blocking() {
        // Claude blocks on every control request. Ignoring an unknown one leaves
        // the turn hanging with no error and no way to recover.
        let mut parser = ClaudeStreamParser::new();
        let events = parser.events(&line(json!({
            "type": "control_request", "request_id": "r1",
            "request": { "subtype": "elicitation", "tool_name": "Whatever" }
        })));
        assert!(events.is_empty(), "an unknown subtype produced an approval");
        let refusal = parser
            .take_refusal()
            .expect("a refusal was recorded for the session to write back");
        assert_eq!(refusal["type"], "control_response");
        assert_eq!(refusal["response"]["request_id"], "r1");
        assert_eq!(
            parser.take_refusal(),
            None,
            "the refusal was not taken once"
        );
    }

    #[test]
    fn a_control_request_with_no_usable_fields_is_still_refused() {
        let mut parser = ClaudeStreamParser::new();
        parser.events(&line(json!({
            "type": "control_request", "request_id": "r1"
        })));
        assert!(parser.take_refusal().is_some());
    }

    #[test]
    fn resetting_clears_any_outstanding_refusal() {
        let mut parser = ClaudeStreamParser::new();
        parser.events(&line(json!({
            "type": "control_request", "request_id": "r1",
            "request": { "subtype": "elicitation" }
        })));
        assert!(parser.take_refusal().is_some());
        parser.reset();
        assert!(parser.take_refusal().is_none());
    }

    #[test]
    fn two_requests_open_at_once_get_different_ids() {
        // Claude can block on a question and a tool at the same time. Ids
        // derived from the pending count would both be `claude-*-1`, so
        // answering one would answer the other.
        let mut parser = ClaudeStreamParser::new();
        parser.events(&line(json!({
            "type": "control_request", "request_id": "r1",
            "request": { "subtype": "can_use_tool", "tool_name": "Bash", "tool_use_id": "t1",
                         "input": { "command": "cargo test" } }
        })));
        parser.events(&line(json!({
            "type": "control_request", "request_id": "r2",
            "request": {
                "subtype": "can_use_tool", "tool_name": "AskUserQuestion", "tool_use_id": "t2",
                "input": { "questions": [{ "question": "Which database?" }] }
            }
        })));

        let ids = parser.pending_ids();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1], "two pending requests shared an id");

        // Answering the *second* must leave the first pending.
        assert!(parser.answer_questions(&ids[1], &[]).is_some());
        assert_eq!(parser.pending_ids(), vec![ids[0].clone()]);
    }

    #[test]
    fn an_id_stays_valid_after_a_different_request_answers() {
        let mut parser = ClaudeStreamParser::new();
        for id in ["r1", "r2", "r3"] {
            parser.events(&line(json!({
                "type": "control_request", "request_id": id,
                "request": { "subtype": "can_use_tool", "tool_name": "Bash", "tool_use_id": id,
                             "input": {} }
            })));
        }
        let ids = parser.pending_ids();
        // Answer the middle one first.
        assert!(parser.approve(&ids[1], true).is_some());
        // The others must still be answerable, quoting their own request ids.
        let response = parser
            .approve(&ids[2], true)
            .expect("the third is answerable");
        assert_eq!(response["response"]["request_id"], "r3");
        assert_eq!(response["response"]["response"]["toolUseID"], "r3");
    }

    #[test]
    fn the_same_tool_call_serializes_the_same_way_regardless_of_key_order() {
        // The text is a dedup key, so two spellings of the same call must not
        // look different.
        let a = json_string(Some(&json!({"path": "/tmp", "line": 2}))).unwrap();
        let b = json_string(Some(&json!({"line": 2, "path": "/tmp"}))).unwrap();
        assert_eq!(a, b);
        // And nested objects are sorted too.
        let c = json_string(Some(&json!({"outer": {"b": 1, "a": 2}}))).unwrap();
        let d = json_string(Some(&json!({"outer": {"a": 2, "b": 1}}))).unwrap();
        assert_eq!(c, d);
        assert_eq!(c, r#"{"outer":{"a":2,"b":1}}"#);
    }

    #[test]
    fn canonical_json_keeps_array_order() {
        // Sorting must not reorder a list, where order is meaning.
        let text = canonical_json(&json!({"items": [3, 1, 2]}));
        assert_eq!(text, r#"{"items":[3,1,2]}"#);
    }

    #[test]
    fn a_tool_input_is_recorded_as_text() {
        let mut parser = ClaudeStreamParser::new();
        let events = parser.events(&line(json!({
            "type": "assistant",
            "message": { "content": [{ "type": "tool_use", "id": "t1", "name": "Bash",
                                       "input": { "command": "ls" } }] }
        })));
        match &events[0] {
            ProviderEvent::ToolUse { input, .. } => {
                assert_eq!(input, r#"{"command":"ls"}"#);
            }
            other => panic!("expected a tool use, got {other:?}"),
        }
    }

    #[test]
    fn string_token_counts_are_accepted() {
        // Older builds write counts as strings.
        let mut parser = ClaudeStreamParser::new();
        let events = parser.events(&line(json!({
            "type": "result",
            "usage": { "input_tokens": "42", "output_tokens": "8" }
        })));
        match &events[0] {
            ProviderEvent::Usage(usage) => {
                assert_eq!(usage.input_tokens, 42);
                assert_eq!(usage.output_tokens, 8);
                assert_eq!(usage.total_tokens, 50);
            }
            other => panic!("expected usage, got {other:?}"),
        }
    }

    #[test]
    fn a_subagents_narration_stays_out_of_the_main_reply() {
        // Frames carrying `parent_tool_use_id` belong to the Task call that
        // started the subagent. They used to fill the main answer and list the
        // subagent's calls as the turn's own.
        let mut parser = ClaudeStreamParser::new();
        let mut events = Vec::new();
        for frame in [
            json!({"type": "assistant", "parent_tool_use_id": null, "message": {"content": [
                {"type": "tool_use", "id": "task-1", "name": "Task", "input": {}}]}}),
            json!({"type": "stream_event", "parent_tool_use_id": "task-1", "event": {
                "type": "content_block_delta", "delta": {"type": "text_delta", "text": "sub says"}}}),
            json!({"type": "assistant", "parent_tool_use_id": "task-1", "message": {"content": [
                {"type": "text", "text": "sub says"},
                {"type": "tool_use", "id": "sub-tool", "name": "Grep", "input": {}}]}}),
            json!({"type": "user", "parent_tool_use_id": "task-1", "message": {"content": [
                {"type": "tool_result", "tool_use_id": "sub-tool", "content": "matches"}]}}),
            json!({"type": "user", "parent_tool_use_id": null, "message": {"content": [
                {"type": "tool_result", "tool_use_id": "task-1", "content": "summary"}]}}),
            json!({"type": "assistant", "message": {"content": [
                {"type": "text", "text": "Done."}]}}),
        ] {
            events.extend(parser.events(&line(frame)));
        }
        assert_eq!(
            events,
            vec![
                ProviderEvent::ToolUse {
                    id: "task-1".into(),
                    name: "Task".into(),
                    input: "{}".into(),
                },
                ProviderEvent::ToolResult {
                    id: "task-1".into(),
                    content: "summary".into(),
                    is_error: false,
                },
                ProviderEvent::Text("Done.".into()),
            ]
        );
        assert!(
            !parser.saw_text_delta(),
            "a subagent's delta suppressed the reply"
        );
    }

    #[test]
    fn a_steer_is_taken_when_its_own_message_starts() {
        // Captured from Claude Code 2.1.283 with a steer written while a
        // `Bash` call ran: `queued` when written, `started` after the call's
        // result and before the next request, which is where Claude read it.
        let mut parser = ClaudeStreamParser::new();
        let lifecycle = |id: &str, state: &str| {
            json!({"type": "command_lifecycle", "state": state, "command_uuid": id,
                   "uuid": "11111111-0000-4000-8000-000000000000", "session_id": "s1"})
        };
        let mut taken = Vec::new();
        for frame in [
            lifecycle("prompt-1", "queued"),
            lifecycle("prompt-1", "started"),
            json!({"type": "system", "subtype": "init"}),
            json!({"type": "system", "subtype": "status", "status": "requesting"}),
            lifecycle("steer-1", "queued"),
            json!({"type": "user", "message": {"content": [{"type": "tool_result",
                   "tool_use_id": "t1", "content": "done"}]}}),
            lifecycle("steer-1", "started"),
            json!({"type": "system", "subtype": "status", "status": "requesting"}),
            lifecycle("steer-1", "completed"),
        ] {
            taken.extend(
                parser
                    .events(&line(frame))
                    .into_iter()
                    .filter(|event| matches!(event, ProviderEvent::InputTaken { .. })),
            );
        }
        assert_eq!(
            taken,
            vec![
                ProviderEvent::InputTaken {
                    id: Some("prompt-1".into())
                },
                ProviderEvent::InputTaken {
                    id: Some("steer-1".into())
                },
            ],
            "a request said it took input the CLI names for itself"
        );
    }

    #[test]
    fn a_cli_that_names_nothing_is_taken_to_read_everything_at_its_next_request() {
        let mut parser = ClaudeStreamParser::new();
        let events = parser.events(&line(
            json!({"type": "system", "subtype": "status", "status": "requesting"}),
        ));
        assert_eq!(events, vec![ProviderEvent::InputTaken { id: None }]);
    }

    #[test]
    fn any_answering_frame_proves_the_prompt_was_read() {
        // Thinking produces no event at all, and usage and lifecycle frames are
        // not content, but each shows Claude read the prompt.
        for frame in [
            json!({"type": "system", "subtype": "status", "status": "requesting"}),
            json!({"type": "system", "subtype": "status", "status": "compacting"}),
            json!({"type": "stream_event", "event": {"type": "content_block_delta",
                   "delta": {"type": "thinking_delta", "thinking": "hmm"}}}),
            json!({"type": "result", "usage": {}}),
        ] {
            let mut parser = ClaudeStreamParser::new();
            parser.events(&line(frame.clone()));
            assert!(parser.prompt_consumed(), "{frame}");
        }
        // An answer to FlowX's own control request comes with or without one.
        let mut parser = ClaudeStreamParser::new();
        parser.events(&line(json!({"type": "control_response", "response": {}})));
        parser.events("not json");
        assert!(!parser.prompt_consumed());
    }

    /// What Claude Code 2.1.283 writes on resuming a session whose background
    /// tasks the previous process lost, before it reads the prompt: a `stopped`
    /// notification per task, then an `init` and a zero-turn `result` per batch.
    fn resumed_preamble() -> Vec<Value> {
        let ack = json!({"type": "result", "subtype": "success", "is_error": false,
            "num_turns": 0, "duration_api_ms": 0, "result": "", "stop_reason": null,
            "origin": {"kind": "task-notification"},
            "usage": {"input_tokens": 0, "output_tokens": 0}, "session_id": "s1"});
        vec![
            json!({"type": "system", "subtype": "task_notification", "task_id": "a1",
                   "status": "stopped", "session_id": "s1"}),
            json!({"type": "system", "subtype": "task_notification", "task_id": "b1",
                   "tool_use_id": "toolu_b1", "status": "stopped", "session_id": "s1"}),
            json!({"type": "system", "subtype": "init", "session_id": "s1"}),
            ack.clone(),
            json!({"type": "system", "subtype": "init", "session_id": "s1"}),
            ack,
        ]
    }

    #[test]
    fn a_resumed_sessions_lost_tasks_neither_end_the_turn_nor_read_the_prompt() {
        // The zero-turn `result` used to end the turn before Claude had read
        // the prompt, so the reply that followed it landed nowhere, and the
        // `init` before it counted as Claude having taken the prompt.
        let mut parser = ClaudeStreamParser::new();
        let mut events = Vec::new();
        for frame in resumed_preamble() {
            events.extend(parser.events(&line(frame)));
        }
        assert!(
            events
                .iter()
                .all(|event| *event == ProviderEvent::Lifecycle(Lifecycle::TurnStarted)),
            "{events:?}"
        );
        assert!(!parser.did_finish());
        assert!(!parser.prompt_consumed());
        assert!(
            !parser.has_running_tasks(),
            "a task the last process lost was counted as running"
        );

        // The real turn still ends at its own `result`.
        for frame in [
            json!({"type": "system", "subtype": "init", "session_id": "s1"}),
            json!({"type": "stream_event", "event": {"type": "content_block_delta",
                   "delta": {"type": "text_delta", "text": "the reply"}}}),
            json!({"type": "result", "subtype": "success", "num_turns": 1,
                   "usage": {"input_tokens": 3, "output_tokens": 2}}),
        ] {
            events.extend(parser.events(&line(frame)));
        }
        assert!(parser.prompt_consumed());
        assert!(events.contains(&ProviderEvent::TextDelta("the reply".into())));
        assert!(matches!(events.last(), Some(ProviderEvent::Done { .. })));
    }

    fn task_started(id: &str, background: bool) -> Value {
        json!({"type": "system", "subtype": "task_started", "task_id": id,
               "tool_use_id": format!("toolu_{id}"), "description": format!("review {id}"),
               "subagent_type": "general-purpose", "is_backgrounded": background,
               "task_type": "local_agent", "session_id": "s1"})
    }

    #[test]
    fn a_background_task_is_reported_from_start_to_finish() {
        let mut parser = ClaudeStreamParser::new();
        let started = parser.events(&line(task_started("t1", true)));
        assert_eq!(
            started,
            vec![ProviderEvent::Task(TaskUpdate {
                id: "t1".into(),
                tool_use_id: Some("toolu_t1".into()),
                description: Some("review t1".into()),
                kind: Some("local_agent".into()),
                subagent_type: Some("general-purpose".into()),
                status: TaskStatus::Running,
                ..TaskUpdate::default()
            })]
        );
        assert!(parser.has_running_tasks());

        let progress = parser.events(&line(json!({
            "type": "system", "subtype": "task_progress", "task_id": "t1",
            "tool_use_id": "toolu_t1", "description": "review t1",
            "usage": {"total_tokens": 1200, "tool_uses": 4, "duration_ms": 9000},
            "last_tool_name": "Grep", "summary": "Searching the transport"
        })));
        let [ProviderEvent::Task(progress)] = progress.as_slice() else {
            panic!("{progress:?}")
        };
        assert_eq!(progress.status, TaskStatus::Running);
        assert_eq!(progress.last_tool.as_deref(), Some("Grep"));
        assert_eq!(progress.tool_uses, Some(4));
        assert_eq!(progress.tokens, Some(1200));
        assert_eq!(progress.duration_ms, Some(9000));

        let finished = parser.events(&line(json!({
            "type": "system", "subtype": "task_notification", "task_id": "t1",
            "tool_use_id": "toolu_t1", "status": "completed", "output_file": "/tmp/t1",
            "summary": "Agent finished", "usage": {"total_tokens": 1500, "tool_uses": 6,
            "duration_ms": 12000}
        })));
        let [ProviderEvent::Task(finished)] = finished.as_slice() else {
            panic!("{finished:?}")
        };
        assert_eq!(finished.status, TaskStatus::Completed);
        assert_eq!(finished.summary.as_deref(), Some("Agent finished"));
        assert!(!parser.has_running_tasks());
        // Told once: a repeated notification is not a second ending.
        assert!(
            parser
                .events(&line(
                    json!({"type": "system", "subtype": "task_notification",
                                      "task_id": "t1", "status": "completed"})
                ))
                .is_empty()
        );
    }

    fn task_ended(id: &str) -> Value {
        json!({"type": "system", "subtype": "task_notification", "task_id": id,
               "tool_use_id": format!("toolu_{id}"), "status": "completed"})
    }

    #[test]
    fn an_ended_task_holds_the_input_open_until_its_follow_up_is_answered() {
        // The task ended before the turn's own `result`, and Claude answers
        // its notification after it. Closed at that `result`, the input could
        // not carry the answer to a permission the follow-up asks for.
        let mut parser = ClaudeStreamParser::new();
        for frame in [
            json!({"type": "system", "subtype": "init"}),
            task_started("t1", true),
            task_ended("t1"),
            json!({"type": "result", "subtype": "success", "num_turns": 1, "usage": {}}),
        ] {
            parser.events(&line(frame));
        }
        assert!(parser.did_finish());
        assert!(!parser.has_running_tasks());
        assert!(parser.holds_input_open(), "the follow-up is still owed");

        for frame in [
            json!({"type": "system", "subtype": "init"}),
            json!({"type": "result", "subtype": "success", "num_turns": 1, "usage": {},
                   "origin": {"kind": "task-notification"}}),
        ] {
            parser.events(&line(frame));
        }
        assert!(!parser.holds_input_open());
    }

    #[test]
    fn a_zero_turn_acknowledgement_after_an_answer_ends_the_wait() {
        // Claude can acknowledge a task's end without asking the model. With
        // nothing left running that is the end of the wait; ignored, it left
        // the input open and the turn spinning until Stop.
        let mut parser = ClaudeStreamParser::new();
        for frame in [
            json!({"type": "system", "subtype": "init"}),
            task_started("t1", true),
            json!({"type": "result", "subtype": "success", "num_turns": 1, "usage": {}}),
            task_ended("t1"),
            json!({"type": "system", "subtype": "init"}),
        ] {
            parser.events(&line(frame));
        }
        assert!(parser.holds_input_open());
        let events = parser.events(&line(json!({
            "type": "result", "subtype": "success", "num_turns": 0,
            "usage": {"input_tokens": 0, "output_tokens": 0},
            "origin": {"kind": "task-notification"}
        })));
        assert_eq!(
            events,
            vec![ProviderEvent::Done {
                stop_reason: "success".into()
            }],
            "no usage: the acknowledgement asked the model nothing"
        );
        assert!(parser.did_finish());
        assert!(!parser.holds_input_open());
    }

    /// What Claude Code 2.1.283 writes when a task is stopped — by TaskStop,
    /// KillShell, or a Monitor's timeout: the patch, then a `stopped`
    /// notification. A shell or monitor is marked told as it is killed, so
    /// nothing is answered after either.
    fn task_stopped(id: &str) -> [Value; 2] {
        [
            json!({"type": "system", "subtype": "task_updated", "task_id": id,
                   "patch": {"status": "killed", "end_time": 1}}),
            json!({"type": "system", "subtype": "task_notification", "task_id": id,
                   "tool_use_id": format!("toolu_{id}"), "status": "stopped",
                   "summary": format!("review {id}")}),
        ]
    }

    #[test]
    fn a_stopped_task_owes_no_answer() {
        // Every notification used to be owed an answer, so a task the agent
        // stopped itself held the input open after the turn's `result`, and
        // the turn spun until Stop recorded the finished answer as cancelled.
        let mut parser = ClaudeStreamParser::new();
        parser.events(&line(json!({"type": "system", "subtype": "init"})));
        parser.events(&line(task_started("t1", true)));
        for frame in task_stopped("t1") {
            parser.events(&line(frame));
        }
        parser.events(&line(
            json!({"type": "result", "subtype": "success", "num_turns": 3, "usage": {}}),
        ));
        assert!(parser.did_finish());
        assert!(!parser.holds_input_open(), "a stopped task was waited on");

        // And stopped between turns, the same.
        let mut parser = ClaudeStreamParser::new();
        for frame in [
            json!({"type": "system", "subtype": "init"}),
            task_started("t1", true),
            json!({"type": "result", "subtype": "success", "num_turns": 1, "usage": {}}),
        ] {
            parser.events(&line(frame));
        }
        assert!(parser.holds_input_open(), "the task is still running");
        let [patch, notification] = task_stopped("t1");
        parser.events(&line(patch));
        assert!(!parser.holds_input_open(), "the stop was waited on");
        let ended = parser.events(&line(notification));
        assert!(
            matches!(ended.as_slice(), [ProviderEvent::Task(task)] if task.status == TaskStatus::Stopped),
            "{ended:?}"
        );
        assert!(!parser.holds_input_open());
    }

    #[test]
    fn a_subagents_task_owes_the_main_session_no_answer() {
        // A task a subagent started tells that subagent when it ends. While
        // it runs it still holds the input, as any background work does.
        let mut parser = ClaudeStreamParser::new();
        let mut started = task_started("t1", true);
        started["owned_by_subagent"] = json!(true);
        for frame in [
            json!({"type": "system", "subtype": "init"}),
            started,
            json!({"type": "result", "subtype": "success", "num_turns": 1, "usage": {}}),
        ] {
            parser.events(&line(frame));
        }
        assert!(parser.holds_input_open());
        parser.events(&line(json!({"type": "system", "subtype": "task_updated",
            "task_id": "t1", "patch": {"status": "completed"}})));
        assert!(
            !parser.holds_input_open(),
            "a subagent's answer was waited on"
        );
        let ended = parser.events(&line(task_ended("t1")));
        assert!(
            matches!(ended.as_slice(), [ProviderEvent::Task(task)] if task.status == TaskStatus::Completed),
            "{ended:?}"
        );
        assert!(!parser.holds_input_open());
    }

    #[test]
    fn a_finished_task_holds_the_input_until_its_notification() {
        // The patch comes a line before the notification that owes the
        // answer. The input used to close at the patch, so the follow-up's
        // permissions failed inside Claude and a steer was refused.
        let mut parser = ClaudeStreamParser::new();
        for frame in [
            json!({"type": "system", "subtype": "init"}),
            task_started("t1", true),
            json!({"type": "result", "subtype": "success", "num_turns": 1, "usage": {}}),
            json!({"type": "system", "subtype": "task_updated", "task_id": "t1",
                   "patch": {"status": "completed", "end_time": 1}}),
        ] {
            parser.events(&line(frame));
        }
        assert!(!parser.has_running_tasks());
        assert!(parser.holds_input_open(), "the input closed at the patch");
        parser.events(&line(task_ended("t1")));
        assert!(parser.holds_input_open(), "the follow-up is owed");
        for frame in [
            json!({"type": "system", "subtype": "init"}),
            json!({"type": "result", "subtype": "success", "num_turns": 1, "usage": {},
                   "origin": {"kind": "task-notification"}}),
        ] {
            parser.events(&line(frame));
        }
        assert!(!parser.holds_input_open());
    }

    #[test]
    fn a_notification_the_turn_sends_on_is_answered_in_that_turn() {
        // A task that ends while the turn still works is told to the model
        // in the turn's next request, and no turn of its own follows. Owed
        // regardless, it held the input open after the turn's `result`.
        // Claude Code 2.1.283 gathers what is queued just before each request
        // and reports the request as `status: requesting`.
        let mut parser = ClaudeStreamParser::new();
        for frame in [
            json!({"type": "system", "subtype": "init"}),
            task_started("t1", true),
            json!({"type": "system", "subtype": "task_updated", "task_id": "t1",
                   "patch": {"status": "completed", "end_time": 1}}),
            task_ended("t1"),
            json!({"type": "system", "subtype": "status", "status": "requesting"}),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "both done"}]}}),
            json!({"type": "result", "subtype": "success", "num_turns": 2, "usage": {}}),
        ] {
            parser.events(&line(frame));
        }
        assert!(parser.did_finish());
        assert!(
            !parser.holds_input_open(),
            "an answer this turn gave was waited for"
        );
    }

    #[test]
    fn queued_user_messages_hold_the_input_until_they_are_answered() {
        // A steer that raced the `result` is still queued in Claude, which
        // answers it next as a turn of its own. The input closed at that
        // `result`, so the answer's permission requests never reached FlowX.
        let mut parser = ClaudeStreamParser::new();
        for frame in [
            json!({"type": "system", "subtype": "init"}),
            json!({"type": "result", "subtype": "success", "num_turns": 1, "usage": {},
                   "queued_turn_count": 1}),
        ] {
            parser.events(&line(frame));
        }
        assert!(parser.did_finish());
        assert!(parser.holds_input_open(), "a queued message was abandoned");
        for frame in [
            json!({"type": "system", "subtype": "init"}),
            json!({"type": "result", "subtype": "success", "num_turns": 1, "usage": {},
                   "queued_turn_count": 0}),
        ] {
            parser.events(&line(frame));
        }
        assert!(!parser.holds_input_open());
    }

    #[test]
    fn a_task_frame_is_kept_whatever_its_parent() {
        // Subagent narration is dropped by its `parent_tool_use_id`. A task
        // frame must not be: it is the main session's record of the task.
        let mut parser = ClaudeStreamParser::new();
        let mut frame = task_started("t1", true);
        frame["parent_tool_use_id"] = json!("toolu_parent");
        assert_eq!(parser.events(&line(frame)).len(), 1);
        assert!(parser.has_running_tasks());
    }

    #[test]
    fn a_foreground_call_is_not_a_background_task_until_it_moves_there() {
        let mut parser = ClaudeStreamParser::new();
        assert!(parser.events(&line(task_started("t1", false))).is_empty());
        assert!(!parser.has_running_tasks());
        let moved = parser.events(&line(json!({"type": "system", "subtype": "task_updated",
            "task_id": "t1", "patch": {"is_backgrounded": true}})));
        assert!(
            matches!(moved.as_slice(), [ProviderEvent::Task(task)] if task.description.as_deref() == Some("review t1")),
            "{moved:?}"
        );
        assert!(parser.has_running_tasks());
        // A terminal patch ends it, even before the notification arrives.
        let ended = parser.events(&line(json!({"type": "system", "subtype": "task_updated",
            "task_id": "t1", "patch": {"status": "killed"}})));
        assert!(
            matches!(ended.as_slice(), [ProviderEvent::Task(task)] if task.status == TaskStatus::Stopped)
        );
        assert!(!parser.has_running_tasks());
    }

    #[test]
    fn ambient_housekeeping_is_not_a_task() {
        let mut parser = ClaudeStreamParser::new();
        let mut frame = task_started("dream", true);
        frame["ambient"] = json!(true);
        assert!(parser.events(&line(frame)).is_empty());
        assert!(!parser.has_running_tasks());
    }

    #[test]
    fn a_follow_up_turn_reports_its_own_text_and_result() {
        // A notification is answered as a new turn. Its echo and its `result`
        // used to be swallowed as repeats of the first turn's.
        let mut parser = ClaudeStreamParser::new();
        let mut events = Vec::new();
        for frame in [
            json!({"type": "system", "subtype": "init"}),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "started"}]}}),
            json!({"type": "result", "subtype": "success", "num_turns": 2, "usage": {}}),
            json!({"type": "system", "subtype": "init"}),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "finished"}]}}),
            json!({"type": "result", "subtype": "success", "num_turns": 1,
                   "origin": {"kind": "task-notification"}, "usage": {}}),
        ] {
            events.extend(parser.events(&line(frame)));
        }
        assert!(events.contains(&ProviderEvent::Text("finished".into())));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ProviderEvent::Done { .. }))
                .count(),
            2,
            "{events:?}"
        );
    }

    #[test]
    fn huge_token_counts_saturate_rather_than_overflow() {
        let mut parser = ClaudeStreamParser::new();
        let events = parser.events(&line(json!({
            "type": "result",
            "modelUsage": {
                "a": { "inputTokens": u64::MAX, "outputTokens": 1 },
                "b": { "inputTokens": u64::MAX, "outputTokens": 1 }
            }
        })));
        match &events[0] {
            ProviderEvent::Usage(usage) => {
                assert_eq!(usage.input_tokens, u64::MAX);
                assert_eq!(usage.total_tokens, u64::MAX);
            }
            other => panic!("expected usage, got {other:?}"),
        }
        let events = ClaudeStreamParser::new().events(&line(json!({
            "type": "result",
            "usage": { "input_tokens": u64::MAX, "output_tokens": 5 }
        })));
        assert!(
            matches!(&events[0], ProviderEvent::Usage(usage) if usage.total_tokens == u64::MAX)
        );
    }

    #[test]
    fn read_only_tools_are_named_by_claude_and_bash_is_judged_by_its_command() {
        for tool in ["Read", "Grep", "Glob", "LS"] {
            assert!(is_read_only_tool(tool, &json!({})), "{tool}");
        }
        assert!(is_read_only_tool("Bash", &json!({ "command": "ls -la" })));
        for (tool, input) in [
            ("Bash", json!({ "command": "find . -delete" })),
            ("Bash", json!({})),
            ("Edit", json!({})),
            ("Write", json!({})),
            ("WebFetch", json!({})),
        ] {
            assert!(!is_read_only_tool(tool, &input), "{tool} {input}");
        }
    }

    #[test]
    fn a_whole_turn_produces_one_message_and_one_completion() {
        // The end-to-end shape, which is what the transcript depends on.
        let mut parser = ClaudeStreamParser::new();
        let mut all = Vec::new();
        for frame in [
            json!({"type": "system", "subtype": "init"}),
            json!({"type": "stream_event",
                   "event": {"type": "content_block_delta",
                             "delta": {"type": "text_delta", "text": "hi"}} }),
            json!({"type": "assistant",
            "message": {"content": [
                {"type": "text", "text": "hi"},
                {"type": "tool_use", "id": "t1", "name": "Read", "input": {}}
            ]}}),
            json!({"type": "user",
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
            ]}}),
            json!({"type": "result", "subtype": "end_turn",
                   "usage": {"input_tokens": 1, "output_tokens": 1}}),
        ] {
            all.extend(parser.events(&line(frame)));
        }

        assert_eq!(
            all.iter()
                .filter(|e| matches!(e, ProviderEvent::TextDelta(_)))
                .count(),
            1
        );
        assert_eq!(
            all.iter()
                .filter(|e| matches!(e, ProviderEvent::ToolUse { .. }))
                .count(),
            1
        );
        assert_eq!(
            all.iter()
                .filter(|e| matches!(e, ProviderEvent::ToolResult { .. }))
                .count(),
            1
        );
        assert_eq!(
            all.iter()
                .filter(|e| matches!(e, ProviderEvent::Done { .. }))
                .count(),
            1
        );
    }
}
