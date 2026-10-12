//! Running one live turn.
//!
//! Everything else in `fx-app` is a view of state somebody else produced. This
//! module is the half that produces it: it opens the configured provider, sends
//! a prompt, feeds every event into a [`Conversation`], and closes the session
//! when the turn ends.
//!
//! Three rules shape the whole file, and each exists because the alternative is
//! a frozen window or a turn that lies about what happened.
//!
//! - **The conversation's lock is never held across an `.await`, and never held
//!   while calling into the UI.** The UI thread takes the same lock on every
//!   render, so a lock held across a socket read is not a style question — it is
//!   the window waiting for Codex. Every lock here is statement-scoped and is
//!   released before anything can suspend.
//! - **The session lives for exactly one turn.** The process and the thread's
//!   state are both server-side; keeping the process alive between turns would
//!   only buy a process nobody supervises.
//! - **A failure is never silent and never dressed up as a completion.** A
//!   missing CLI, a server that never finishes its handshake, and a server that
//!   dies mid-turn are three different problems with three different messages,
//!   and all three leave the conversation in `Failed` rather than `Idle`.
//!
//! A thread's identity is its provider-native id, and it is kept on the
//! [`Conversation`] rather than anywhere in here. That is deliberate: the
//! conversation already has a `session_id` slot for exactly this, it is already
//! shared with the UI, and it is already what outlives a turn — so the next turn
//! resumes rather than starting a fresh thread, and a thread reopened from disk
//! resumes the same way.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use fx_agent::claude::{
    ClaudeTransport, Launch as ClaudeLaunch, StreamItem, TurnRequest as ClaudeTurnRequest,
};
use fx_agent::provider::UserResponse;

use fx_agent::codex::provider::CLIENT_NAME;
use fx_agent::codex::session::TurnRequest;
use fx_agent::codex::{ApprovalPolicy, CodexProvider, CodexSession, Transport, TurnOutcome};
use fx_agent::{Conversation, Phase, ProviderEvent};
use fx_core::services::runtime::{BinarySpec, RuntimeDiscovery, claude_spec};
use fx_core::{Agent, AgentAccess, AgentMode, DEFAULT_CODEX_MODEL_ID};

/// The version reported to Codex.
///
/// FlowX's own, because the client is the app, not the crate that happens to
/// speak the protocol.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The ways a turn can fail before it has an answer.
///
/// Each is a different instruction for the user — install Codex, check that it
/// runs, check what it printed — so these are separate strings rather than one
/// "something went wrong". They also name separate *stages*, which is why
/// discovery is reported before the process is spawned: a quarantined binary
/// must never look like a server that failed to start.
const DISCOVERY_FAILED: &str = "FlowX could not find a usable Codex install.";
const HANDSHAKE_FAILED: &str = "Codex started but did not finish its handshake.";
const THREAD_FAILED: &str = "FlowX could not open a Codex thread.";
const TURN_FAILED: &str = "FlowX could not run the turn.";
const WORKER_FAILED: &str = "FlowX could not start a background worker.";

/// The prefix `ApprovalPolicy` puts on every refusal it reports.
const REFUSAL_SENTINEL: &str = "FlowX ";

/// What a runner reports to the UI.
///
/// Delivered through a callback rather than an entity notification: the work
/// finishes on a thread GPUI knows nothing about, and there is no entity to
/// notify. The callback runs on the runner's thread, so whatever it does must be
/// thread-safe.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionUpdate {
    /// Streaming content changed; wake the matching conversation view.
    Changed,
    /// The provider accepted the submitted prompt; a retained draft may clear.
    Accepted,
    /// Complete interaction data, preserving the grouping of multiple questions.
    Interaction { event: ProviderEvent },
    /// The request was answered or cleared by the provider.
    InteractionResolved { id: String },
    /// The session is open and the turn has a provider-native thread id.
    Started { thread_id: String },
    /// The turn produced an answer. A cancelled turn ends here too.
    Finished { outcome: TurnOutcome },
    /// There is no answer, and `reason` says why in the user's terms.
    Failed { reason: String },
    /// The server asked something and FlowX answered it on the user's behalf.
    Refused { tool: String, reason: String },
}

/// A running turn the UI can stop.
///
/// Cloning is a refcount bump, so a view may hold one for as long as it likes
/// without keeping the turn, the process, or the conversation alive.
#[derive(Clone, Debug)]
pub struct TurnRunner {
    /// Woken when the user asks the turn to stop.
    ///
    /// An unbounded channel rather than a flag: `interrupt` runs on the UI
    /// thread and must not wait, and `send` on an unbounded channel is an
    /// ordinary function call rather than a future. Idempotent because the
    /// runner reads one signal and then stops listening — a second stop is
    /// queued into a channel nobody reads, which is the cheapest no-op there is.
    interrupt: tokio::sync::mpsc::UnboundedSender<()>,
    responses: tokio::sync::mpsc::UnboundedSender<UserResponse>,
}

impl TurnRunner {
    /// Start a turn in the background.
    ///
    /// Returns immediately, with the handle that stops it. `on_update` is called
    /// from the runner's thread, so whatever it does must be thread-safe.
    ///
    /// `cx` is part of the signature so a caller can hand over its `App` without
    /// a special path, but this runner uses none of it. The work happens on a
    /// plain OS thread with its own async runtime and comes back through
    /// `on_update`: the transport spawns a child process and registers it with
    /// tokio's reactor, which does not exist inside GPUI's executor, so
    /// borrowing the app's would fail when the child is spawned rather than when
    /// the window opens.
    pub fn start(
        _cx: &mut gpui::App,
        conversation: Arc<Mutex<Conversation>>,
        project_root: String,
        agent: Agent,
        prompt: String,
        approvals: ApprovalPolicy,
        on_update: impl Fn(SessionUpdate) + Send + 'static,
    ) -> Self {
        let provider = match agent
            .configuration
            .provider_id
            .as_deref()
            .unwrap_or("codex")
        {
            "codex" => RuntimeProvider::Codex(CodexProvider::new(RuntimeDiscovery::default())),
            "claude" => RuntimeProvider::Claude {
                discovery: RuntimeDiscovery::default(),
                spec: claude_spec(),
            },
            other => RuntimeProvider::Unsupported(other.to_owned()),
        };
        Self::launch(
            conversation,
            project_root,
            agent,
            prompt,
            approvals,
            provider,
            on_update,
        )
    }

    /// Ask the running turn to stop. Best-effort and idempotent.
    ///
    /// Never waits and never blocks — it is one channel send — so it is as safe
    /// from a paint, a menu callback, or any other thread as from the code that
    /// owns the turn.
    pub fn interrupt(&self) {
        // A failed send means the runner has already gone, which is the one
        // outcome that needs no action.
        let _ = self.interrupt.send(());
    }

    /// Reply only to a request owned by this runner. Wrong or stale ids are
    /// ignored by the provider without consuming any other pending request.
    pub fn respond(&self, response: UserResponse) -> bool {
        self.responses.send(response).is_ok()
    }

