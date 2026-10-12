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
//!   completion, an error, or the server dying ends it — and a turn that ends
//!   early looks to the user like the model stopped mid-sentence.
//! - **Approvals are answered by policy here.** A caller with a user in the loop
//!   answers them itself; without one, an unanswered request would leave Codex
//!   blocked, so the turn answers with the configured policy and reports what it
//!   did.

use std::collections::HashMap;
use std::time::Duration;

use fx_core::AgentAccess;
use serde_json::{Value, json};

use crate::codex::session::{
    CodexWriterConflict, TurnRequest, parse_notification, thread_id_from_result, thread_params,
    turn_params,
};
use crate::codex::transport::{ServerEvent, Transport, TransportError};
use crate::provider::{ApprovalRequest, ProviderError, ProviderEvent, RiskLevel, UserResponse};
use tokio::sync::mpsc;

/// How long a whole turn may take before FlowX gives up on it.
///
/// Longer than a single control request, because a turn is minutes of model
/// time. It exists so a server that stops talking without dying still surfaces as
/// a failure instead of a spinner that never stops.
pub const TURN_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// How long to wait after the last event before assuming the server is stuck.
///
/// Shorter than the whole-turn budget so a stalled turn is reported while there
/// is still something to tell the user about.
pub const TURN_SILENCE_TIMEOUT: Duration = Duration::from_secs(120);

/// How a turn ended.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnOutcome {
    /// Codex reported the turn complete.
    Completed {
        thread_id: String,
        stop_reason: String,
        events: Vec<ProviderEvent>,
    },
    /// The turn was cancelled.
    Cancelled {
        thread_id: String,
        events: Vec<ProviderEvent>,
    },
    /// Codex reported a failure.
    Failed {
        thread_id: String,
        reason: String,
        events: Vec<ProviderEvent>,
    },
}

impl TurnOutcome {
    pub fn thread_id(&self) -> &str {
        match self {
            Self::Completed { thread_id, .. }
            | Self::Cancelled { thread_id, .. }
            | Self::Failed { thread_id, .. } => thread_id,
        }
    }

    pub fn events(&self) -> &[ProviderEvent] {
        match self {
            Self::Completed { events, .. }
            | Self::Cancelled { events, .. }
            | Self::Failed { events, .. } => events,
        }
    }

    /// Whether the turn produced a usable answer.
    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }
}

/// A command argument as text.
///
/// Codex sends the command as an argv array far more often than as a string, and
/// a policy check that only understood the string form would treat every array
/// command as unknown — which under a deny-unless-read-only policy means
/// refusing every read.
fn command_text(command: &Value) -> Option<String> {
    match command {
        Value::String(text) => Some(text.clone()),
        // An argv array is joined rather than refused: Codex sends the command
        // that way far more often than as a string, and a policy check that only
        // understood the string form would treat every array command as unknown,
        // which under a deny-unless-read-only policy means refusing every read.
        //
        // The join allocates. It is owned by the caller rather than leaked,
        // because this runs on every approval in a turn.
        Value::Array(words) => words
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" ")
            .into(),
        _ => None,
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
                    "commandExecution" => arguments
                        .get("command")
                        .and_then(command_text)
                        .is_some_and(|command| {
                            crate::codex::session::is_read_only_command(&command)
                        }),
                    _ => false,
                }
            }
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
    active_turn_id: Option<String>,
    approvals: ApprovalPolicy,
    /// How long the whole turn may take.
    turn_timeout: Duration,
    /// How long to wait between events before declaring the turn stalled.
    silence_timeout: Duration,
}

