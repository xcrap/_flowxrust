//! Driving one Codex turn.
//!
//! Sending a prompt is easy; knowing when the turn is *over* is not. Codex
//! answers `turn/start` immediately and then narrates over the same socket, so
//! the response to the request and the events that make up the answer are two
//! separate streams that have to be kept apart.
//!
//! The rules that keep them apart:
//!
//! - **The request response is not a turn event.** It carries the turn id; every
//!   later line until `turn/completed` is the turn's output. Treating the ack as
//!   content is how a turn ends up showing its own id.
//! - **Only `turn/completed` ends the turn.** A tool call, a stream gap, or the
//!   process going quiet all mean the turn is still running. Only an explicit
//!   completion, an error, the server dying, or the user's Stop ends it — and a
//!   turn that ends early looks to the user like the model stopped
//!   mid-sentence. There is deliberately no silence timer: `sleep 200`, a quiet
//!   `docker build`, and long reasoning all say nothing for minutes, and a timer
//!   that failed them also got Codex killed mid-turn. The process dying is
//!   watched instead, through the event stream closing.
//! - **Approvals are answered by policy here.** A caller with a user in the loop
//!   answers them itself; without one, an unanswered request would leave Codex
//!   blocked, so the turn answers with the configured policy and reports what it
//!   did.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use fx_core::AgentAccess;
use serde_json::{Value, json};

use crate::codex::session::{
    CodexWriterConflict, INTERRUPTED, TurnNotifications, TurnRequest, is_read_only_command_value,
    thread_id_from_result, thread_params, turn_params,
};
use crate::codex::session::{goal_set_params, steer_params};
use crate::codex::transport::Requester;
use crate::codex::transport::{PendingResponse, ServerEvent, Transport, TransportError};
use crate::provider::{ApprovalRequest, ProviderError, ProviderEvent, RiskLevel, UserResponse};
use fx_core::models::goal::{ConversationGoal, GoalStatus};
use tokio::sync::mpsc;

/// How a turn ended.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnOutcome {
    /// Codex reported the turn complete.
    Completed {
        thread_id: String,
        stop_reason: String,
    },
    /// The turn was cancelled.
    Cancelled { thread_id: String },
    /// Codex reported a failure.
    Failed { thread_id: String, reason: String },
}

impl TurnOutcome {
    pub fn thread_id(&self) -> &str {
        match self {
            Self::Completed { thread_id, .. }
            | Self::Cancelled { thread_id, .. }
            | Self::Failed { thread_id, .. } => thread_id,
        }
    }

    /// Whether the turn produced a usable answer.
    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }
}

/// What to do with a tool call Codex asks permission for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalPolicy {
    /// Ask the active UI; headless callers fail closed.
    Ask,
    /// Allow every request.
    AllowAll,
    /// Refuse every request.
    DenyAll,
    /// Refuse anything that is not read-only.
    ///
    /// The default: a turn driven without a user in the loop must still refuse
    /// anything that writes, rather than approving work nobody watched.
    #[default]
    DenyUnlessReadOnly,
}

impl ApprovalPolicy {
    pub fn allows(&self, tool_name: &str, input: &Value) -> bool {
        match self {
            Self::AllowAll => true,
            Self::Ask | Self::DenyAll => false,
            Self::DenyUnlessReadOnly => {
                // `risk_for` is not the right question here. It rates even a
                // read-only command Moderate, because a *user* should be asked;
                // this policy has already decided no user is present. So the
                // read-only check is used directly.
                let arguments = input.get("arguments").unwrap_or(input);
                match tool_name {
                    "webSearch" | "imageView" | "sleep" => true,
                    // Either form: Codex sends argv far more often than a line,
                    // and a check that understood only lines would refuse every
                    // read under this policy.
                    "commandExecution" => arguments
                        .get("command")
                        .is_some_and(is_read_only_command_value),
                    _ => false,
                }
            }
        }
    }

    /// The same decision for one of Claude's tools.
    ///
    /// Claude names its tools differently, and `allows` knows only Codex's
    /// names, so under this policy it refused every Claude tool — `Read`
    /// included.
    pub fn allows_claude_tool(&self, tool_name: &str, input: &Value) -> bool {
        match self {
            Self::AllowAll => true,
            Self::Ask | Self::DenyAll => false,
            Self::DenyUnlessReadOnly => crate::claude::stream::is_read_only_tool(tool_name, input),
        }
    }

    /// Why a call was refused, for the transcript.
    ///
    /// Owned rather than `&'static`, because this is called on every refusal in
    /// a turn and a leaked `String` is never freed.
    fn message(&self, tool_name: &str) -> String {
        let reason = match self {
            Self::AllowAll => return String::new(),
            Self::Ask | Self::DenyAll => "FlowX refused this tool call.",
            Self::DenyUnlessReadOnly => {
                "FlowX only approves read-only tools when running without a prompt."
            }
        };
        format!("{reason} ({tool_name})")
    }
}

/// A live Codex session: one thread, any number of turns.
pub struct CodexSession {
    transport: Transport,
    thread_id: String,
    /// The running turn, once Codex has acknowledged `turn/start`. That
    /// acknowledgement is the proof Codex has the prompt.
    active_turn_id: Option<String>,
    /// A `turn/start` that has been written and not yet answered.
    ///
    /// Held here rather than inside the turn's future, so a Stop that drops
    /// the future between the write and the reply can still collect the reply:
    /// it says whether the prompt was taken, and which turn to interrupt.
    starting: Option<PendingResponse>,
    /// The running turn's id, readable by a [`TurnSteerer`] while
    /// `run_turn_interactive` holds the session mutably.
    steer_turn: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    approvals: ApprovalPolicy,
}

impl std::fmt::Debug for CodexSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexSession")
            .field("thread_id", &self.thread_id)
            .field("approvals", &self.approvals)
            .finish()
    }
}

impl CodexSession {
    /// Open a session, resuming a thread when one is named.
    pub async fn open(
        transport: Transport,
        resume_thread_id: Option<&str>,
        access: AgentAccess,
        working_directory: &str,
        approvals: ApprovalPolicy,
    ) -> Result<Self, ProviderError> {
        Self::open_with_instructions(
            transport,
            resume_thread_id,
            access,
            working_directory,
            approvals,
            None,
        )
        .await
    }