    /// The single path into a worker, shared by `start` and its test seam.
    fn launch(
        conversation: Arc<Mutex<Conversation>>,
        project_root: String,
        agent: Agent,
        prompt: String,
        approvals: ApprovalPolicy,
        provider: RuntimeProvider,
        on_update: impl Fn(SessionUpdate) + Send + 'static,
    ) -> Self {
        let (handle, interrupt) = tokio::sync::mpsc::unbounded_channel();
        let (response_handle, responses) = tokio::sync::mpsc::unbounded_channel();
        let claude = matches!(provider, RuntimeProvider::Claude { .. });
        let model = agent.configuration.model_id.clone().unwrap_or_else(|| {
            if claude {
                String::new()
            } else {
                DEFAULT_CODEX_MODEL_ID.to_string()
            }
        });
        let effort = crate::provider_catalog::effective_effort(
            if claude { "claude" } else { "codex" },
            if claude && model.is_empty() {
                "default"
            } else {
                &model
            },
            agent.configuration.effort.as_deref(),
        );
        let job = Job {
            conversation: conversation.clone(),
            provider,
            project_root,
            prompt,
            approvals,
            access: agent.configuration.resolved_access(AgentAccess::default()),
            model,
            effort,
            mode: agent.configuration.resolved_mode(),
            system_prompt: agent.configuration.system_prompt.clone(),
            on_update: Arc::new(Mutex::new(Box::new(on_update))),
            // The job keeps a sender of its own for the length of the run, so
            // the receiver cannot see a closed channel. The UI dropping its
            // handle is housekeeping, and a closed channel would read as a
            // request to stop.
            interrupt: handle.clone(),
            responses: response_handle.clone(),
        };

        // Claimed before the worker exists. A turn that has been asked for must
        // not read as idle for the few milliseconds the thread takes to start, or
        // the composer offers to send it again.
        lock(&job.conversation).set_phase(Phase::Starting);

        // One handle each for the failure path, which needs both after the
        // closure that owns the originals has been handed to `spawn`.
        let reported_conversation = job.conversation.clone();
        let reported_updates = job.on_update.clone();

        let worker = std::thread::Builder::new()
            .name("flowx-turn".to_string())
            .spawn({
                let reported_conversation = reported_conversation.clone();
                move || {
                    // Its own runtime rather than the app's executor: see `start`.
                    let runtime = match tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(runtime) => runtime,
                        Err(error) => {
                            let reason = format!("{WORKER_FAILED} {error}");
                            lock(&reported_conversation).set_error(reason.clone());
                            emit(&job.on_update, SessionUpdate::Failed { reason });
                            return;
                        }
                    };
                    runtime.block_on(job.run(interrupt, responses));
                }
            });

        if let Err(error) = worker {
            let reason = format!("{WORKER_FAILED} {error}");
            lock(&reported_conversation).set_error(reason.clone());
            emit(&reported_updates, SessionUpdate::Failed { reason });
        }
        Self {
            interrupt: handle,
            responses: response_handle,
        }
    }

    /// The seam the process tests need: the same runner, pointed at a provider that
    /// resolves to a fixture rather than to the user's install. It takes no `App`
    /// because [`TurnRunner::start`] does not use one either, and passing an
    /// empty one would only suggest otherwise.
    #[cfg(test)]
    fn launch_with(
        conversation: Arc<Mutex<Conversation>>,
        project_root: String,
        agent: Agent,
        prompt: String,
        approvals: ApprovalPolicy,
        provider: CodexProvider,
        on_update: impl Fn(SessionUpdate) + Send + 'static,
    ) -> Self {
        Self::launch(
            conversation,
            project_root,
            agent,
            prompt,
            approvals,
            RuntimeProvider::Codex(provider),
            on_update,
        )
    }
}

enum RuntimeProvider {
    Codex(CodexProvider),
    Claude {
        discovery: RuntimeDiscovery,
        spec: BinarySpec,
    },
    Unsupported(String),
}

/// Everything one worker needs, gathered so it crosses the thread boundary in a
/// single move.
struct Job {
    conversation: Arc<Mutex<Conversation>>,
    provider: RuntimeProvider,
    project_root: String,
    prompt: String,
    approvals: ApprovalPolicy,
    access: AgentAccess,
    model: String,
    effort: Option<String>,
    mode: AgentMode,
    system_prompt: Option<String>,
    on_update: UpdateSink,
    /// Held for the length of the run so its receivers never close spuriously.
    interrupt: tokio::sync::mpsc::UnboundedSender<()>,
    responses: tokio::sync::mpsc::UnboundedSender<UserResponse>,
}

/// How a turn ended, or that it was stopped.
enum Ending {
    Ran(Result<TurnOutcome, fx_agent::ProviderError>),
    Interrupted,
}

/// The caller's update callback, shareable by the job and by the sink that runs
/// inside `run_turn`.
///
/// Behind a mutex rather than used as a bare `Arc<dyn Fn + Send>`: sharing an
/// `Arc` across an ownership boundary needs `Sync`, and demanding `Sync` of the
/// caller's closure would be a change to the API it is handed through. A
/// `Mutex` needs only `Send`, and the caller's callback may hold things that are
/// safely sent to this thread but not shared between two of them.
type UpdateSink = Arc<Mutex<Box<dyn Fn(SessionUpdate) + Send + 'static>>>;

/// Call the caller's callback.
fn emit(sink: &UpdateSink, update: SessionUpdate) {
    // The lock is held across the call deliberately. It is what makes the sink
    // `Send` without asking the closure for `Sync`, and nothing re-enters it:
    // every call site is outside the conversation's own lock, so a callback that
    // renders cannot deadlock against the event that triggered it.
    let callback = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    callback(update);
}

impl Job {
    async fn run(
        self,
        interrupts: tokio::sync::mpsc::UnboundedReceiver<()>,
        responses: tokio::sync::mpsc::UnboundedReceiver<UserResponse>,
    ) {
        let _handles_stay_alive = (&self.interrupt, &self.responses);
        match &self.provider {
            RuntimeProvider::Codex(provider) => {
                self.run_codex(provider, interrupts, responses).await
            }
            RuntimeProvider::Claude { discovery, spec } => {
                self.run_claude(discovery, spec, interrupts, responses)
                    .await
            }
            RuntimeProvider::Unsupported(id) => {
                self.fail("FlowX does not support this provider:", id)
            }
        }
    }

    async fn run_codex(
        &self,
        provider: &CodexProvider,
        mut interrupts: tokio::sync::mpsc::UnboundedReceiver<()>,
        mut responses: tokio::sync::mpsc::UnboundedReceiver<UserResponse>,
    ) {
        // Borrowed, not moved: the sender has to stay alive for the whole run so
        // the receiver cannot report a closed channel, and a move would take
        // `self` apart before the turn starts.
        let _handle_stays_alive = &self.interrupt;

        // Discovery first, and separately from the spawn: "Codex is not
        // installed" and "Codex started but never answered" are different
        // instructions, and a quarantined binary has to be refused here rather
        // than look like a server that failed to start.
        let discovered = tokio::select! {
            result = provider.launch() => result,
            _ = interrupts.recv() => return self.cancel_before_start(),
        };
        let launch = match discovered {
            Ok(launch) => launch,
            Err(error) => return self.fail(DISCOVERY_FAILED, error.to_string()),
        };

        let started = tokio::select! {
            result = Transport::start(launch, CLIENT_NAME, CLIENT_VERSION) => result,
            _ = interrupts.recv() => return self.cancel_before_start(),
        };
        let transport = match started {
            Ok(transport) => transport,
            Err(error) => return self.fail(HANDSHAKE_FAILED, error.to_string()),
        };

        // The thread this conversation is already on. Read under the lock and
        // released before the await — `session_id` is the whole resume story.
        let resume = lock(&self.conversation).session_id().map(str::to_string);

        let opened = tokio::select! {
            result = CodexSession::open_with_instructions(
            transport,
            resume.as_deref(),
            self.access,
            &self.project_root,
            self.approvals,
            self.system_prompt.as_deref(),
        ) => result,
            _ = interrupts.recv() => return self.cancel_before_start(),
        };
        let mut session = match opened {
            Ok(session) => session,
            Err(error) => return self.fail(THREAD_FAILED, error.to_string()),
        };

        // Recorded before it is reported, so a caller that reopens the
        // conversation in between finds the id already there.
        let thread_id = session.thread_id().to_string();
        lock(&self.conversation).set_session_id(thread_id.clone());
        emit(
            &self.on_update,
            SessionUpdate::Started {
                thread_id: thread_id.clone(),
            },
        );

        // Everything borrowing `session` or `self` is scoped to this block.
        // `run_turn` holds a `&mut session` for as long as its future lives, and
        // interrupting needs a `&session` — so the request, the future, and the
        // borrows they carry must all be dropped before `session` can be touched
        // again. That is the whole reason for the block.
        let ending = {
            let request = TurnRequest::new(&thread_id, &self.prompt, &self.project_root)
                .model(&self.model)
                .effort(self.effort.as_deref().unwrap_or_default())
                .access(self.access)
                .plan_mode(self.mode == AgentMode::Plan);
            let mut turn = Box::pin(session.run_turn_interactive(
                &request,
                &mut responses,
                |event| self.apply(event),
                || emit(&self.on_update, SessionUpdate::Accepted),
                |id| self.resolve(id),
            ));

            let ending = tokio::select! {
                // Biased towards the turn, so an interrupt arriving while the
                // turn is producing output cannot starve it.
                biased;
                ending = &mut turn => Ending::Ran(ending),
                _ = interrupts.recv() => Ending::Interrupted,
            };
            drop(turn);
            ending
        };

        match ending {
            Ending::Ran(Ok(TurnOutcome::Failed { reason, .. })) => {
                session.close().await;
                // The reason is Codex's own wording — a model refusal, an exit,
                // a stall — and prefixing it would only bury the part that tells
                // the user what happened.
                self.fail("", reason);
            }
            Ending::Ran(Ok(outcome)) => {
                session.close().await;
                self.settle(&outcome);
            }
            Ending::Ran(Err(error)) => {
                session.close().await;
                self.fail(TURN_FAILED, error.to_string());
            }
            Ending::Interrupted => {
                // Marked before the request goes out, so the composer says
                // "stopping" while Codex is deciding rather than after.
                lock(&self.conversation).set_phase(Phase::Cancelling);
                emit(&self.on_update, SessionUpdate::Changed);
                // Best-effort by design: the turn future is already unpolled, so
                // a server that has gone cannot be told. The turn ends as
                // cancelled either way.
                let _ = tokio::time::timeout(Duration::from_secs(2), session.interrupt()).await;
                session.close().await;
                // No events here. They reached the conversation through the
                // sink, and keeping a second copy of every streamed delta for
                // the length of a turn buys nothing the transcript does not
                // already have.
                self.settle(&TurnOutcome::Cancelled {
                    thread_id,
                    events: Vec::new(),
                });
            }
        }
    }