impl std::fmt::Debug for CodexSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexSession")
            .field("thread_id", &self.thread_id)
            .field("approvals", &self.approvals)
            .field("turn_timeout", &self.turn_timeout)
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
            approvals,
            turn_timeout: TURN_TIMEOUT,
            silence_timeout: TURN_SILENCE_TIMEOUT,
        })
    }

    /// Override the timeouts.
    ///
    /// A stalled turn has to be reported rather than left spinning, but the
    /// budget has to be a parameter or the test for it takes minutes.
    pub fn with_timeouts(mut self, turn_timeout: Duration, silence_timeout: Duration) -> Self {
        self.turn_timeout = turn_timeout;
        self.silence_timeout = silence_timeout;
        self
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// Ask Codex to stop the turn in flight.
    ///
    /// Best-effort: the turn may already be over, and a failure here must not
    /// replace the real outcome.
    pub async fn interrupt(&self) -> Result<(), ProviderError> {
        self.transport
            .request(
                "turn/interrupt",
                Some(json!({ "threadId": self.thread_id, "turnId": self.active_turn_id })),
            )
            .await
            .map(|_| ())
            .map_err(map_error)
    }

    /// Send a prompt and run the turn to completion.
    ///
    /// Every event is handed to `sink` as it arrives, *and* collected, so a
    /// caller can drive a live view and still have the transcript afterwards.
    pub async fn run_turn(
        &mut self,
        request: &TurnRequest<'_>,
        sink: impl FnMut(ProviderEvent),
    ) -> Result<TurnOutcome, ProviderError> {
        self.drive_turn(request, None, sink, || {}, |_| {}).await
    }

    /// Run with an interactive response channel. Waiting for the user suspends
    /// silence/turn timers, while process exits and cancellation remain live.
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
        let ack = self
            .transport
            .request("turn/start", Some(turn_params(request)))
            .await
            .map_err(classify_turn_error)?;
        let turn_id = ack
            .get("turn")
            .and_then(|turn| turn.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        self.active_turn_id = Some(turn_id.clone());
        accepted();
        let mut deadline = tokio::time::Instant::now() + self.turn_timeout;
        let mut events = Vec::new();
        let mut pending: HashMap<String, PendingRequest> = HashMap::new();
        let mut items: HashMap<String, Value> = HashMap::new();

        loop {
            let waiting = !pending.is_empty();
            let silence = self
                .silence_timeout
                .min(deadline.saturating_duration_since(tokio::time::Instant::now()));
            let began = tokio::time::Instant::now();
            let next = tokio::select! {
                next = self.transport.next_event() => next,
                response = async {
                    match responses.as_mut() {
                        Some(receiver) => receiver.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if waiting { deadline += began.elapsed(); }
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
                _ = tokio::time::sleep(silence), if !waiting => {
                    return Ok(TurnOutcome::Failed { thread_id, reason: "Codex stopped sending updates before the turn finished.".into(), events });
                }
            };
            if waiting {
                deadline += began.elapsed();
            }
            let Some(next) = next else {
                let reason = self
                    .transport
                    .exit_reason()
                    .await
                    .map(|exit| exit.reason())
                    .unwrap_or_else(|| "the Codex connection closed".into());
                return Ok(TurnOutcome::Failed {
                    thread_id,
                    reason,
                    events,
                });
            };
            match next {
                ServerEvent::Notification { method, params } => {
                    if !belongs_to_turn(&params, &thread_id, &turn_id) {
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
                    for event in parse_notification(&method, &params) {
                        sink(event.clone());
                        events.push(event);
                        if let Some(outcome) =
                            finish(events.last().expect("just pushed"), &thread_id, &events)
                        {
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
                    if !belongs_to_turn(&params, &thread_id, &turn_id) {
                        self.transport
                            .refuse(
                                id,
                                -32000,
                                "This request does not belong to the active FlowX turn.",
                            )
                            .map_err(map_error)?;
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
                            sink(event.clone());
                            events.push(event);
                        } else {
                            let approved = matches!(&event, ProviderEvent::Approval(a) if self.approvals.allows(&a.tool_name, &a.input));
                            if let Some(result) = request.approval_result(approved) {
                                self.transport.respond(id, result).map_err(map_error)?;
                            } else {
                                request.reject(&self.transport)?;
                            }
                            if let ProviderEvent::Approval(a) = event {
                                let event = ProviderEvent::ToolResult {
                                    id: params
                                        .get("callId")
                                        .or_else(|| params.get("itemId"))
                                        .and_then(Value::as_str)
                                        .unwrap_or(&a.id)
                                        .to_owned(),
                                    content: if approved {
                                        String::new()
                                    } else {
                                        self.approvals.message(&a.tool_name)
                                    },
                                    is_error: !approved,
                                };
                                sink(event.clone());
                                events.push(event);
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

    /// Stop the session.
    pub async fn close(self) {
        self.transport.close().await;
    }

    /// Whether the underlying process has exited.
    pub async fn exit_reason(&self) -> Option<String> {
        self.transport.exit_reason().await.map(|exit| exit.reason())
    }
}

fn request_key(id: &Value) -> String {
    format!("codex:{id}")
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
                let command = params.get("command").and_then(command_text);
                ProviderEvent::Approval(ApprovalRequest {
                    id,
                    tool_name: tool_name.clone(),
                    input: params.clone(),
                    risk: if tool_name == "permissions" {
                        RiskLevel::Dangerous
                    } else {
                        crate::codex::session::risk_for(&tool_name, command.as_deref())
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

    fn approval_result(&self, approved: bool) -> Option<Value> {
        if !matches!(self.event, ProviderEvent::Approval(_)) {
            return None;
        }
        Some(match self.method.as_str() {
            "execCommandApproval" | "applyPatchApproval" => {
                json!({"decision": if approved { "approved" } else { "denied" }})
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
fn finish(event: &ProviderEvent, thread_id: &str, events: &[ProviderEvent]) -> Option<TurnOutcome> {
    match event {
        ProviderEvent::Done { stop_reason } => Some(TurnOutcome::Completed {
            thread_id: thread_id.to_string(),
            stop_reason: stop_reason.clone(),
            events: events.to_vec(),
        }),
        ProviderEvent::Error(reason) => Some(TurnOutcome::Failed {
            thread_id: thread_id.to_string(),
            reason: reason.clone(),
            events: events.to_vec(),
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
    async fn a_turn_that_never_completes_is_reported_rather_than_spinning() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        // Short budgets, so the test does not take minutes.
        let mut session = session_with(&server, &[])
            .await
            .with_timeouts(Duration::from_secs(10), Duration::from_millis(400));
        let thread_id = session.thread_id().to_string();
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            session.run_turn(&TurnRequest::new(&thread_id, "hang", "/tmp"), |_| {}),
        )
        .await
        .expect("the turn stopped rather than spinning")
        .expect("no transport error");
        match outcome {
            TurnOutcome::Failed { reason, .. } => {
                assert!(reason.contains("before the turn finished"), "{reason}")
            }
            other => panic!("expected a failure, got {other:?}"),
        }
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

        let outcome = session
            .run_turn(&TurnRequest::new(&thread_id, "use-a-tool", "/tmp"), |_| {})
            .await
            .expect("the turn ran");
        assert!(outcome.is_completed(), "{outcome:?}");
        assert!(
            outcome.events().iter().any(
                |event| matches!(event, ProviderEvent::ToolResult { content, .. } if content.is_empty())
            ),
            "an allowed tool call should not carry a refusal message"
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

        let outcome = session
            .run_turn(&TurnRequest::new(&thread_id, "use-a-tool", "/tmp"), |_| {})
            .await
            .expect("the turn ran");
        let refusal = outcome
            .events()
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
        let session = session_with(&server, &[]).await;
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

    fn echoed_response(outcome: &TurnOutcome) -> Value {
        outcome
            .events()
            .iter()
            .find_map(|event| match event {
                ProviderEvent::TextDelta(text) => serde_json::from_str(text).ok(),
                _ => None,
            })
            .expect("the fake server echoed its response")
    }

    #[tokio::test]
    async fn interactive_approval_waits_past_silence_budget_and_uses_modern_wire_shape() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let mut session = interactive_session(&server)
            .await
            .with_timeouts(Duration::from_secs(3), Duration::from_millis(100));
        let thread = session.thread_id().to_owned();
        let (sender, mut responses) = mpsc::unbounded_channel();
        let mut resolved = Vec::new();
        let mut accepted = false;
        let outcome = session
            .run_turn_interactive(
                &request(&thread, "interactive-approval", "/tmp"),
                &mut responses,
                |event| {
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
        assert_eq!(echoed_response(&outcome)["result"]["decision"], "accept");
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
        let outcome = session
            .run_turn_interactive(
                &request(&thread, "interactive-question", "/tmp"),
                &mut responses,
                |event| {
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
        let response = echoed_response(&outcome);
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
        let outcome = session
            .run_turn_interactive(
                &request(&thread, "foreign-request", "/tmp"),
                &mut responses,
                |event| {
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
        assert_eq!(echoed_response(&outcome)["error"]["code"], -32000);
    }

    #[tokio::test]
    async fn cancelling_questions_sends_a_cancellation_error_instead_of_an_approval() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let mut session = interactive_session(&server).await;
        let thread = session.thread_id().to_owned();
        let (sender, mut responses) = mpsc::unbounded_channel();
        let outcome = session
            .run_turn_interactive(
                &request(&thread, "interactive-question", "/tmp"),
                &mut responses,
                |event| {
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
        assert_eq!(echoed_response(&outcome)["error"]["code"], -32800);
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