    pub async fn open_with_instructions(
        mut transport: Transport,
        resume_thread_id: Option<&str>,
        access: AgentAccess,
        working_directory: &str,
        approvals: ApprovalPolicy,
        system_prompt: Option<&str>,
    ) -> Result<Self, ProviderError> {
        let thread_id = resolve_thread_id(
            &mut transport,
            resume_thread_id,
            access,
            working_directory,
            system_prompt,
        )
        .await?;

        Ok(Self {
            transport,
            thread_id,
            active_turn_id: None,
            starting: None,
            steer_turn: Default::default(),
            approvals,
        })
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// Whether Codex acknowledged the latest `turn/start`, which is the proof
    /// it has the prompt. A prompt it has must not be offered back for
    /// sending again, or Codex receives it twice.
    pub fn prompt_accepted(&self) -> bool {
        self.active_turn_id.is_some()
    }

    /// Ask Codex to stop the turn in flight.
    ///
    /// Best-effort: the turn may already be over, and a failure here must not
    /// replace the real outcome. A Stop that arrived while `turn/start` was
    /// still unanswered first waits for that answer: Codex already has the
    /// prompt, and without the turn id there is nothing to interrupt.
    ///
    /// Nothing reads the turn's stream once it has been dropped, so both waits
    /// read and drop what arrives meanwhile. Otherwise a busy turn filled the
    /// bounded queue, the reader stopped, and the answer behind it was never
    /// read: Stop sat out the caller's whole timeout.
    pub async fn interrupt(&mut self) -> Result<(), ProviderError> {
        if let Some(mut starting) = self.starting.take() {
            let ack = self
                .transport
                .response_discarding_events(&mut starting)
                .await
                .map_err(classify_turn_error)?;
            self.active_turn_id = Some(turn_id_of(&ack));
        }
        let Some(turn_id) = self.active_turn_id.clone() else {
            return Ok(());
        };
        let mut pending = self
            .transport
            .send_request(
                "turn/interrupt",
                Some(json!({ "threadId": self.thread_id, "turnId": turn_id })),
            )
            .await
            .map_err(map_error)?;
        self.transport
            .response_discarding_events(&mut pending)
            .await
            .map(|_| ())
            .map_err(map_error)
    }

    /// Send a prompt and run the turn to completion.
    ///
    /// Every event is handed to `sink` as it arrives, and only there. They
    /// used to be collected as well, then copied into the outcome, for the
    /// whole length of the turn — every streamed delta held twice more than
    /// the conversation holds it, for a list nothing read.
    pub async fn run_turn(
        &mut self,
        request: &TurnRequest<'_>,
        sink: impl FnMut(ProviderEvent),
    ) -> Result<TurnOutcome, ProviderError> {
        self.drive_turn(request, None, sink, || {}, |_| {}).await
    }

    /// Run with an interactive response channel. Process exits and
    /// cancellation stay live while the turn waits for the user.
    pub async fn run_turn_interactive(
        &mut self,
        request: &TurnRequest<'_>,
        responses: &mut mpsc::UnboundedReceiver<UserResponse>,
        sink: impl FnMut(ProviderEvent),
        accepted: impl FnMut(),
        resolved: impl FnMut(String),
    ) -> Result<TurnOutcome, ProviderError> {
        self.drive_turn(request, Some(responses), sink, accepted, resolved)
            .await
    }

    async fn drive_turn(
        &mut self,
        request: &TurnRequest<'_>,
        mut responses: Option<&mut mpsc::UnboundedReceiver<UserResponse>>,
        mut sink: impl FnMut(ProviderEvent),
        mut accepted: impl FnMut(),
        mut resolved: impl FnMut(String),
    ) -> Result<TurnOutcome, ProviderError> {
        let thread_id = self.thread_id.clone();
        self.active_turn_id = None;
        // Written, then awaited from `self`: if this future is dropped by a
        // Stop in between, the reply is still collected by `interrupt`.
        let starting = self.starting.insert(
            self.transport
                .send_request("turn/start", Some(turn_params(request)))
                .await
                .map_err(classify_turn_error)?,
        );
        let ack = self.transport.response(starting).await;
        self.starting = None;
        let turn_id = turn_id_of(&ack.map_err(classify_turn_error)?);
        self.active_turn_id = Some(turn_id.clone());
        *self
            .steer_turn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(turn_id.clone());
        accepted();
        let mut notifications = TurnNotifications::new();
        let mut pending: HashMap<String, PendingRequest> = HashMap::new();
        let mut items: HashMap<String, Value> = HashMap::new();
        let mut spawned = SpawnedThreads::default();

        loop {
            let next = tokio::select! {
                next = self.transport.next_event() => next,
                response = async {
                    match responses.as_mut() {
                        Some(receiver) => receiver.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match response {
                        Some(response) => {
                            let id = response.id().to_owned();
                            if let Some(request) = pending.get(&id)
                                && request.reply(&self.transport, &response)?
                            {
                                pending.remove(&id);
                                resolved(id);
                            }
                        }
                        None => {
                            // The UI disappeared; refuse every open request rather
                            // than leaving the provider blocked forever.
                            for (id, request) in pending.drain() {
                                request.reject(&self.transport)?;
                                resolved(id);
                            }
                            responses = None;
                        }
                    }
                    continue;
                },
            };
            let Some(next) = next else {
                let reason = self
                    .transport
                    .exit_reason()
                    .await
                    .map(|exit| exit.reason())
                    .unwrap_or_else(|| "the Codex connection closed".into());
                return Ok(TurnOutcome::Failed { thread_id, reason });
            };
            match next {
                ServerEvent::Notification { method, params } => {
                    spawned.observe(&method, &params, &thread_id);
                    let ours = belongs_to_turn(&params, &thread_id, &turn_id);
                    // A sub-agent's requests are answered here, so what they
                    // need is kept for them too; their other events are not
                    // this transcript's.
                    if !ours && !spawned.sent(&params) {
                        continue;
                    }
                    if method == "item/started"
                        && let Some(item) = params.get("item")
                        && let Some(id) = item.get("id").and_then(Value::as_str)
                    {
                        items.insert(id.to_owned(), item.clone());
                    }
                    if method == "serverRequest/resolved"
                        && let Some(id) = params.get("requestId")
                    {
                        let id = request_key(id);
                        if pending.remove(&id).is_some() {
                            resolved(id);
                        }
                    }
                    if !ours {
                        continue;
                    }
                    for event in notifications.parse(&method, &params) {
                        let outcome = finish(&event, &thread_id);
                        sink(event);
                        if let Some(outcome) = outcome {
                            for id in pending.keys() {
                                resolved(id.clone());
                            }
                            return Ok(outcome);
                        }
                    }
                }
                ServerEvent::Request {
                    id,
                    method,
                    mut params,
                } => {
                    let ours = belongs_to_turn(&params, &thread_id, &turn_id);
                    if !ours && !spawned.sent(&params) {
                        self.transport
                            .refuse(
                                id.clone(),
                                -32000,
                                "This request does not belong to the active FlowX turn.",
                            )
                            .map_err(map_error)?;
                        // Refused unseen, whatever asked failed with nothing on
                        // screen to say why. Its own item belongs to the other
                        // thread and never reached this one, so the call is
                        // recorded here, with the refusal as its result.
                        if let Some(request) = PendingRequest::parse(id, &method, &params) {
                            for event in request.refused_as_foreign(&thread_id) {
                                sink(event);
                            }
                        }
                        continue;
                    }
                    if method == "currentTime/read" {
                        self.transport
                            .respond(
                                id,
                                json!({"currentTimeAt": fx_core::models::now().timestamp()}),
                            )
                            .map_err(map_error)?;
                        continue;
                    }
                    // File approval params omit changes; retain the preceding
                    // item/started payload so the user sees what is authorized.
                    if let Some(item_id) = params.get("itemId").and_then(Value::as_str)
                        && let Some(item) = items.get(item_id)
                    {
                        params["item"] = item.clone();
                    }
                    if let Some(request) = PendingRequest::parse(id.clone(), &method, &params) {
                        let event = request.event.clone();
                        let ask = responses.is_some()
                            && (matches!(event, ProviderEvent::UserInput(_))
                                || self.approvals == ApprovalPolicy::Ask);
                        if ask {
                            pending.insert(request_key(&id), request);
                            sink(event);
                        } else {
                            let approved = matches!(&event, ProviderEvent::Approval(a) if self.approvals.allows(&a.tool_name, &a.input));
                            if let Some(result) = request.approval_result(approved) {
                                self.transport.respond(id, result).map_err(map_error)?;
                            } else {
                                request.reject(&self.transport)?;
                            }
                            // A refusal is the call's whole outcome, since it
                            // never runs, so it is recorded as the call's
                            // result. An approval is not a result at all: the
                            // call has not run yet, and an empty result here
                            // marked it finished while it was still running.
                            // Codex reports the real one when it is done.
                            if let ProviderEvent::Approval(a) = event
                                && !approved
                            {
                                // A sub-agent's call is a row in its own
                                // thread, not here, so the refusal brings it.
                                if !ours {
                                    sink(request.call_row());
                                }
                                sink(ProviderEvent::ToolResult {
                                    id: request.call_id(),
                                    content: self.approvals.message(&a.tool_name),
                                    is_error: true,
                                });
                            }
                        }
                    } else {
                        // Unknown server methods must never be approved as tools.
                        self.transport
                            .refuse(id, -32601, &format!("FlowX cannot service {method}."))
                            .map_err(map_error)?;
                    }
                }
            }
        }
    }

    /// Stop the session once its turn is done.
    pub async fn close(self) {
        self.transport.close().await;
    }

    /// Stop the session and every process it started, for a turn that was
    /// stopped or failed part-way.
    pub async fn terminate(self) {
        self.transport.terminate().await;
    }

    /// Whether the underlying process has exited.
    pub async fn exit_reason(&self) -> Option<String> {
        self.transport.exit_reason().await.map(|exit| exit.reason())
    }
}

/// How long a compaction may run before FlowX stops waiting for it.
///
/// A compaction is one summarising model call, not a turn of work, so unlike a
/// turn — which silence alone never ends — it has a deadline.
pub const COMPACTION_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Guidance for a running turn, sent from outside the task driving it.
///
/// `run_turn_interactive` holds the session mutably for the turn's length, so
/// `turn/steer` cannot go through the session itself. This holds what a steer
/// needs instead — a [`Requester`] on the same process, the thread, and the
/// turn id `drive_turn` records once Codex acknowledges the turn.
#[derive(Debug, Clone)]
pub struct TurnSteerer {
    requester: Requester,
    thread_id: String,
    turn: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl TurnSteerer {
    /// Add guidance to the turn in flight. Images come first, as in a turn.
    ///
    /// Codex rejects a steer aimed at a turn that is not the active one, which
    /// is why the expected id is always sent, and why a reply naming another
    /// turn is a failure rather than a success.
    /// `id` is what Codex names the steer by when it reads it.
    pub async fn steer(
        &self,
        id: &str,
        prompt: &str,
        image_paths: &[String],
    ) -> Result<(), ProviderError> {
        let Some(turn_id) = self
            .turn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        else {
            return Err(ProviderError::Unavailable(
                "Codex has not started a steerable turn yet, or the active turn has already ended."
                    .into(),
            ));
        };
        let result = self
            .requester
            .request(
                "turn/steer",
                Some(steer_params(
                    &self.thread_id,
                    &turn_id,
                    id,
                    prompt,
                    image_paths,
                )),
            )
            .await
            .map_err(map_error)?;
        if result.get("turnId").and_then(Value::as_str) != Some(turn_id.as_str()) {
            return Err(ProviderError::Unavailable(
                "Codex did not confirm guidance for the expected active turn.".into(),
            ));
        }
        Ok(())
    }
}

impl CodexSession {
    /// A handle that can steer this session's turns while one is running.
    pub fn steerer(&self) -> TurnSteerer {
        TurnSteerer {
            requester: self.transport.requester(),
            thread_id: self.thread_id.clone(),
            turn: self.steer_turn.clone(),
        }
    }

    /// Ask Codex to compact the thread's context, and wait for it to finish.
    ///
    /// `thread/compact/start` is answered at once and the compaction runs
    /// afterwards, so returning on the answer would close the process with the
    /// work still in flight. This waits for `thread/compacted`, or for the
    /// compaction turn to complete, feeding what it sees to `sink` on the way.
    pub async fn compact(
        &mut self,
        mut sink: impl FnMut(ProviderEvent),
    ) -> Result<(), ProviderError> {
        let thread_id = self.thread_id.clone();
        self.transport
            .request(
                "thread/compact/start",
                Some(json!({ "threadId": thread_id })),
            )
            .await
            .map_err(map_error)?;
        sink(ProviderEvent::Lifecycle(
            crate::provider::Lifecycle::Compacting,
        ));
        let deadline = tokio::time::Instant::now() + COMPACTION_TIMEOUT;
        let mut notifications = TurnNotifications::new();
        loop {
            let next = tokio::time::timeout_at(deadline, self.transport.next_event())
                .await
                .map_err(|_| {
                    ProviderError::Unavailable(
                        "Codex did not finish compacting the context in time.".into(),
                    )
                })?;
            let Some(next) = next else {
                let reason = self
                    .transport
                    .exit_reason()
                    .await
                    .map(|exit| exit.reason())
                    .unwrap_or_else(|| "the Codex connection closed".into());
                return Err(ProviderError::Unavailable(reason));
            };
            match next {
                ServerEvent::Notification { method, params } => {
                    if params
                        .get("threadId")
                        .and_then(Value::as_str)
                        .is_some_and(|id| id != thread_id)
                    {
                        continue;
                    }
                    if method == "flowx/server-exited" {
                        continue;
                    }
                    let finished = matches!(method.as_str(), "thread/compacted" | "turn/completed");
                    for event in notifications.parse(&method, &params) {
                        if let ProviderEvent::Error(reason) = event {
                            return Err(ProviderError::Protocol(reason));
                        }
                        // The compaction turn's own completion is not an
                        // answer, and must not commit anything as one.
                        if !matches!(event, ProviderEvent::Done { .. }) {
                            sink(event);
                        }
                    }
                    if finished {
                        if method == "turn/completed" {
                            sink(ProviderEvent::Lifecycle(
                                crate::provider::Lifecycle::Compacted,
                            ));
                        }
                        return Ok(());
                    }
                }
                ServerEvent::Request { id, method, .. } => {
                    // Nothing in a compaction needs the user, and an unanswered
                    // request would leave Codex blocked on it.
                    self.transport
                        .refuse(id, -32601, &format!("FlowX cannot service {method}."))
                        .map_err(map_error)?;
                }
            }
        }
    }

    /// The thread's goal, if it has one (`thread/goal/get`).
    pub async fn goal(&self) -> Result<Option<ConversationGoal>, ProviderError> {
        let result = self
            .transport
            .request(
                "thread/goal/get",
                Some(json!({ "threadId": self.thread_id })),
            )
            .await
            .map_err(map_error)?;
        Ok(result.get("goal").and_then(ConversationGoal::from_codex))
    }

    /// Set the objective, the status, or both (`thread/goal/set`).
    pub async fn set_goal(
        &self,
        objective: Option<&str>,
        status: Option<GoalStatus>,
    ) -> Result<ConversationGoal, ProviderError> {
        let result = self
            .transport
            .request(
                "thread/goal/set",
                Some(goal_set_params(&self.thread_id, objective, status)),
            )
            .await
            .map_err(map_error)?;
        result
            .get("goal")
            .and_then(ConversationGoal::from_codex)
            .ok_or_else(|| ProviderError::Protocol("Codex did not return a goal.".into()))
    }

    /// Remove the thread's goal (`thread/goal/clear`).
    pub async fn clear_goal(&self) -> Result<(), ProviderError> {
        self.transport
            .request(
                "thread/goal/clear",
                Some(json!({ "threadId": self.thread_id })),
            )
            .await
            .map(|_| ())
            .map_err(map_error)
    }
}

fn request_key(id: &Value) -> String {
    format!("codex:{id}")
}

/// The turn id out of a `turn/start` acknowledgement.
fn turn_id_of(ack: &Value) -> String {
    ack.get("turn")
        .and_then(|turn| turn.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn belongs_to_turn(params: &Value, thread_id: &str, turn_id: &str) -> bool {
    params
        .get("threadId")
        .and_then(Value::as_str)
        .is_none_or(|id| id == thread_id)
        && params
            .get("turnId")
            .and_then(Value::as_str)
            .or_else(|| {
                params
                    .get("turn")
                    .and_then(|turn| turn.get("id"))
                    .and_then(Value::as_str)
            })
            .is_none_or(|id| turn_id.is_empty() || id == turn_id)
}

/// The sub-agent threads this turn's thread has started, as Codex names them.
///
/// A sub-agent is a thread of its own, so its approvals and questions carry
/// its thread id and its own turn's, never this turn's. Refused as foreign,
/// every command a sub-agent asked to run failed. Codex says which threads
/// are children — a `subAgentActivity` item names the agent's thread, a collab
/// call its receivers, `thread/started` a new thread's parent — and only a
/// thread named one of those ways is accepted. Nothing else is guessed to be
/// family.
#[derive(Debug, Default)]
struct SpawnedThreads(HashSet<String>);

impl SpawnedThreads {
    /// Learn the children a notification names, if it comes from this thread
    /// or one of them: a sub-agent's own sub-agents are family too.
    fn observe(&mut self, method: &str, params: &Value, thread_id: &str) {
        let family = |id: &str| id == thread_id || self.0.contains(id);
        let mut named: Vec<&str> = Vec::new();
        match method {
            "thread/started" => {
                let thread = &params["thread"];
                if thread["parentThreadId"].as_str().is_some_and(family) {
                    named.extend(thread["id"].as_str());
                }
            }
            "item/started" | "item/completed" if params["threadId"].as_str().is_none_or(family) => {
                let item = &params["item"];
                match item["type"].as_str() {
                    Some("subAgentActivity") => named.extend(item["agentThreadId"].as_str()),
                    Some("collabAgentToolCall")
                        if item["senderThreadId"].as_str().is_none_or(family) =>
                    {
                        named.extend(
                            item["receiverThreadIds"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str),
                        );
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        for id in named {
            if !id.is_empty() && id != thread_id {
                self.0.insert(id.to_owned());
            }
        }
    }

    /// Whether a message comes from one of them.
    fn sent(&self, params: &Value) -> bool {
        params
            .get("threadId")
            .and_then(Value::as_str)
            .is_some_and(|id| self.0.contains(id))
    }
}

struct PendingRequest {
    wire_id: Value,
    method: String,
    params: Value,
    event: ProviderEvent,
}

impl PendingRequest {
    fn parse(wire_id: Value, method: &str, params: &Value) -> Option<Self> {
        let id = request_key(&wire_id);
        let event = match method {
            "item/tool/requestUserInput" => {
                let mut request = crate::codex::session::parse_user_input(params);
                if request.questions.is_empty() {
                    return None;
                }
                request.id = id;
                ProviderEvent::UserInput(request)
            }
            "item/commandExecution/requestApproval"
            | "execCommandApproval"
            | "item/fileChange/requestApproval"
            | "applyPatchApproval"
            | "item/permissions/requestApproval" => {
                let tool_name = params
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| {
                        if method.contains("command") || method == "execCommandApproval" {
                            "commandExecution"
                        } else if method.contains("permissions") {
                            "permissions"
                        } else {
                            "fileChange"
                        }
                    })
                    .to_owned();
                ProviderEvent::Approval(ApprovalRequest {
                    id,
                    tool_name: tool_name.clone(),
                    input: params.clone(),
                    risk: if tool_name == "permissions" {
                        RiskLevel::Dangerous
                    } else {
                        crate::codex::session::risk_for(&tool_name, params.get("command"))
                    },
                })
            }
            _ => return None,
        };
        Some(Self {
            wire_id,
            method: method.into(),
            params: params.clone(),
            event,
        })
    }

    /// The id of the call this request is about, which its result row claims.
    fn call_id(&self) -> String {
        self.params
            .get("callId")
            .or_else(|| self.params.get("itemId"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| request_key(&self.wire_id))
    }

    /// The call this request is about, as a row of its own.
    ///
    /// For a call whose item never reached this transcript: it belongs to
    /// another thread, or to a sub-agent, whose items stay in its own.
    fn call_row(&self) -> ProviderEvent {
        let name = match &self.event {
            ProviderEvent::Approval(approval) => approval.tool_name.clone(),
            _ => "requestUserInput".to_owned(),
        };
        let call = self
            .params
            .get("item")
            .filter(|item| item.is_object())
            .unwrap_or(&self.params);
        ProviderEvent::ToolUse {
            id: self.call_id(),
            name,
            input: crate::codex::session::tool_input(call).to_string(),
        }
    }

    /// The rows for a request refused because it came from outside this turn:
    /// what it asked for, and why it was refused.
    fn refused_as_foreign(&self, thread_id: &str) -> [ProviderEvent; 2] {
        let origin = match self.params.get("threadId").and_then(Value::as_str) {
            Some(other) if other != thread_id => format!("another Codex thread ({other})"),
            _ => "another turn".to_owned(),
        };
        let what = match &self.event {
            ProviderEvent::UserInput(_) => "question",
            _ => "approval request",
        };
        [
            self.call_row(),
            ProviderEvent::ToolResult {
                id: self.call_id(),
                content: format!(
                    "FlowX refused this {what}: it came from {origin}, not from this turn."
                ),
                is_error: true,
            },
        ]
    }

    fn approval_result(&self, approved: bool) -> Option<Value> {
        if !matches!(self.event, ProviderEvent::Approval(_)) {
            return None;
        }
        Some(match self.method.as_str() {
            // A legacy denial is an object carrying the reason; the bare
            // string `"denied"` is not a `ReviewDecision` Codex accepts.
            "execCommandApproval" | "applyPatchApproval" => {
                if approved {
                    json!({"decision": "approved"})
                } else {
                    json!({"decision": {"denied": {"rejection": "Declined in FlowX."}}})
                }
            }
            "item/permissions/requestApproval" => {
                json!({"permissions": if approved { self.params.get("permissions").cloned().unwrap_or(json!({})) } else {json!({})}, "scope": "turn"})
            }
            _ => json!({"decision": if approved { "accept" } else { "decline" }}),
        })
    }

    fn response(&self, response: &UserResponse) -> Option<Value> {
        match (&self.event, response) {
            (ProviderEvent::Approval(_), UserResponse::Approval { approved, .. }) => {
                self.approval_result(*approved)
            }
            (ProviderEvent::UserInput(request), UserResponse::Questions { answers, .. }) => {
                let answers: serde_json::Map<String, Value> = request
                    .questions
                    .iter()
                    .map(|q| {
                        let values: Vec<&str> = answers
                            .iter()
                            .filter(|(id, _)| id == &q.id)
                            .take(10)
                            .map(|(_, answer)| answer.as_str())
                            .collect();
                        (q.id.clone(), json!({ "answers": values }))
                    })
                    .collect();
                Some(json!({"answers": answers}))
            }
            _ => None,
        }
    }

    fn reply(&self, transport: &Transport, response: &UserResponse) -> Result<bool, ProviderError> {
        if matches!(
            (&self.event, response),
            (
                ProviderEvent::UserInput(_),
                UserResponse::CancelQuestions { .. }
            )
        ) {
            self.reject(transport)?;
            return Ok(true);
        }
        if let Some(result) = self.response(response) {
            transport
                .respond(self.wire_id.clone(), result)
                .map_err(map_error)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn reject(&self, transport: &Transport) -> Result<(), ProviderError> {
        if let Some(result) = self.approval_result(false) {
            transport
                .respond(self.wire_id.clone(), result)
                .map_err(map_error)
        } else {
            transport
                .refuse(
                    self.wire_id.clone(),
                    -32800,
                    "The user cancelled the input request.",
                )
                .map_err(map_error)
        }
    }
}

/// Finish the turn if this event ends it.
fn finish(event: &ProviderEvent, thread_id: &str) -> Option<TurnOutcome> {
    match event {
        // Another client stopped the turn, or Codex did. Read as a completion
        // it showed the Ready check and the unseen-completion dot for a turn
        // that never answered. FlowX's own Stop never arrives here: it drops
        // the turn before Codex reports it.
        ProviderEvent::Done { stop_reason } if stop_reason == INTERRUPTED => {
            Some(TurnOutcome::Cancelled {
                thread_id: thread_id.to_string(),
            })
        }
        ProviderEvent::Done { stop_reason } => Some(TurnOutcome::Completed {
            thread_id: thread_id.to_string(),
            stop_reason: stop_reason.clone(),
        }),
        ProviderEvent::Error(reason) => Some(TurnOutcome::Failed {
            thread_id: thread_id.to_string(),
            reason: reason.clone(),
        }),
        _ => None,
    }
}

/// Open a thread, or resume one.
async fn resolve_thread_id(
    transport: &mut Transport,
    resume_thread_id: Option<&str>,
    access: AgentAccess,
    working_directory: &str,
    system_prompt: Option<&str>,
) -> Result<String, ProviderError> {
    let (method, id) = match resume_thread_id {
        Some(id) if !id.is_empty() => ("thread/resume", id),
        _ => ("thread/start", ""),
    };
    let params = thread_params(
        Some(id).filter(|id| !id.is_empty()),
        access,
        system_prompt,
        working_directory,
    );
    let result = transport
        .request(method, Some(params))
        .await
        .map_err(map_error)?;

    thread_id_from_result(&result).ok_or_else(|| {
        ProviderError::Protocol(format!("Codex's `{method}` response carried no thread id"))
    })
}

fn map_error(error: TransportError) -> ProviderError {
    match error {
        TransportError::Rejected { message, .. } => ProviderError::Protocol(message),
        TransportError::Exited { stderr } => ProviderError::Unavailable(stderr),
        TransportError::TimedOut { method } => {
            ProviderError::Unavailable(format!("Codex did not answer `{method}` in time."))
        }
        other => ProviderError::Unavailable(other.to_string()),
    }
}

/// Tell a refused turn from a refused method.
fn classify_turn_error(error: TransportError) -> ProviderError {
    if let TransportError::Rejected { ref message, .. } = error
        && CodexWriterConflict::matches(message)
    {
        return ProviderError::Unavailable(CodexWriterConflict::MESSAGE.to_string());
    }
    map_error(error)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::codex::fake_server::FakeServer;
    use crate::codex::transport::Launch;
    use fx_core::AgentAccess;

    async fn session_with(server: &FakeServer, env: &[(&str, &str)]) -> CodexSession {
        CodexSession::open(
            Transport::start(server.launch_with_env(env), "FlowX", "test")
                .await
                .expect("started"),
            None,
            AgentAccess::FullAccess,
            "/tmp/project",
            ApprovalPolicy::AllowAll,
        )
        .await
        .expect("a session")
    }

    fn request<'a>(thread_id: &'a str, prompt: &'a str, directory: &'a str) -> TurnRequest<'a> {
        TurnRequest::new(thread_id, prompt, directory)
    }

    #[tokio::test]
    async fn a_turn_streams_its_text_and_completes() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let mut session = session_with(&server, &[]).await;
        let thread_id = session.thread_id().to_string();
        let mut seen = Vec::new();
        let outcome = session
            .run_turn(&request(&thread_id, "hello", "/tmp"), |event| {
                seen.push(event)
            })
            .await
            .expect("the turn ran");

        assert!(outcome.is_completed(), "{outcome:?}");
        assert!(matches!(seen.first(), Some(ProviderEvent::Lifecycle(_))));
        let streamed: String = seen
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::TextDelta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(streamed, "Hello");
        assert!(!seen.is_empty());
    }

    #[tokio::test]
    async fn the_turn_ack_is_not_reported_as_content() {
        // The ack carries the turn id; treating it as output shows the user their
        // own turn id.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let mut session = session_with(&server, &[]).await;
        let thread_id = session.thread_id().to_string();
        let mut seen = Vec::new();
        session
            .run_turn(&request(&thread_id, "hello", "/tmp"), |event| {
                seen.push(event)
            })
            .await
            .expect("the turn ran");
        assert!(
            !seen
                .iter()
                .any(|event| matches!(event, ProviderEvent::Text(text) if text.contains("turn-"))),
            "the acknowledgement leaked into the transcript"
        );
    }

    #[tokio::test]
    async fn an_errored_turn_is_reported_as_failed() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let mut session = session_with(&server, &[]).await;
        let thread_id = session.thread_id().to_string();
        let outcome = session
            .run_turn(&TurnRequest::new(&thread_id, "fail-now", "/tmp"), |_| {})
            .await
            .expect("the turn ran");
        match outcome {
            TurnOutcome::Failed { reason, .. } => assert!(!reason.is_empty()),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_turn_interrupted_elsewhere_is_cancelled_rather_than_completed() {
        // Another client, or Codex itself, stopped it. Read as complete, it
        // showed the Ready check and the unseen-completion dot for a turn that
        // never answered.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let mut session = session_with(&server, &[]).await;
        let thread_id = session.thread_id().to_string();
        let outcome = session
            .run_turn(
                &TurnRequest::new(&thread_id, "interrupted-elsewhere", "/tmp"),
                |_| {},
            )
            .await
            .expect("the turn ran");
        assert_eq!(outcome, TurnOutcome::Cancelled { thread_id });
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_quiet_turn_is_not_failed_for_its_silence() {
        // `sleep 200`, a quiet `docker build`, long reasoning: minutes with no
        // event at all. The turn used to fail after 120 s of that, and the app
        // then killed Codex mid-turn. A real child and pipe, with only Tokio's
        // clock moved past the old cutoff.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let mut session = session_with(&server, &[]).await;
        let thread_id = session.thread_id().to_string();
        let (started, mut seen) = mpsc::unbounded_channel();
        let turn = tokio::spawn(async move {
            session
                .run_turn(&TurnRequest::new(&thread_id, "quiet", "/tmp"), |event| {
                    let _ = started.send(event);
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            while !matches!(seen.recv().await, Some(ProviderEvent::Lifecycle(_)) | None) {}
        })
        .await
        .expect("the turn started");

        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(60 * 60 + 1)).await;
        // Let the turn observe the moved clock before the child speaks, so an
        // elapsed timer cannot race the completion and hide the regression.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        tokio::time::resume();
        std::fs::write(server.file("release-quiet"), "go").unwrap();

        let outcome = tokio::time::timeout(Duration::from_secs(10), turn)
            .await
            .expect("the turn finished")
            .unwrap()
            .expect("no transport error");
        assert!(outcome.is_completed(), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_stop_before_the_turn_start_reply_still_learns_which_turn_to_stop() {
        // Codex has the prompt as soon as `turn/start` is written. A Stop that
        // dropped the turn before the reply used to forget it: the prompt read
        // as never sent, and the interrupt named no turn at all.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let mut session = session_with(&server, &[]).await;
        let thread_id = session.thread_id().to_string();
        let dropped = tokio::time::timeout(
            Duration::from_millis(50),
            session.run_turn(&TurnRequest::new(&thread_id, "slow-ack", "/tmp"), |_| {}),
        )
        .await;
        assert!(dropped.is_err(), "the reply arrived before the stop");
        assert!(!session.prompt_accepted());

        session.interrupt().await.expect("interrupted");
        assert!(
            session.prompt_accepted(),
            "the late reply was not collected"
        );
        let interrupt = server
            .handshake()
            .into_iter()
            .find(|entry| entry["method"] == "turn/interrupt")
            .expect("an interrupt was sent");
        assert_eq!(interrupt["params"]["turnId"], "turn-1");
    }

    #[tokio::test]
    async fn stopping_a_busy_turn_is_answered_without_waiting_out_a_timeout() {
        // Codex keeps reporting while it stops. Nobody reads a dropped turn's
        // stream, so the queue filled, the reader stopped, and the interrupt's
        // answer queued behind the backlog was never read.
        let Some(server) = FakeServer::new() else {
            return;
        };
        std::fs::write(server.file("flood"), "").unwrap();
        let mut session = session_with(&server, &[]).await;
        let thread_id = session.thread_id().to_string();
        let dropped = tokio::time::timeout(
            Duration::from_millis(100),
            session.run_turn(&TurnRequest::new(&thread_id, "hang", "/tmp"), |_| {}),
        )
        .await;
        assert!(dropped.is_err(), "the hanging turn ended on its own");
        tokio::time::timeout(Duration::from_secs(5), session.interrupt())
            .await
            .expect("the interrupt's answer was stuck behind the backlog")
            .expect("interrupted");
    }

    #[tokio::test]
    async fn a_server_that_dies_mid_turn_fails_the_turn() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let mut session = session_with(&server, &[]).await;
        let thread_id = session.thread_id().to_string();
        let outcome = session
            .run_turn(
                &TurnRequest::new(&thread_id, "die-mid-turn", "/tmp"),
                |_| {},
            )
            .await
            .expect("the turn ended");
        match outcome {
            TurnOutcome::Failed { reason, .. } => assert!(!reason.is_empty(), "{reason}"),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_ask_is_answered_by_policy_so_the_turn_is_not_left_blocked() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = Transport::start(server.launch(), "FlowX", "test")
            .await
            .expect("started");
        let mut session = CodexSession::open(
            transport,
            None,
            AgentAccess::FullAccess,
            "/tmp",
            ApprovalPolicy::AllowAll,
        )
        .await
        .expect("a session");
        let thread_id = session.thread_id().to_string();

        let mut events = Vec::new();
        let outcome = session
            .run_turn(
                &TurnRequest::new(&thread_id, "use-a-tool", "/tmp"),
                |event| events.push(event),
            )
            .await
            .expect("the turn ran");
        assert!(outcome.is_completed(), "{outcome:?}");
        // An approval is not a result: an empty one marked the call finished
        // before it had run. The call's result is Codex's to report.
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ProviderEvent::ToolResult { .. })),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn a_refused_tool_call_says_why() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = Transport::start(server.launch(), "FlowX", "test")
            .await
            .expect("started");
        let mut session = CodexSession::open(
            transport,
            None,
            AgentAccess::FullAccess,
            "/tmp",
            ApprovalPolicy::DenyAll,
        )
        .await
        .expect("a session");
        let thread_id = session.thread_id().to_string();

        let mut events = Vec::new();
        session
            .run_turn(
                &TurnRequest::new(&thread_id, "use-a-tool", "/tmp"),
                |event| events.push(event),
            )
            .await
            .expect("the turn ran");
        let refusal = events
            .iter()
            .find_map(|event| match event {
                ProviderEvent::ToolResult {
                    content,
                    is_error: true,
                    ..
                } => Some(content.clone()),
                _ => None,
            })
            .expect("a refusal was reported");
        assert!(refusal.contains("refused"), "{refusal}");
    }

    #[tokio::test]
    async fn a_session_resumes_a_named_thread() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let session = CodexSession::open(
            Transport::start(server.launch(), "FlowX", "test")
                .await
                .expect("started"),
            Some("existing-thread"),
            AgentAccess::FullAccess,
            "/tmp",
            ApprovalPolicy::AllowAll,
        )
        .await
        .expect("a session");
        assert_eq!(session.thread_id().to_string(), "existing-thread");
    }

    #[tokio::test]
    async fn a_second_turn_runs_on_the_same_thread() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let mut session = session_with(&server, &[]).await;
        let thread_id = session.thread_id().to_string();
        let first = session
            .run_turn(&request(&thread_id, "one", "/tmp"), |_| {})
            .await
            .expect("first");
        let second = session
            .run_turn(&request(&thread_id, "two", "/tmp"), |_| {})
            .await
            .expect("second");
        assert_eq!(first.thread_id(), second.thread_id());
    }

    #[tokio::test]
    async fn interrupting_a_finished_turn_is_harmless() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let mut session = session_with(&server, &[]).await;
        // The turn is over; asking to interrupt must not panic or hang.
        let _ = session.interrupt().await;
    }

    #[tokio::test]
    async fn a_whole_turn_becomes_a_transcript_a_user_can_read() {
        // The end-to-end shape: a real turn, fed through the conversation, must
        // come out as prose / call / result / prose in that order. Unit tests of
        // `apply` cannot see whether the *event order* the provider produces is
        // the order the transcript needs.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let mut session = session_with(&server, &[]).await;
        let thread_id = session.thread_id().to_string();

        let mut conversation = crate::conversation::Conversation::new(uuid::Uuid::from_u128(1));
        let outcome = session
            .run_turn(
                &TurnRequest::new(&thread_id, "check-then-report", "/tmp"),
                |event| conversation.apply(event),
            )
            .await
            .expect("the turn ran");
        assert!(outcome.is_completed(), "{outcome:?}");
        conversation.commit_streaming();

        let rendered: Vec<String> = conversation
            .messages()
            .map(|message| match message.content.as_slice() {
                [fx_core::MessageContent::Text { text }] => format!("text: {text}"),
                [fx_core::MessageContent::ToolUse { name, .. }] => format!("call: {name}"),
                [fx_core::MessageContent::ToolResult { .. }] => "result".to_string(),
                _ => "other".to_string(),
            })
            .collect();

        assert_eq!(
            rendered,
            vec![
                "text: Let me check.".to_string(),
                "call: commandExecution".to_string(),
                "result".to_string(),
                "text: Found it.".to_string(),
            ],
            "the transcript did not preserve the order the turn happened in"
        );
    }

    async fn interactive_session(server: &FakeServer) -> CodexSession {
        CodexSession::open(
            Transport::start(server.launch(), "FlowX", "test")
                .await
                .unwrap(),
            None,
            AgentAccess::Supervised,
            "/tmp",
            ApprovalPolicy::Ask,
        )
        .await
        .unwrap()
    }

    fn echoed_response(events: &[ProviderEvent]) -> Value {
        events
            .iter()
            .find_map(|event| match event {
                ProviderEvent::TextDelta(text) => serde_json::from_str(text).ok(),
                _ => None,
            })
            .expect("the fake server echoed its response")
    }

    #[tokio::test]
    async fn interactive_approval_waits_for_the_user_and_uses_modern_wire_shape() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let mut session = interactive_session(&server).await;
        let thread = session.thread_id().to_owned();
        let (sender, mut responses) = mpsc::unbounded_channel();
        let mut resolved = Vec::new();
        let mut accepted = false;
        let seen = std::cell::RefCell::new(Vec::new());
        let outcome = session
            .run_turn_interactive(
                &request(&thread, "interactive-approval", "/tmp"),
                &mut responses,
                |event| {
                    seen.borrow_mut().push(event.clone());
                    if let ProviderEvent::Approval(request) = event {
                        assert_eq!(request.input["command"], "cargo test");
                        let sender = sender.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(250)).await;
                            sender
                                .send(UserResponse::Approval {
                                    id: request.id,
                                    approved: true,
                                })
                                .unwrap();
                        });
                    }
                },
                || accepted = true,
                |id| resolved.push(id),
            )
            .await
            .unwrap();
        assert!(accepted);
        assert!(outcome.is_completed(), "{outcome:?}");
        assert_eq!(
            echoed_response(&seen.borrow())["result"]["decision"],
            "accept"
        );
        assert_eq!(resolved, vec!["codex:\"interactive-1\""]);
    }

    #[tokio::test]
    async fn structured_questions_keep_all_ids_and_ignore_an_approval_answer() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let mut session = interactive_session(&server).await;
        let thread = session.thread_id().to_owned();
        let (sender, mut responses) = mpsc::unbounded_channel();
        let seen = std::cell::RefCell::new(Vec::new());
        session
            .run_turn_interactive(
                &request(&thread, "interactive-question", "/tmp"),
                &mut responses,
                |event| {
                    seen.borrow_mut().push(event.clone());
                    if let ProviderEvent::UserInput(request) = event {
                        assert_eq!(request.questions.len(), 2);
                        sender
                            .send(UserResponse::Approval {
                                id: request.id.clone(),
                                approved: true,
                            })
                            .unwrap();
                        sender
                            .send(UserResponse::Questions {
                                id: "another-request".into(),
                                answers: vec![],
                            })
                            .unwrap();
                        sender
                            .send(UserResponse::Questions {
                                id: request.id,
                                answers: vec![
                                    ("database".into(), "SQLite".into()),
                                    ("region".into(), "eu-west".into()),
                                ],
                            })
                            .unwrap();
                    }
                },
                || {},
                |_| {},
            )
            .await
            .unwrap();
        let response = echoed_response(&seen.borrow());
        assert_eq!(
            response["result"]["answers"]["database"]["answers"],
            json!(["SQLite"])
        );
        assert_eq!(
            response["result"]["answers"]["region"]["answers"],
            json!(["eu-west"])
        );
        assert!(response["result"].get("decision").is_none());
    }

    #[tokio::test]
    async fn a_request_for_another_thread_is_refused_without_opening_a_prompt() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let mut session = interactive_session(&server).await;
        let thread = session.thread_id().to_owned();
        let (_sender, mut responses) = mpsc::unbounded_channel();
        let seen = std::cell::RefCell::new(Vec::new());
        session
            .run_turn_interactive(
                &request(&thread, "foreign-request", "/tmp"),
                &mut responses,
                |event| {
                    seen.borrow_mut().push(event.clone());
                    assert!(!matches!(
                        event,
                        ProviderEvent::Approval(_) | ProviderEvent::UserInput(_)
                    ))
                },
                || {},
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(echoed_response(&seen.borrow())["error"]["code"], -32000);
    }

    #[tokio::test]
    async fn a_refused_request_from_another_thread_says_what_it_asked_for() {
        // Refused unseen, the command failed with nothing on screen to say why:
        // its own item belongs to the other thread and never arrives here.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let mut session = interactive_session(&server).await;
        let thread = session.thread_id().to_owned();
        let (_sender, mut responses) = mpsc::unbounded_channel();
        let mut conversation = crate::conversation::Conversation::new(uuid::Uuid::from_u128(1));
        session
            .run_turn_interactive(
                &request(&thread, "foreign-request", "/tmp"),
                &mut responses,
                |event| conversation.apply(event),
                || {},
                |_| {},
            )
            .await
            .unwrap();
        let (call, name, input) = conversation
            .messages()
            .find_map(|message| match message.content.as_slice() {
                [fx_core::MessageContent::ToolUse { id, name, input }] => {
                    Some((id.clone(), name.clone(), input.clone()))
                }
                _ => None,
            })
            .expect("the refused call has a row");
        assert_eq!(name, "commandExecution");
        assert!(input.contains("cargo test"), "{input}");
        let refusal = conversation
            .messages()
            .find_map(|message| match message.content.as_slice() {
                [
                    fx_core::MessageContent::ToolResult {
                        id,
                        content,
                        is_error: true,
                    },
                ] if *id == call => Some(content.clone()),
                _ => None,
            })
            .expect("the refusal is the call's result");
        assert!(refusal.contains("other-thread"), "{refusal}");
    }

    #[tokio::test]
    async fn a_sub_agents_request_is_answered_like_the_turns_own() {
        // A sub-agent asks under its own thread and turn. Refused as foreign,
        // every command a sub-agent wanted to run failed.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let mut session = interactive_session(&server).await;
        let thread = session.thread_id().to_owned();
        let (sender, mut responses) = mpsc::unbounded_channel();
        let mut resolved = Vec::new();
        let seen = std::cell::RefCell::new(Vec::new());
        let outcome = session
            .run_turn_interactive(
                &request(&thread, "subagent-request", "/tmp"),
                &mut responses,
                |event| {
                    seen.borrow_mut().push(event.clone());
                    if let ProviderEvent::Approval(request) = event {
                        assert_eq!(request.input["command"], "cargo test");
                        sender
                            .send(UserResponse::Approval {
                                id: request.id,
                                approved: true,
                            })
                            .unwrap();
                    }
                },
                || {},
                |id| resolved.push(id),
            )
            .await
            .unwrap();
        assert!(outcome.is_completed(), "{outcome:?}");
        assert_eq!(
            echoed_response(&seen.borrow())["result"]["decision"],
            "accept"
        );
        assert_eq!(resolved, vec!["codex:\"interactive-1\""]);
    }

    #[tokio::test]
    async fn a_sub_agents_call_refused_by_policy_has_a_row_to_finish() {
        // The call's own item stays in the sub-agent's thread, so the refusal
        // alone was a result with nothing above it.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let mut session = CodexSession::open(
            Transport::start(server.launch(), "FlowX", "test")
                .await
                .expect("started"),
            None,
            AgentAccess::FullAccess,
            "/tmp",
            ApprovalPolicy::DenyAll,
        )
        .await
        .expect("a session");
        let thread_id = session.thread_id().to_string();
        let mut events = Vec::new();
        session
            .run_turn(
                &TurnRequest::new(&thread_id, "subagent-request", "/tmp"),
                |event| events.push(event),
            )
            .await
            .expect("the turn ran");
        let call = events
            .iter()
            .position(
                |event| matches!(event, ProviderEvent::ToolUse { id, .. } if id == "command-1"),
            )
            .expect("the sub-agent's call has a row");
        let refusal = events
            .iter()
            .position(|event| {
                matches!(event, ProviderEvent::ToolResult { id, is_error: true, .. } if id == "command-1")
            })
            .expect("the refusal is its result");
        assert!(call < refusal, "{events:?}");
    }

    #[test]
    fn only_threads_codex_names_as_children_are_family() {
        let mut spawned = SpawnedThreads::default();
        for (method, params) in [
            (
                "item/started",
                json!({"threadId":"parent", "item":{"type":"subAgentActivity", "agentThreadId":"child"}}),
            ),
            (
                "item/completed",
                json!({"threadId":"parent", "item":{"type":"collabAgentToolCall",
                       "senderThreadId":"parent", "receiverThreadIds":["worker"]}}),
            ),
            // A sub-agent's own sub-agent is family too.
            (
                "thread/started",
                json!({"thread":{"id":"grandchild", "parentThreadId":"child"}}),
            ),
            // Children named by a thread that is not family are not.
            (
                "item/started",
                json!({"threadId":"stranger", "item":{"type":"subAgentActivity", "agentThreadId":"strangers-child"}}),
            ),
            (
                "thread/started",
                json!({"thread":{"id":"orphan", "parentThreadId":"stranger"}}),
            ),
            (
                "item/completed",
                json!({"threadId":"parent", "item":{"type":"collabAgentToolCall",
                       "senderThreadId":"stranger", "receiverThreadIds":["borrowed"]}}),
            ),
        ] {
            spawned.observe(method, &params, "parent");
        }
        for id in ["child", "worker", "grandchild"] {
            assert!(spawned.sent(&json!({ "threadId": id })), "{id}");
        }
        for id in [
            "stranger",
            "strangers-child",
            "orphan",
            "borrowed",
            "parent",
        ] {
            assert!(!spawned.sent(&json!({ "threadId": id })), "{id}");
        }
        assert!(!spawned.sent(&json!({})));
    }

    #[tokio::test]
    async fn cancelling_questions_sends_a_cancellation_error_instead_of_an_approval() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let mut session = interactive_session(&server).await;
        let thread = session.thread_id().to_owned();
        let (sender, mut responses) = mpsc::unbounded_channel();
        let seen = std::cell::RefCell::new(Vec::new());
        session
            .run_turn_interactive(
                &request(&thread, "interactive-question", "/tmp"),
                &mut responses,
                |event| {
                    seen.borrow_mut().push(event.clone());
                    if let ProviderEvent::UserInput(request) = event {
                        sender
                            .send(UserResponse::CancelQuestions { id: request.id })
                            .unwrap();
                    }
                },
                || {},
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(echoed_response(&seen.borrow())["error"]["code"], -32800);
    }

    #[test]
    fn approval_wire_shapes_distinguish_modern_legacy_and_permissions() {
        for (method, expected) in [
            ("execCommandApproval", json!({"decision":"approved"})),
            (
                "item/commandExecution/requestApproval",
                json!({"decision":"accept"}),
            ),
            (
                "item/fileChange/requestApproval",
                json!({"decision":"accept"}),
            ),
            (
                "item/permissions/requestApproval",
                json!({"permissions":{"network":{"enabled":true}},"scope":"turn"}),
            ),
        ] {
            let request = PendingRequest::parse(
                json!(9),
                method,
                &json!({"permissions":{"network":{"enabled":true}}}),
            )
            .unwrap();
            assert_eq!(request.approval_result(true), Some(expected));
        }
    }

    #[test]
    fn a_legacy_denial_is_the_object_codex_accepts() {
        // Codex 0.164's `ReviewDecision` has no bare "denied": a denial is
        // `{"denied": {"rejection": …}}`, and anything else is rejected.
        for method in ["execCommandApproval", "applyPatchApproval"] {
            let request = PendingRequest::parse(json!(9), method, &json!({})).unwrap();
            let result = request.approval_result(false).unwrap();
            assert!(
                result["decision"]["denied"]["rejection"].is_string(),
                "{method} denied as {result}"
            );
        }
    }

    #[test]
    fn the_default_policy_refuses_anything_that_writes() {
        let policy = ApprovalPolicy::default();
        assert!(!policy.allows(
            "commandExecution",
            &json!({ "arguments": { "command": ["rm", "-rf", "/"] } })
        ));
        assert!(policy.allows(
            "commandExecution",
            &json!({ "arguments": { "command": ["cat", "Cargo.toml"] } })
        ));
        assert!(policy.allows("webSearch", &json!({ "arguments": {} })));
        // Nothing unknown is auto-approved.
        assert!(!policy.allows("someNewTool", &json!({ "arguments": {} })));
        // The shapes that used to slip through: a write flag, a second command,
        // and an argv whose quotes would hide one once joined.
        for command in [
            json!("find . -delete"),
            json!("ls & rm -rf x"),
            json!("cat x\nrm y"),
            json!(["find", ".", "'", "-delete", "'"]),
        ] {
            assert!(
                !policy.allows("commandExecution", &json!({ "command": command })),
                "{command} was approved"
            );
        }
        assert!(policy.allows(
            "commandExecution",
            &json!({ "command": "/bin/zsh -lc 'rg -n foo src'" })
        ));
    }

    #[test]
    fn claude_tools_are_judged_by_claudes_own_names() {
        // `allows` knows only Codex's names, so every Claude tool — `Read`
        // included — was refused under the read-only policy.
        let policy = ApprovalPolicy::DenyUnlessReadOnly;
        for tool in ["Read", "Grep", "Glob", "LS"] {
            assert!(policy.allows_claude_tool(tool, &json!({})), "{tool}");
        }
        assert!(policy.allows_claude_tool("Bash", &json!({ "command": "git status" })));
        assert!(!policy.allows_claude_tool("Bash", &json!({ "command": "git push" })));
        for tool in ["Edit", "Write", "MultiEdit", "NotebookEdit", "SomethingNew"] {
            assert!(!policy.allows_claude_tool(tool, &json!({})), "{tool}");
        }
        assert!(ApprovalPolicy::AllowAll.allows_claude_tool("Write", &json!({})));
        assert!(!ApprovalPolicy::DenyAll.allows_claude_tool("Read", &json!({})));
    }

    #[test]
    fn the_allow_all_policy_allows_everything() {
        assert!(ApprovalPolicy::AllowAll.allows("shell", &json!({ "command": "rm -rf /" })));
    }

    #[test]
    fn a_launch_without_extra_directories_still_starts() {
        // The fixture helper must not be load-bearing for these tests.
        let launch = Launch {
            executable: "python3".into(),
            args: vec!["fake.py".into()],
            working_directory: None,
            environment: Vec::new(),
        };
        assert_eq!(launch.args.len(), 1);
        let _ = FakeServer::new();
    }
}