    async fn run_claude(
        &self,
        discovery: &RuntimeDiscovery,
        spec: &BinarySpec,
        mut interrupts: tokio::sync::mpsc::UnboundedReceiver<()>,
        mut responses: tokio::sync::mpsc::UnboundedReceiver<UserResponse>,
    ) {
        let discovered = tokio::select! {
            result = discovery.launch(spec) => result,
            _ = interrupts.recv() => return self.cancel_before_start(),
        };
        let runtime = match discovered {
            Ok(runtime) => runtime,
            Err(error) => {
                return self.fail(
                    "FlowX could not find a usable Claude Code install.",
                    error.to_string(),
                );
            }
        };
        let resume = lock(&self.conversation).session_id().map(str::to_owned);
        let request = ClaudeTurnRequest {
            model: (!self.model.is_empty()).then(|| self.model.clone()),
            effort: self.effort.clone(),
            system_prompt: self.system_prompt.clone(),
            mode: self.mode,
            access: Some(self.access),
            resume_session_id: resume.clone(),
            ..Default::default()
        };
        let mut args = spec.arguments.clone();
        args.extend(request.arguments());
        let launch = ClaudeLaunch {
            executable: runtime.executable.to_string_lossy().into_owned(),
            args,
            working_directory: Some(self.project_root.clone()),
            environment: runtime.environment.into_iter().collect(),
        };
        let mut transport = match ClaudeTransport::start(launch).await {
            Ok(transport) => transport,
            Err(error) => {
                return self.fail("FlowX could not start Claude Code.", error.to_string());
            }
        };
        let reader = transport.reader().await;
        if let Err(error) = transport.send_prompt(&self.prompt, &[] as &[String]).await {
            transport.close().await;
            return self.fail(TURN_FAILED, error.to_string());
        }
        let mut thread_id = resume.unwrap_or_default();
        let mut accepted = false;
        let mut waiting = std::collections::HashSet::new();
        let ending = loop {
            tokio::select! {
                _ = interrupts.recv() => {
                    lock(&self.conversation).set_phase(Phase::Cancelling);
                    emit(&self.on_update, SessionUpdate::Changed);
                    break TurnOutcome::Cancelled { thread_id, events: Vec::new() };
                }
                response = responses.recv() => {
                    let Some(response) = response else { continue; };
                    let id = response.id().to_owned();
                    if !waiting.contains(&id) { continue; }
                    let result = match response {
                        UserResponse::Approval { approved, .. } => transport.approve(&id, approved).await,
                        UserResponse::Questions { answers, .. } => transport.answer_questions(&id, &answers).await,
                        UserResponse::CancelQuestions { .. } => transport.decline_questions(&id).await,
                    };
                    if result.is_ok() { waiting.remove(&id); self.resolve(id); }
                }
                next = reader.next_event() => {
                    match next {
                        Some(StreamItem::Event(event)) => {
                            if let Some(id) = transport.session_id().await
                                && id != thread_id
                            {
                                thread_id = id.clone();
                                lock(&self.conversation).set_session_id(id.clone());
                                emit(&self.on_update, SessionUpdate::Started { thread_id: id });
                            }
                            // Init identifies the process/session. Real content
                            // proves the submitted prompt was accepted.
                            if !accepted && matches!(event, ProviderEvent::TextDelta(_) | ProviderEvent::Text(_) | ProviderEvent::ToolUse { .. } | ProviderEvent::Approval(_) | ProviderEvent::UserInput(_) | ProviderEvent::Done { .. }) {
                                accepted = true; emit(&self.on_update, SessionUpdate::Accepted);
                            }
                            if let ProviderEvent::Approval(ref request) = event {
                                let auto = if self.approvals == ApprovalPolicy::Ask {
                                    (self.access == AgentAccess::FullAccess && self.mode != AgentMode::Plan).then_some(true)
                                } else { Some(self.approvals.allows(&request.tool_name, &request.input)) };
                                if let Some(approved) = auto {
                                    if let Err(error) = transport.approve(&request.id, approved).await {
                                        break TurnOutcome::Failed { thread_id, reason: error.to_string(), events: Vec::new() };
                                    }
                                    continue;
                                }
                                waiting.insert(request.id.clone());
                            } else if let ProviderEvent::UserInput(ref request) = event {
                                waiting.insert(request.id.clone());
                            }
                            self.apply(event.clone());
                            match event {
                                ProviderEvent::Done { stop_reason } => break TurnOutcome::Completed { thread_id, stop_reason, events: Vec::new() },
                                ProviderEvent::Error(reason) => break TurnOutcome::Failed { thread_id, reason, events: Vec::new() },
                                _ => {}
                            }
                        }
                        Some(StreamItem::Exited(exit)) => break TurnOutcome::Failed { thread_id, reason: exit.reason(), events: Vec::new() },
                        None => break TurnOutcome::Failed { thread_id, reason: "Claude Code ended before reporting a result.".into(), events: Vec::new() },
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(120)), if waiting.is_empty() => {
                    break TurnOutcome::Failed { thread_id, reason: "Claude Code stopped sending updates before the turn finished.".into(), events: Vec::new() };
                }
            }
        };
        transport.close().await;
        match ending {
            TurnOutcome::Failed { reason, .. } => self.fail("", reason),
            outcome => self.settle(&outcome),
        }
    }

    fn cancel_before_start(&self) {
        let thread_id = lock(&self.conversation)
            .session_id()
            .unwrap_or_default()
            .to_owned();
        self.settle(&TurnOutcome::Cancelled {
            thread_id,
            events: Vec::new(),
        });
    }

    fn apply(&self, event: ProviderEvent) {
        if let Some((tool, reason)) = refused(&event) {
            emit(&self.on_update, SessionUpdate::Refused { tool, reason });
        }
        lock(&self.conversation).apply(event.clone());
        if matches!(
            event,
            ProviderEvent::Approval(_) | ProviderEvent::UserInput(_)
        ) {
            emit(&self.on_update, SessionUpdate::Interaction { event });
        }
        emit(&self.on_update, SessionUpdate::Changed);
    }

    fn resolve(&self, id: String) {
        let mut conversation = lock(&self.conversation);
        conversation.remove_approval(&id);
        conversation.remove_question(&id);
        drop(conversation);
        emit(&self.on_update, SessionUpdate::InteractionResolved { id });
    }

    fn clear_interactions(&self) {
        let ids: Vec<String> = {
            let conversation = lock(&self.conversation);
            conversation
                .pending_approvals()
                .iter()
                .map(|p| p.id.clone())
                .chain(
                    conversation
                        .pending_questions()
                        .iter()
                        .map(|p| p.id.clone()),
                )
                .collect()
        };
        for id in ids {
            self.resolve(id);
        }
    }

    /// Land a finished turn on the conversation and tell the UI.
    fn settle(&self, outcome: &TurnOutcome) {
        self.clear_interactions();
        let mut conversation = lock(&self.conversation);
        // `apply` already commits on `Done`, so this is the net for the paths
        // where `Done` never arrived — a stop, most obviously — where skipping it
        // would discard the turn's last and often only sentence. `fail` commits
        // for the same reason.
        conversation.commit_streaming();
        // `apply` leaves the conversation waiting when it is waiting on the user,
        // and that must survive the turn ending: an open approval is not
        // answered by the turn finishing.
        if !conversation.is_waiting_for_user() {
            conversation.set_phase(Phase::Idle);
        }
        drop(conversation);
        emit(
            &self.on_update,
            SessionUpdate::Finished {
                outcome: outcome.clone(),
            },
        );
    }

    /// Record a failure on the conversation and report it.
    ///
    /// `summary` is empty where the provider's own wording is already the whole
    /// message, which is the case for a turn that failed mid-answer.
    fn fail(&self, summary: &str, detail: impl AsRef<str>) {
        self.clear_interactions();
        let detail = detail.as_ref();
        let reason = if summary.is_empty() {
            detail.to_string()
        } else {
            format!("{summary} {detail}")
        };
        let mut conversation = lock(&self.conversation);
        conversation.commit_streaming();
        conversation.set_error(reason.clone());
        drop(conversation);
        emit(&self.on_update, SessionUpdate::Failed { reason });
    }
}

/// Take the conversation's lock, ignoring poisoning.
///
/// `unwrap_or_else` rather than `unwrap`: a panic taken while the lock was held
/// poisons it, and a poisoned conversation is still a conversation. A runner that
/// died on its first failure would report nothing at all, which is the one
/// outcome this module exists to prevent.
fn lock(conversation: &Mutex<Conversation>) -> MutexGuard<'_, Conversation> {
    conversation
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Whether an event is FlowX refusing a tool call, and on what.
///
/// `CodexSession` answers the server itself and reports what it did as a tool
/// result — which is also how a tool that genuinely failed arrives. The tool
/// name is on that text and nowhere else, since the event carries only an id, so
/// telling the two apart means recognising FlowX's own wording.
fn refused(event: &ProviderEvent) -> Option<(String, String)> {
    let ProviderEvent::ToolResult {
        content,
        is_error: true,
        ..
    } = event
    else {
        return None;
    };
    let tool = tool_in_refusal(content)?;
    Some((tool, content.clone()))
}

/// The tool name out of a refusal message shaped `"{reason} ({tool})"`.
///
/// If that wording ever changes the cost is a missing banner, not a wrong one: a
/// tool error never starts with [`REFUSAL_SENTINEL`], so nothing can be
/// misreported as a refusal.
fn tool_in_refusal(message: &str) -> Option<String> {
    let (reason, tool) = message.strip_prefix(REFUSAL_SENTINEL)?.rsplit_once(" (")?;
    let tool = tool.strip_suffix(')')?;
    if reason.trim().is_empty() || tool.is_empty() || tool.contains(['\n', '\r']) {
        return None;
    }
    Some(tool.to_string())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use fx_core::services::runtime::{BinarySpec, RuntimeDiscovery};
    use fx_core::{MessageContent, MessageRole};
    use serde_json::Value;
    use uuid::Uuid;

    use super::*;

    /// How long a test waits for a turn that should already be over.
    ///
    /// Generous because the budget covers spawning python and a process, not
    /// because a turn is slow: every fixture turn finishes in milliseconds, so a
    /// test that hits this was going to fail whatever the number is.
    const TURN_BUDGET: Duration = Duration::from_secs(30);

    fn conversation() -> Arc<Mutex<Conversation>> {
        Arc::new(Mutex::new(Conversation::new(Uuid::from_u128(1))))
    }

    /// Every update a run reported.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<SessionUpdate>>>);

    impl Recorder {
        fn record(&self, update: SessionUpdate) {
            lock_updates(&self.0).push(update);
        }

        fn updates(&self) -> Vec<SessionUpdate> {
            lock_updates(&self.0).clone()
        }

        /// Wait for the one update that ends a turn.
        fn wait_for_end(&self, budget: Duration) -> Vec<SessionUpdate> {
            let deadline = Instant::now() + budget;
            loop {
                let updates = self.updates();
                let ended = updates.iter().any(|update| {
                    matches!(
                        update,
                        SessionUpdate::Finished { .. } | SessionUpdate::Failed { .. }
                    )
                });
                if ended || Instant::now() >= deadline {
                    return updates;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn lock_updates(updates: &Mutex<Vec<SessionUpdate>>) -> MutexGuard<'_, Vec<SessionUpdate>> {
        updates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Start a turn against the fixture.
    ///
    /// Goes through the production path with one thing replaced — the provider,
    /// which would otherwise discover whatever `codex` the machine running the
    /// suite happens to have installed.
    fn start(
        conversation: &Arc<Mutex<Conversation>>,
        provider: CodexProvider,
        prompt: &str,
        approvals: ApprovalPolicy,
    ) -> (TurnRunner, Recorder) {
        let recorder = Recorder::default();
        let sink = recorder.clone();
        let conversation = conversation.clone();
        let runner = TurnRunner::launch_with(
            conversation,
            "/tmp/project".to_string(),
            Agent::new("Runner"),
            prompt.to_string(),
            approvals,
            provider,
            move |update| sink.record(update),
        );
        (runner, recorder)
    }

    /// The transcript, rendered as the row kinds the view distinguishes.
    fn rows(conversation: &Mutex<Conversation>) -> Vec<String> {
        lock(conversation)
            .messages()
            .map(|message| match message.content.as_slice() {
                [MessageContent::Text { text }] => format!("text: {text}"),
                [MessageContent::ToolUse { name, .. }] => format!("call: {name}"),
                [MessageContent::ToolResult { .. }] => "result".to_string(),
                _ => "other".to_string(),
            })
            .collect()
    }

    #[test]
    fn a_prompt_lands_in_the_conversation_and_the_turn_ends() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let conversation = conversation();

        let (_runner, recorder) = start(&conversation, provider, "hello", ApprovalPolicy::AllowAll);
        let updates = recorder.wait_for_end(TURN_BUDGET);

        assert!(
            matches!(updates.first(), Some(SessionUpdate::Started { .. })),
            "the thread id was not reported first: {updates:?}"
        );
        assert!(
            matches!(updates.last(), Some(SessionUpdate::Finished { outcome }) if outcome.is_completed()),
            "the turn did not complete: {updates:?}"
        );

        assert_eq!(
            rows(&conversation),
            vec!["text: Hello".to_string()],
            "the answer did not reach the transcript"
        );

        let conversation = lock(&conversation);
        assert!(!conversation.is_busy(), "the conversation stayed busy");
        assert_eq!(conversation.phase(), Phase::Idle);
        assert!(conversation.error().is_none(), "{:?}", conversation.error());
    }

    #[test]
    fn a_whole_turn_reads_as_prose_call_result_prose() {
        // The end-to-end ordering the transcript depends on. Unit tests of
        // `apply` cannot see whether the order a provider *produces* is the
        // order the transcript needs; only a real process shows that.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let conversation = conversation();

        let (_runner, recorder) = start(
            &conversation,
            provider,
            "check-then-report",
            ApprovalPolicy::AllowAll,
        );
        recorder.wait_for_end(TURN_BUDGET);

        assert_eq!(
            rows(&conversation),
            vec![
                "text: Let me check.".to_string(),
                "call: commandExecution".to_string(),
                "result".to_string(),
                "text: Found it.".to_string(),
            ],
            "the transcript did not preserve the order the turn happened in"
        );
    }

    #[test]
    fn a_second_turn_resumes_the_thread_the_first_one_opened() {
        // The identity of a thread is its provider-native id, so keeping it on
        // the conversation is what makes the next turn a continuation. Asserted
        // from the server's side, because a resume that silently started a new
        // thread would still look like a successful turn here.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let Some(second_provider) = server.provider() else {
            return;
        };
        let conversation = conversation();

        let (_runner, recorder) = start(&conversation, provider, "hello", ApprovalPolicy::AllowAll);
        recorder.wait_for_end(TURN_BUDGET);
        let opened = lock(&conversation).session_id().map(str::to_string);
        assert_eq!(opened.as_deref(), Some("thread-new"));

        let (_second, second) = start(
            &conversation,
            second_provider,
            "hello",
            ApprovalPolicy::AllowAll,
        );
        second.wait_for_end(TURN_BUDGET);

        assert_eq!(
            lock(&conversation).session_id().map(str::to_string),
            opened,
            "the second turn changed the thread"
        );
        let calls = server.thread_calls();
        assert_eq!(
            calls
                .iter()
                .map(|(method, _)| method.as_str())
                .collect::<Vec<_>>(),
            vec!["thread/start", "thread/resume"],
            "the second turn started a fresh thread instead of resuming: {calls:?}"
        );
        assert_eq!(
            calls[1].1.get("threadId").and_then(Value::as_str),
            opened.as_deref(),
            "the resumed thread is not the one the first turn opened"
        );
    }

    #[test]
    fn a_tool_call_flowx_refused_is_reported_as_a_refusal() {
        // There is no user in the loop here, so the policy answers for them. The
        // refusal has to surface: a turn that quietly blocked on an unanswered
        // request would look like a turn that finished.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let conversation = conversation();

        let (_runner, recorder) = start(
            &conversation,
            provider,
            "use-a-tool",
            ApprovalPolicy::DenyAll,
        );
        let updates = recorder.wait_for_end(TURN_BUDGET);

        let refusal = updates
            .iter()
            .find_map(|update| match update {
                SessionUpdate::Refused { tool, reason } => Some((tool.as_str(), reason.as_str())),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the refusal was not reported: {updates:?}"));
        assert_eq!(refusal.0, "shell");
        assert!(refusal.1.contains("refused"), "{}", refusal.1);

        // It is also a row, because the transcript shows what the model was told.
        assert!(
            lock(&conversation).messages().any(|message| {
                matches!(
                    message.content.as_slice(),
                    [MessageContent::ToolResult { is_error: true, .. }]
                )
            }),
            "the refusal left no trace in the transcript"
        );
    }

    #[test]
    fn a_server_that_dies_mid_turn_reports_a_failure_rather_than_a_completion() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let conversation = conversation();

        let (_runner, recorder) = start(
            &conversation,
            provider,
            "die-mid-turn",
            ApprovalPolicy::AllowAll,
        );
        let updates = recorder.wait_for_end(TURN_BUDGET);

        assert!(
            matches!(updates.last(), Some(SessionUpdate::Failed { .. })),
            "a dead server was reported as a finished turn: {updates:?}"
        );
        let conversation = lock(&conversation);
        let reason = conversation
            .error()
            .unwrap_or_else(|| panic!("the failure was not recorded: {updates:?}"));
        assert!(reason.contains("Codex"), "{reason}");
        assert_eq!(conversation.phase(), Phase::Failed);
        assert!(
            !conversation.is_busy(),
            "a dead server left the composer closed"
        );
    }

    #[test]
    fn a_turn_codex_fails_reports_codex_s_own_reason() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let conversation = conversation();

        let (_runner, recorder) = start(
            &conversation,
            provider,
            "fail-now",
            ApprovalPolicy::AllowAll,
        );
        let updates = recorder.wait_for_end(TURN_BUDGET);

        assert!(
            matches!(updates.last(), Some(SessionUpdate::Failed { reason }) if reason.contains("the model refused")),
            "{updates:?}"
        );
        assert_eq!(lock(&conversation).phase(), Phase::Failed);
    }

    #[test]
    fn a_missing_cli_is_reported_before_anything_is_started() {
        // Discovery, not the spawn. A quarantined or absent binary has to be
        // refused by the search rather than surfacing as a server that would not
        // start, because the two mean different things to the user.
        let spec = BinarySpec::new("codex", "Codex", vec!["/nonexistent/codex".to_string()]);
        let provider = CodexProvider::with_spec(
            RuntimeDiscovery::default().without_extra_directories(),
            spec,
        );
        let conversation = conversation();

        let (_runner, recorder) = start(&conversation, provider, "hello", ApprovalPolicy::AllowAll);
        let updates = recorder.wait_for_end(TURN_BUDGET);

        assert!(
            matches!(updates.last(), Some(SessionUpdate::Failed { reason }) if reason.starts_with(DISCOVERY_FAILED)),
            "{updates:?}"
        );
        let conversation = lock(&conversation);
        assert!(
            conversation
                .error()
                .is_some_and(|reason| reason.starts_with(DISCOVERY_FAILED)),
            "{:?}",
            conversation.error()
        );
        assert_eq!(conversation.phase(), Phase::Failed);
        assert!(
            !conversation.is_busy(),
            "a missing CLI left the composer closed"
        );
    }

    #[test]
    fn starting_a_turn_claims_the_conversation_before_the_worker_exists() {
        // Read immediately, with no wait. A turn that has been asked for but has
        // not yet spoken must still leave the composer closed, or the user can
        // send the same prompt twice.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let conversation = conversation();

        let (_runner, _recorder) =
            start(&conversation, provider, "hello", ApprovalPolicy::AllowAll);
        assert_eq!(lock(&conversation).phase(), Phase::Starting);
    }

    #[test]
    fn interrupting_a_running_turn_ends_it_without_an_answer() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let conversation = conversation();

        let (runner, recorder) = start(&conversation, provider, "hang", ApprovalPolicy::AllowAll);
        // The fixture accepts the turn and then says nothing, so the stop is the
        // only thing that can end it. Wait for the worker to be live first, or
        // this would pass by racing the interrupt.
        wait_for_phase(&conversation, Phase::Running);
        runner.interrupt();

        let updates = recorder.wait_for_end(TURN_BUDGET);
        assert!(
            matches!(updates.last(), Some(SessionUpdate::Finished { outcome }) if matches!(outcome, TurnOutcome::Cancelled { .. })),
            "a stopped turn did not end as cancelled: {updates:?}"
        );
        let conversation = lock(&conversation);
        assert_eq!(conversation.phase(), Phase::Idle);
        assert!(
            conversation.error().is_none(),
            "a stop the user asked for was reported as a failure: {:?}",
            conversation.error()
        );
    }

    #[test]
    fn interrupting_a_finished_turn_changes_nothing() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let conversation = conversation();

        let (runner, recorder) = start(&conversation, provider, "hello", ApprovalPolicy::AllowAll);
        let updates = recorder.wait_for_end(TURN_BUDGET);

        runner.interrupt();
        runner.interrupt();
        std::thread::sleep(Duration::from_millis(200));

        assert_eq!(
            recorder.updates(),
            updates,
            "a stop after the turn ended produced a second outcome"
        );
        let conversation = lock(&conversation);
        assert_eq!(conversation.phase(), Phase::Idle);
        assert!(conversation.error().is_none());
    }

    #[test]
    fn two_threads_do_not_corrupt_each_other() {
        // The mutex is the point. Two runners, two conversations, two processes
        // that hand out the *same* thread id — so anything that keyed off the id
        // instead of off the conversation would collide here.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(other) = FakeServer::new() else {
            return;
        };
        let (Some(provider), Some(other_provider)) = (server.provider(), other.provider()) else {
            return;
        };
        let first = conversation();
        let second = Arc::new(Mutex::new(Conversation::new(Uuid::from_u128(2))));

        let (_one, one) = start(
            &first,
            provider,
            "check-then-report",
            ApprovalPolicy::AllowAll,
        );
        let (_two, two) = start(&second, other_provider, "hello", ApprovalPolicy::AllowAll);

        one.wait_for_end(TURN_BUDGET);
        two.wait_for_end(TURN_BUDGET);

        assert_eq!(
            rows(&first),
            vec![
                "text: Let me check.".to_string(),
                "call: commandExecution".to_string(),
                "result".to_string(),
                "text: Found it.".to_string(),
            ],
            "one thread's events landed in the other"
        );
        assert_eq!(
            rows(&second),
            vec!["text: Hello".to_string()],
            "one thread's events landed in the other"
        );
        assert_eq!(
            lock(&first).session_id(),
            lock(&second).session_id(),
            "the fixture hands both the same id, so only the lock kept them apart"
        );
    }

    #[test]
    fn codex_uses_the_agents_model_effort_plan_access_and_instructions() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let conversation = conversation();
        let mut agent = Agent::new("Configured");
        agent.configuration.model_id = Some("fixture-model".into());
        agent.configuration.effort = Some("high".into());
        agent.configuration.agent_mode = Some(AgentMode::Plan);
        agent.configuration.agent_access = Some(AgentAccess::Supervised);
        agent.configuration.system_prompt = Some("fixture instructions".into());
        let recorder = Recorder::default();
        let sink = recorder.clone();
        let _runner = TurnRunner::launch_with(
            conversation.clone(),
            "/tmp/project".into(),
            agent,
            "echo-params".into(),
            ApprovalPolicy::Ask,
            provider,
            move |update| sink.record(update),
        );
        let updates = recorder.wait_for_end(TURN_BUDGET);
        assert!(
            matches!(updates.last(), Some(SessionUpdate::Finished { outcome }) if outcome.is_completed()),
            "{updates:?}"
        );
        let text = lock(&conversation)
            .messages()
            .last()
            .unwrap()
            .text_content();
        let params: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(params["model"], "fixture-model");
        assert_eq!(params["effort"], "high");
        assert_eq!(params["collaborationMode"]["mode"], "plan");
        assert_eq!(params["approvalPolicy"], "untrusted");
        assert_eq!(params["sandboxPolicy"]["type"], "workspaceWrite");
        assert_eq!(
            server.thread_calls()[0].1["developerInstructions"],
            "fixture instructions"
        );
    }

    struct ClaudeFixture {
        dir: tempfile::TempDir,
        script: PathBuf,
        log: PathBuf,
    }

    impl ClaudeFixture {
        fn new() -> Option<Self> {
            FakeServer::python()?;
            let dir = tempfile::tempdir().ok()?;
            let script = dir.path().join("claude.py");
            let log = dir.path().join("received.jsonl");
            std::fs::write(&script, r#"
import json, sys

def send(value):
    print(json.dumps(value), flush=True)
def record(value):
    with open("received.jsonl", "a") as log: log.write(json.dumps(value) + "\n")
def finish(text):
    send({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":text}}})
    send({"type":"result","subtype":"end_turn","session_id":"native-claude","usage":{"input_tokens":5,"output_tokens":2}})
record({"args":sys.argv[1:]})
for line in sys.stdin:
    message=json.loads(line)
    record(message)
    if message.get("type")=="user":
        prompt=message["message"]["content"][0]["text"]
        send({"type":"system","subtype":"init","session_id":"native-claude"})
        if prompt=="hang": continue
        if prompt in ("tool", "questions"):
            tool="Bash" if prompt=="tool" else "AskUserQuestion"
            data={"command":"cargo test"} if prompt=="tool" else {"questions":[
                {"header":"Database","question":"Which database?","options":[{"label":"SQLite"}]},
                {"header":"Region","question":"Which region?","options":[]}]}
            send({"type":"control_request","request_id":"control-1","request":{"subtype":"can_use_tool","tool_name":tool,"tool_use_id":"tool-1","input":data}})
        else: finish(prompt)
    elif message.get("type")=="control_response": finish("answered")
"#).ok()?;
            Some(Self { dir, script, log })
        }

        fn start(
            &self,
            conversation: &Arc<Mutex<Conversation>>,
            prompt: &str,
            access: AgentAccess,
        ) -> (TurnRunner, Recorder) {
            self.start_with_model(conversation, prompt, access, "fixture-model")
        }

        fn start_with_model(
            &self,
            conversation: &Arc<Mutex<Conversation>>,
            prompt: &str,
            access: AgentAccess,
            model: &str,
        ) -> (TurnRunner, Recorder) {
            let mut agent = Agent::new("Claude");
            agent.configuration.provider_id = Some("claude".into());
            agent.configuration.model_id = Some(model.into());
            agent.configuration.effort = Some("high".into());
            agent.configuration.system_prompt = Some("fixture instructions".into());
            agent.configuration.agent_access = Some(access);
            let provider = RuntimeProvider::Claude {
                discovery: RuntimeDiscovery::default().without_extra_directories(),
                spec: BinarySpec::new("claude", "Claude Code", vec![FakeServer::python().unwrap()])
                    .with_arguments(vec![self.script.display().to_string()]),
            };
            let recorder = Recorder::default();
            let sink = recorder.clone();
            let runner = TurnRunner::launch(
                conversation.clone(),
                self.dir.path().display().to_string(),
                agent,
                prompt.into(),
                ApprovalPolicy::Ask,
                provider,
                move |update| sink.record(update),
            );
            (runner, recorder)
        }

        fn received(&self) -> Vec<Value> {
            std::fs::read_to_string(&self.log)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    fn wait_for_interaction(recorder: &Recorder) -> ProviderEvent {
        let deadline = Instant::now() + TURN_BUDGET;
        loop {
            for update in recorder.updates() {
                match update {
                    SessionUpdate::Interaction { event } => return event,
                    SessionUpdate::Failed { reason } => panic!("runner failed: {reason}"),
                    _ => {}
                }
            }
            assert!(
                Instant::now() < deadline,
                "no interaction: {:?}",
                recorder.updates()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn claude_routes_streams_and_resumes_its_native_session_with_configuration() {
        let Some(server) = ClaudeFixture::new() else {
            return;
        };
        let conversation = conversation();
        let (_runner, first) = server.start(&conversation, "first", AgentAccess::Supervised);
        let updates = first.wait_for_end(TURN_BUDGET);
        assert!(
            matches!(updates.last(), Some(SessionUpdate::Finished { outcome }) if outcome.is_completed()),
            "{updates:?}"
        );
        assert!(updates.contains(&SessionUpdate::Accepted));
        assert!(updates.contains(&SessionUpdate::Changed));
        assert_eq!(lock(&conversation).session_id(), Some("native-claude"));
        let (_runner, second) = server.start(&conversation, "second", AgentAccess::Supervised);
        second.wait_for_end(TURN_BUDGET);
        assert_eq!(rows(&conversation), vec!["text: first", "text: second"]);
        let logs = server.received();
        let launches: Vec<_> = logs.iter().filter_map(|item| item.get("args")).collect();
        let args = launches[0].as_array().unwrap();
        for pair in [
            ("--model", "fixture-model"),
            ("--effort", "high"),
            ("--append-system-prompt", "fixture instructions"),
            ("--permission-mode", "manual"),
        ] {
            assert!(
                args.windows(2)
                    .any(|window| window[0] == pair.0 && window[1] == pair.1),
                "{args:?}"
            );
        }
        assert!(
            launches[1]
                .as_array()
                .unwrap()
                .windows(2)
                .any(|window| window[0] == "--resume" && window[1] == "native-claude")
        );
    }

    #[test]
    fn claude_models_without_effort_support_omit_the_flag_even_with_a_stale_saved_choice() {
        let Some(server) = ClaudeFixture::new() else {
            return;
        };
        let conversation = conversation();
        let (_runner, recorder) = server.start_with_model(
            &conversation,
            "hello",
            AgentAccess::Supervised,
            "claude-haiku-4-5",
        );
        let updates = recorder.wait_for_end(TURN_BUDGET);
        assert!(
            matches!(updates.last(), Some(SessionUpdate::Finished { outcome }) if outcome.is_completed()),
            "{updates:?}"
        );
        let logs = server.received();
        let args = logs[0]["args"].as_array().unwrap();
        assert!(!args.iter().any(|arg| arg == "--effort"), "{args:?}");
    }

    #[test]
    fn claude_supervised_tool_waits_for_the_matching_user_decision() {
        let Some(server) = ClaudeFixture::new() else {
            return;
        };
        let conversation = conversation();
        let (runner, recorder) = server.start(&conversation, "tool", AgentAccess::Supervised);
        let ProviderEvent::Approval(request) = wait_for_interaction(&recorder) else {
            panic!("approval expected")
        };
        assert_eq!(lock(&conversation).phase(), Phase::WaitingForApproval);
        assert!(runner.respond(UserResponse::Approval {
            id: "stale-id".into(),
            approved: true
        }));
        assert!(runner.respond(UserResponse::Approval {
            id: request.id.clone(),
            approved: false
        }));
        let updates = recorder.wait_for_end(TURN_BUDGET);
        assert!(
            matches!(updates.last(), Some(SessionUpdate::Finished { outcome }) if outcome.is_completed()),
            "{updates:?}"
        );
        assert!(updates.contains(&SessionUpdate::InteractionResolved { id: request.id }));
        let response = server
            .received()
            .into_iter()
            .find(|item| item["type"] == "control_response")
            .unwrap();
        assert_eq!(response["response"]["response"]["behavior"], "deny");
        assert!(lock(&conversation).pending_approvals().is_empty());
    }

    #[test]
    fn claude_full_access_still_waits_for_answers_to_every_question() {
        let Some(server) = ClaudeFixture::new() else {
            return;
        };
        let conversation = conversation();
        let (runner, recorder) = server.start(&conversation, "questions", AgentAccess::FullAccess);
        let ProviderEvent::UserInput(request) = wait_for_interaction(&recorder) else {
            panic!("questions expected")
        };
        assert_eq!(request.questions.len(), 2);
        runner.respond(UserResponse::Approval {
            id: request.id.clone(),
            approved: true,
        });
        runner.respond(UserResponse::Questions {
            id: request.id,
            answers: vec![
                (request.questions[0].id.clone(), "SQLite".into()),
                (request.questions[1].id.clone(), "eu-west".into()),
            ],
        });
        let updates = recorder.wait_for_end(TURN_BUDGET);
        assert!(
            matches!(updates.last(), Some(SessionUpdate::Finished { outcome }) if outcome.is_completed()),
            "{updates:?}"
        );
        let response = server
            .received()
            .into_iter()
            .find(|item| item["type"] == "control_response")
            .unwrap();
        let answers = &response["response"]["response"]["updatedInput"]["answers"];
        assert_eq!(answers[0]["answer"], "SQLite");
        assert_eq!(answers[1]["answer"], "eu-west");
        assert!(lock(&conversation).pending_questions().is_empty());
    }

    #[test]
    fn cancelling_claude_while_waiting_clears_the_interaction_and_settles() {
        let Some(server) = ClaudeFixture::new() else {
            return;
        };
        let conversation = conversation();
        let (runner, recorder) = server.start(&conversation, "tool", AgentAccess::Supervised);
        wait_for_interaction(&recorder);
        runner.interrupt();
        let updates = recorder.wait_for_end(TURN_BUDGET);
        assert!(
            matches!(
                updates.last(),
                Some(SessionUpdate::Finished {
                    outcome: TurnOutcome::Cancelled { .. }
                })
            ),
            "{updates:?}"
        );
        assert_eq!(lock(&conversation).phase(), Phase::Idle);
        assert!(lock(&conversation).pending_approvals().is_empty());
    }

    #[test]
    fn a_codex_session_saved_and_reopened_from_disk_resumes_the_same_native_thread() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let original = conversation();
        let (_runner, recorder) = start(&original, provider, "hello", ApprovalPolicy::AllowAll);
        let updates = recorder.wait_for_end(TURN_BUDGET);
        assert!(
            matches!(updates.last(), Some(SessionUpdate::Finished { outcome }) if outcome.is_completed())
        );
        let dir = tempfile::tempdir().unwrap();
        let store = fx_store::Store::new(fx_store::AppPaths::at(dir.path().join("state")));
        let project_id = Uuid::new_v4();
        let agent_id = Uuid::from_u128(1);
        store
            .save_conversation(
                project_id,
                agent_id,
                &crate::state::persisted_from(&lock(&original)),
            )
            .unwrap();
        let persisted = store
            .load_conversation(project_id, agent_id)
            .unwrap()
            .unwrap();
        let restored = conversation();
        crate::state::restore_conversation(&mut lock(&restored), persisted);
        let thread_id = lock(&restored).session_id().unwrap().to_owned();
        let provider = server.provider().unwrap();
        let (_runner, recorder) = start(&restored, provider, "hello", ApprovalPolicy::AllowAll);
        let updates = recorder.wait_for_end(TURN_BUDGET);
        assert!(
            matches!(updates.last(), Some(SessionUpdate::Finished { outcome }) if outcome.is_completed()),
            "{updates:?}"
        );
        let calls = server.thread_calls();
        assert_eq!(calls.last().unwrap().0, "thread/resume");
        assert_eq!(calls.last().unwrap().1["threadId"], thread_id);
        assert_eq!(rows(&restored), vec!["text: Hello", "text: Hello"]);
    }

    #[test]
    fn cancelling_during_a_stalled_handshake_does_not_wait_for_the_control_timeout() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let marker = server._dir.path().join("initializing");
        let source = std::fs::read_to_string(&server.script).unwrap().replace(
            "if method == \"initialize\":", &format!("if method == \"initialize\":\n        open({}, 'w').close()\n        time.sleep(10)", serde_json::to_string(&marker.display().to_string()).unwrap()),
        );
        std::fs::write(&server.script, source).unwrap();
        let conversation = conversation();
        let (runner, recorder) = start(&conversation, provider, "hello", ApprovalPolicy::Ask);
        let deadline = Instant::now() + TURN_BUDGET;
        while !marker.exists() {
            assert!(Instant::now() < deadline, "fixture did not start");
            std::thread::sleep(Duration::from_millis(5));
        }
        runner.interrupt();
        let updates = recorder.wait_for_end(Duration::from_secs(2));
        assert!(
            matches!(
                updates.last(),
                Some(SessionUpdate::Finished {
                    outcome: TurnOutcome::Cancelled { .. }
                })
            ),
            "{updates:?}"
        );
        assert!(!updates.contains(&SessionUpdate::Accepted));
        assert_eq!(lock(&conversation).phase(), Phase::Idle);
    }

    /// Wait for a phase, or give up.
    ///
    /// Used to make sure a worker is live before it is interrupted; without it
    /// the interrupt would race the start and the test would pass for the wrong
    /// reason.
    fn wait_for_phase(conversation: &Mutex<Conversation>, phase: Phase) {
        let deadline = Instant::now() + TURN_BUDGET;
        while Instant::now() < deadline {
            if lock(conversation).phase() == phase {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A fake `codex app-server` on disk.
    ///
    /// Copied from `fx-agent`'s own fixture rather than shared: it is test-only
    /// in that crate, and a shared fixture across crates would have to be
    /// exported as public API to be reachable from here. The only difference is
    /// the `thread/start` / `thread/resume` branch, which writes what it saw so
    /// a resumption can be asserted from the server's side rather than from the
    /// conversation's.
    struct FakeServer {
        _dir: tempfile::TempDir,
        script: PathBuf,
        log: PathBuf,
    }

    impl FakeServer {
        /// A server on disk, or `None` to skip the test.
        ///
        /// `python3` is present on every macOS install and needs no build step,
        /// so a missing one is a machine without Python rather than a failure.
        fn new() -> Option<Self> {
            Self::python()?;
            let dir = tempfile::tempdir().ok()?;
            Some(Self {
                script: dir.path().join("fake_codex.py"),
                log: dir.path().join("THREAD_LOG"),
                _dir: dir,
            })
        }

        fn python() -> Option<String> {
            let output = Command::new("/usr/bin/env")
                .arg("which")
                .arg("python3")
                .output()
                .ok()?;
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            (!path.is_empty()).then_some(path)
        }

        /// A provider that resolves to this server instead of to `codex`.
        ///
        /// Discovery is left searching only what the spec names: a test that
        /// passed because of whatever `codex` happens to be installed on the
        /// machine running it asserts nothing.
        fn provider(&self) -> Option<CodexProvider> {
            self.write();
            let spec = BinarySpec::new("codex", "Codex", vec![Self::python()?])
                .with_arguments(vec![self.script.to_string_lossy().into_owned()]);
            Some(CodexProvider::with_spec(
                RuntimeDiscovery::default().without_extra_directories(),
                spec,
            ))
        }

        /// The thread lifecycle calls the server actually saw.
        fn thread_calls(&self) -> Vec<(String, Value)> {
            let Ok(text) = std::fs::read_to_string(&self.log) else {
                return Vec::new();
            };
            text.lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .map(|entry| {
                    (
                        entry
                            .get("method")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        entry.get("params").cloned().unwrap_or(Value::Null),
                    )
                })
                .collect()
        }

        fn write(&self) {
            let source = SOURCE.replace("THREAD_LOG", &self.log.to_string_lossy());
            // `python3` runs this as data rather than as a program, so it needs
            // no execute bit — only to be readable, and the temp directory is
            // already private.
            let _ = std::fs::write(&self.script, source);
        }
    }

    /// Just enough of the protocol to drive a real turn.
    const SOURCE: &str = r#"
import json, os, sys, time

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    mid = msg.get("id")

    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"userAgent": "fake"}})
    elif method == "initialized":
        pass
    elif method in ("thread/start", "thread/resume"):
        # Recorded so a test can tell a resume from a fresh thread.
        with open("THREAD_LOG", "a") as log:
            log.write(json.dumps({"method": method,
                                  "params": msg.get("params")}) + "\n")
        requested = (msg.get("params") or {}).get("threadId")
        send({"jsonrpc": "2.0", "id": mid, "result": {"thread": {
            "id": requested or "thread-new"}}})
    elif method == "turn/start":
        prompt = ""
        for item in (msg.get("params") or {}).get("input") or []:
            if item.get("type") == "text":
                prompt = item.get("text") or ""
        send({"jsonrpc": "2.0", "id": mid, "result": {"turn": {"id": "turn-1"}}})

        if prompt == "fail-now":
            send({"jsonrpc": "2.0", "method": "turn/completed",
                  "params": {"turn": {"id": "turn-1", "status": "failed",
                                      "error": {"message": "the model refused"}}}})
        elif prompt == "echo-params":
            send({"jsonrpc": "2.0", "method": "turn/started", "params": {"turn": {"id": "turn-1"}}})
            send({"jsonrpc": "2.0", "method": "item/agentMessage/delta", "params": {"delta": json.dumps(msg.get("params"))}})
            send({"jsonrpc": "2.0", "method": "turn/completed", "params": {"turn": {"id": "turn-1", "status": "completed"}}})
        elif prompt == "hang":
            # Starts, says nothing, and never completes.
            send({"jsonrpc": "2.0", "method": "turn/started",
                  "params": {"turn": {"id": "turn-1"}}})
        elif prompt == "die-mid-turn":
            send({"jsonrpc": "2.0", "method": "turn/started",
                  "params": {"turn": {"id": "turn-1"}}})
            sys.stderr.write("server died mid turn\n")
            sys.stderr.flush()
            sys.exit(4)
        elif prompt == "use-a-tool":
            send({"jsonrpc": "2.0", "method": "turn/started",
                  "params": {"turn": {"id": "turn-1"}}})
            send({"jsonrpc": "2.0", "id": "srv-1",
                  "method": "execCommandApproval",
                  "params": {"callId": "c1", "toolName": "shell",
                             "arguments": {"command": ["ls"]}}})
            send({"jsonrpc": "2.0", "method": "turn/completed",
                  "params": {"turn": {"id": "turn-1", "status": "completed"}}})
        elif prompt == "check-then-report":
            # Prose, a call, a result, then more prose — the interleaving the
            # transcript has to reproduce.
            send({"jsonrpc": "2.0", "method": "turn/started",
                  "params": {"turn": {"id": "turn-1"}}})
            send({"jsonrpc": "2.0", "method": "item/agentMessage/delta",
                  "params": {"delta": "Let me check."}})
            send({"jsonrpc": "2.0", "method": "item/started",
                  "params": {"item": {"id": "tool-1", "type": "commandExecution",
                                      "command": "cat Cargo.toml"}}})
            send({"jsonrpc": "2.0", "method": "item/completed",
                  "params": {"item": {"id": "tool-1", "type": "commandExecution",
                                      "command": "cat Cargo.toml",
                                      "aggregatedOutput": "name = \"flowx\""}}})
            send({"jsonrpc": "2.0", "method": "item/agentMessage/delta",
                  "params": {"delta": "Found it."}})
            send({"jsonrpc": "2.0", "method": "turn/completed",
                  "params": {"turn": {"id": "turn-1", "status": "completed"}}})
        else:
            send({"jsonrpc": "2.0", "method": "turn/started",
                  "params": {"turn": {"id": "turn-1"}}})
            send({"jsonrpc": "2.0", "method": "item/agentMessage/delta",
                  "params": {"delta": "Hello"}})
            send({"jsonrpc": "2.0", "method": "turn/completed",
                  "params": {"turn": {"id": "turn-1", "status": "completed",
                                      "usage": {"input_tokens": 12, "output_tokens": 3}}}})
    elif method == "turn/interrupt":
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
    else:
        send({"jsonrpc": "2.0", "id": mid,
              "error": {"code": -32601, "message": "unknown method"}})
"#;

    #[test]
    fn the_embedded_fixture_is_valid_python() {
        // Not paranoia. A missing quote in the script makes every process test
        // here fail by timing out, which reads as a runner bug rather than as a
        // typo in a string literal.
        if FakeServer::python().is_none() {
            return;
        }
        let script = std::env::temp_dir().join(format!("fx-fake-codex-{}.py", std::process::id()));
        assert!(std::fs::write(&script, SOURCE).is_ok());
        let output = Command::new("python3")
            .args(["-m", "py_compile", &script.to_string_lossy()])
            .output();
        let _ = std::fs::remove_file(&script);
        let output = output.expect("python3 ran");
        assert!(
            output.status.success(),
            "the fixture does not compile: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn a_tool_error_is_not_reported_as_a_refusal() {
        // The one shape that must not be mistaken for FlowX saying no: a tool
        // that failed says something of its own, and reporting it as a refusal
        // would tell the user the model was stopped when it was not.
        assert!(
            refused(&ProviderEvent::ToolResult {
                id: "t1".into(),
                content: "permission denied".into(),
                is_error: true,
            })
            .is_none()
        );
        // An allowed tool result says nothing and is not an error at all.
        assert!(
            refused(&ProviderEvent::ToolResult {
                id: "t1".into(),
                content: String::new(),
                is_error: false,
            })
            .is_none()
        );
        assert_eq!(
            tool_in_refusal("FlowX refused this tool call. (commandExecution)").as_deref(),
            Some("commandExecution")
        );
        assert_eq!(tool_in_refusal("FlowX refused this tool call."), None);
        assert_eq!(tool_in_refusal("FlowX refused this tool call. ()"), None);
    }

    #[test]
    fn a_user_row_is_not_produced_by_a_turn() {
        // The runner reports a turn; it does not invent the user's message.
        // The composer owns that row, because it also owns retry and the
        // queued-prompt list, and two owners would mean two copies of it.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let conversation = conversation();

        let (_runner, recorder) = start(&conversation, provider, "hello", ApprovalPolicy::AllowAll);
        recorder.wait_for_end(TURN_BUDGET);

        assert!(
            !lock(&conversation)
                .messages()
                .any(|message| message.role == MessageRole::User),
            "the runner echoed the prompt into the transcript"
        );
    }

    #[test]
    fn a_runner_can_be_cloned_and_stopped_from_another_thread() {
        // `interrupt` is called from wherever the UI happens to be, including
        // off the main thread, and cloning is how a view keeps a handle.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let conversation = conversation();

        let (runner, recorder) = start(&conversation, provider, "hang", ApprovalPolicy::AllowAll);
        let clone = runner.clone();
        wait_for_phase(&conversation, Phase::Running);
        std::thread::spawn(move || clone.interrupt())
            .join()
            .expect("the interrupting thread finished");

        let updates = recorder.wait_for_end(TURN_BUDGET);
        assert!(
            matches!(updates.last(), Some(SessionUpdate::Finished { .. })),
            "an off-thread stop did not end the turn: {updates:?}"
        );
    }
}
