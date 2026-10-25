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
    ClaudeTransport, Launch as ClaudeLaunch, StreamItem, TransportError as ClaudeTransportError,
    TurnRequest as ClaudeTurnRequest,
};
use fx_agent::provider::UserResponse;

use fx_agent::codex::provider::CLIENT_NAME;
use fx_agent::codex::session::TurnRequest;
use fx_agent::codex::turn::TurnSteerer;
use fx_agent::codex::{ApprovalPolicy, CodexProvider, CodexSession, Transport, TurnOutcome};
use fx_agent::{Conversation, Phase, ProviderEvent};
use fx_core::services::runtime::{BinarySpec, RuntimeDiscovery, claude_spec};
use fx_core::{Agent, AgentAccess, DEFAULT_CODEX_MODEL_ID};

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
const ANSWER_FAILED: &str = "FlowX could not deliver your answer to Claude Code.";
const STEER_AFTER_INPUT_CLOSED: &str =
    "Claude Code is finishing its background tasks and takes no more guidance this turn.";

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
    /// A separate ephemeral naming request completed; it never changes history.
    AutoTitle {
        generation: uuid::Uuid,
        result: Result<String, String>,
    },
    /// Streaming content changed; wake the matching conversation view.
    Changed,
    /// The provider accepted the prompt; its retained recovery copy may clear.
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
    /// Guidance sent into the running turn was accepted, or why it was not.
    ///
    /// On success the runner has already added the user's row, at the point in
    /// the stream where the provider took it.
    Steered {
        id: uuid::Uuid,
        result: Result<(), String>,
    },
    /// A thread control reported the thread's goal, or that it has none.
    GoalChanged {
        goal: Option<fx_core::models::ConversationGoal>,
    },
}

/// A thread-level request that is not a prompt: what `/compact` and `/goal`
/// ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadControl {
    /// Compact the context, with what to keep in the summary, if anything.
    ///
    /// Claude's `/compact` takes these; dropping them compacted with Claude's
    /// default summary instead of the one asked for.
    Compact(String),
    GoalView,
    GoalSet(String),
    GoalStatus(fx_core::models::GoalStatus),
    GoalClear,
}

impl ThreadControl {
    /// Read a slash command from a submitted draft, or `None` for a prompt.
    ///
    /// Only the two commands FlowX itself handles are recognised. Anything
    /// else that starts with a slash is a prompt, because providers have
    /// slash commands of their own and swallowing those would be worse than
    /// passing them through.
    pub fn parse(draft: &str) -> Option<Self> {
        let trimmed = draft.trim();
        let rest = trimmed.strip_prefix('/')?;
        let (command, argument) = match rest.split_once(char::is_whitespace) {
            Some((command, argument)) => (command, argument.trim()),
            None => (rest, ""),
        };
        match command.to_lowercase().as_str() {
            "compact" => Some(Self::Compact(argument.to_string())),
            "goal" => Some(match argument.to_lowercase().as_str() {
                "" => Self::GoalView,
                "pause" => Self::GoalStatus(fx_core::models::GoalStatus::Paused),
                "resume" => Self::GoalStatus(fx_core::models::GoalStatus::Active),
                "clear" => Self::GoalClear,
                _ => Self::GoalSet(argument.to_string()),
            }),
            _ => None,
        }
    }

    /// Whether the provider can carry this control at all.
    ///
    /// Claude Code compacts through its own `/compact` command, sent as the
    /// turn's prompt, instructions and all. It has no goals.
    pub fn supported_by(&self, provider: &str) -> bool {
        match provider {
            "claude" => matches!(self, Self::Compact(_)),
            _ => true,
        }
    }

    /// Whether the provider can take this control's instructions as well.
    ///
    /// Codex's `thread/compact/start` has nowhere to put them, so a request
    /// with some is refused rather than carried out without them.
    pub fn instructions_supported_by(&self, provider: &str) -> bool {
        match self {
            Self::Compact(instructions) => provider == "claude" || instructions.is_empty(),
            _ => true,
        }
    }

    /// The prompt that runs this control as Claude Code's own command.
    pub fn claude_prompt(&self) -> Option<String> {
        match self {
            Self::Compact(instructions) if instructions.is_empty() => Some("/compact".into()),
            Self::Compact(instructions) => Some(format!("/compact {instructions}")),
            _ => None,
        }
    }
}

/// What a runner is asked to do: a prompt with its images, or a control.
#[derive(Debug, Clone, Default)]
pub struct Work {
    pub prompt: String,
    pub attachments: Vec<fx_core::Attachment>,
    pub control: Option<ThreadControl>,
}

impl Work {
    pub fn prompt(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            ..Self::default()
        }
    }

    pub fn with_attachments(mut self, attachments: Vec<fx_core::Attachment>) -> Self {
        self.attachments = attachments;
        self
    }

    pub fn control(control: ThreadControl) -> Self {
        Self {
            control: Some(control),
            ..Self::default()
        }
    }
}

/// Guidance for the running turn, as the UI asked for it.
#[derive(Debug)]
struct Steer {
    id: uuid::Uuid,
    text: String,
    attachments: Vec<fx_core::Attachment>,
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
    steers: tokio::sync::mpsc::UnboundedSender<Steer>,
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
        Self::start_work(
            _cx,
            conversation,
            project_root,
            agent,
            Work::prompt(prompt),
            approvals,
            on_update,
        )
    }

    /// Start a turn with images, or a thread control, in the background.
    ///
    /// The same runner as [`Self::start`]: a control shows as a running turn,
    /// can be stopped the same way, and reports through the same updates.
    pub fn start_work(
        _cx: &mut gpui::App,
        conversation: Arc<Mutex<Conversation>>,
        project_root: String,
        agent: Agent,
        work: Work,
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
            work,
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

    /// A runner with no turn behind it, and the receiver its answers reach.
    #[cfg(test)]
    pub(crate) fn detached() -> (Self, tokio::sync::mpsc::UnboundedReceiver<UserResponse>) {
        let (responses, received) = tokio::sync::mpsc::unbounded_channel();
        let runner = Self {
            interrupt: tokio::sync::mpsc::unbounded_channel().0,
            responses,
            steers: tokio::sync::mpsc::unbounded_channel().0,
        };
        (runner, received)
    }

    /// Send guidance into the running turn. `false` once the runner has gone.
    ///
    /// The answer arrives later as [`SessionUpdate::Steered`] with this `id`:
    /// the provider has to accept the guidance before it is part of the turn.
    pub fn steer(
        &self,
        id: uuid::Uuid,
        text: String,
        attachments: Vec<fx_core::Attachment>,
    ) -> bool {
        self.steers
            .send(Steer {
                id,
                text,
                attachments,
            })
            .is_ok()
    }

    /// The single path into a worker, shared by `start` and its test seam.
    fn launch(
        conversation: Arc<Mutex<Conversation>>,
        project_root: String,
        agent: Agent,
        work: Work,
        approvals: ApprovalPolicy,
        provider: RuntimeProvider,
        on_update: impl Fn(SessionUpdate) + Send + 'static,
    ) -> Self {
        let (handle, interrupt) = tokio::sync::mpsc::unbounded_channel();
        let (response_handle, responses) = tokio::sync::mpsc::unbounded_channel();
        let (steer_handle, steers) = tokio::sync::mpsc::unbounded_channel();
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
            prompt: work.prompt,
            attachments: work.attachments,
            control: work.control,
            steers: Mutex::new(Some(steers)),
            approvals,
            access: agent.configuration.resolved_access(AgentAccess::default()),
            model,
            effort,
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
        // the composer offers to send it again. The previous turn's background
        // tasks go with it: they belonged to a process that has exited.
        {
            let mut conversation = lock(&job.conversation);
            conversation.clear_tasks();
            conversation.set_phase(Phase::Starting);
        }

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
            steers: steer_handle,
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
            Work::prompt(prompt),
            approvals,
            RuntimeProvider::Codex(provider),
            on_update,
        )
    }

    /// [`Self::launch_with`] for a turn with images, or for a control.
    #[cfg(test)]
    fn launch_work_with(
        conversation: Arc<Mutex<Conversation>>,
        project_root: String,
        agent: Agent,
        work: Work,
        approvals: ApprovalPolicy,
        provider: CodexProvider,
        on_update: impl Fn(SessionUpdate) + Send + 'static,
    ) -> Self {
        Self::launch(
            conversation,
            project_root,
            agent,
            work,
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
    /// Images sent with the prompt, written to private files for the turn.
    attachments: Vec<fx_core::Attachment>,
    /// A thread control to run instead of a prompt.
    control: Option<ThreadControl>,
    /// Guidance for the running turn, taken by whichever loop drives it.
    steers: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<Steer>>>,
    approvals: ApprovalPolicy,
    access: AgentAccess,
    model: String,
    effort: Option<String>,
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
        // Taken once, here: whichever loop drives the turn owns the receiver.
        // A runner built without one gets a closed channel, which never fires.
        let steers = self
            .steers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .unwrap_or_else(|| tokio::sync::mpsc::unbounded_channel().1);
        if let Some(control) = self.control.clone() {
            return self.run_control(control, interrupts).await;
        }
        match &self.provider {
            RuntimeProvider::Codex(provider) => {
                self.run_codex(provider, interrupts, responses, steers)
                    .await
            }
            RuntimeProvider::Claude { discovery, spec } => {
                self.run_claude(discovery, spec, interrupts, responses, steers)
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
        mut steers: tokio::sync::mpsc::UnboundedReceiver<Steer>,
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

        // Whether `Accepted` went out from inside the turn, so a Stop that has to
        // report it afterwards does not report it twice. Atomic only because
        // the turn's closures and this frame share it; nothing else runs here.
        let accepted = std::sync::atomic::AtomicBool::new(false);

        // Everything borrowing `session` or `self` is scoped to this block.
        // `run_turn` holds a `&mut session` for as long as its future lives, and
        // interrupting needs a `&mut session` — so the request, the future, and
        // the borrows they carry must all be dropped before `session` can be
        // touched again. That is the whole reason for the block.
        // Codex reads a `localImage` from disk, so the images live in private
        // files until the turn is over.
        let files = match attachment_files(&self.attachments) {
            Ok(files) => files,
            Err(error) => {
                session.close().await;
                return self.fail(TURN_FAILED, error);
            }
        };
        let image_paths = file_paths(&files);
        let steerer = session.steerer();
        let mut steering = Steering::default();
        let ending = {
            let images: &[String] = &image_paths;
            let request = TurnRequest::new(&thread_id, &self.prompt, &self.project_root)
                .images(&images)
                .model(&self.model)
                .effort(self.effort.as_deref().unwrap_or_default())
                .access(self.access);
            let mut turn = Box::pin(session.run_turn_interactive(
                &request,
                &mut responses,
                |event| self.apply(event),
                || {
                    accepted.store(true, std::sync::atomic::Ordering::Relaxed);
                    emit(&self.on_update, SessionUpdate::Accepted);
                },
                |id| self.resolve(id),
            ));

            let ending = loop {
                tokio::select! {
                    // An answered steer first: the reader resolves its response
                    // before it queues the reply that follows, so taking it
                    // first is what puts the guidance's row before that reply.
                    // Then the turn, so an interrupt arriving while the turn is
                    // producing output cannot starve it.
                    biased;
                    (id, result) = steering.next_answer(), if steering.in_flight() => {
                        self.steer_settled(&mut steering, id, result);
                    }
                    ending = &mut turn => break Ending::Ran(ending),
                    _ = interrupts.recv() => break Ending::Interrupted,
                    // Steering is a request made while the turn's own future
                    // reads the stream, so it runs beside it rather than in it.
                    Some(steer) = steers.recv() => {
                        self.begin_codex_steer(steer, &steerer, &mut steering);
                    }
                }
            };
            drop(turn);
            ending
        };
        self.abandon_steers(&mut steering, &mut steers).await;
        drop(files);

        // A turn that failed or was stopped part-way can leave a tool running,
        // so those endings take down the whole process group. A finished turn
        // has nothing running and only closes the server.
        match ending {
            Ending::Ran(Ok(TurnOutcome::Failed { reason, .. })) => {
                session.terminate().await;
                // The reason is Codex's own wording — a model refusal, an exit
                // — and prefixing it would only bury the part that tells the
                // user what happened.
                self.fail("", reason);
            }
            Ending::Ran(Ok(outcome)) => {
                session.close().await;
                self.settle(&outcome);
            }
            Ending::Ran(Err(error)) => {
                session.terminate().await;
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
                // A Stop between writing `turn/start` and its reply: the
                // interrupt collected that reply, and a prompt Codex has must not
                // be offered back, or sending it again delivers it twice.
                if session.prompt_accepted() && !accepted.load(std::sync::atomic::Ordering::Relaxed)
                {
                    emit(&self.on_update, SessionUpdate::Accepted);
                }
                session.terminate().await;
                self.settle(&TurnOutcome::Cancelled { thread_id });
            }
        }
    }

    async fn run_claude(
        &self,
        discovery: &RuntimeDiscovery,
        spec: &BinarySpec,
        mut interrupts: tokio::sync::mpsc::UnboundedReceiver<()>,
        mut responses: tokio::sync::mpsc::UnboundedReceiver<UserResponse>,
        mut steers: tokio::sync::mpsc::UnboundedReceiver<Steer>,
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
        // Share Swift's reservation directory and keep the lease alive for the
        // whole turn. A native transcript may still have an external writer.
        let _session_lease = if let Some(id) = resume.as_deref() {
            let Some(home) = runtime.environment.get("HOME") else {
                return self.fail(
                    TURN_FAILED,
                    "Cannot locate the Claude session reservation directory.",
                );
            };
            let directory = std::path::Path::new(home)
                .join("Library/Application Support/FlowX/ProviderSessionLocks/Claude");
            let lease =
                match crate::claude_session_lease::ClaudeSessionLease::acquire(&directory, id) {
                    Ok(lease) => lease,
                    Err(error) => return self.fail(TURN_FAILED, error),
                };
            let inactive = tokio::select! {
                result = fx_agent::claude::native::ensure_session_inactive(&runtime, &spec.arguments, id) => result,
                _ = interrupts.recv() => return self.cancel_before_start(),
            };
            if let Err(error) = inactive {
                return self.fail(TURN_FAILED, error.to_string());
            }
            Some(lease)
        } else {
            None
        };
        let request = ClaudeTurnRequest {
            model: (!self.model.is_empty()).then(|| self.model.clone()),
            effort: self.effort.clone(),
            system_prompt: self.system_prompt.clone(),
            access: Some(self.access),
            resume_session_id: resume.clone(),
            ..Default::default()
        };
        let mut args = spec.arguments.clone();
        args.extend(request.arguments());
        let mut background = BackgroundWait::new(background_ceiling(runtime.environment.iter()));
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
        // Claude reads each image into the envelope as it is written, so the
        // files are only needed until the prompt is sent.
        let sent = match attachment_files(&self.attachments) {
            Ok(files) => transport
                .send_prompt(&self.prompt, &files)
                .await
                .map_err(|error| error.to_string()),
            Err(error) => Err(error),
        };
        if let Err(error) = sent {
            transport.close().await;
            return self.fail(TURN_FAILED, error);
        }
        let mut thread_id = resume.unwrap_or_default();
        let mut accepted = false;
        let mut waiting = std::collections::HashSet::new();
        // A closed channel is ready forever, so selecting on it again would
        // spin a CPU for the rest of the turn. Once it closes it is left out.
        let mut responses_open = true;
        // Whether the turn ends by stopping the whole process group, which a
        // turn that gave up on its background tasks has to: they are still
        // running, in Claude's group.
        let mut stop_group = false;
        // Silence is not a failure: thinking and long tools can take minutes,
        // and the parser intentionally omits non-display frames. Only a result
        // with no background work outstanding, a provider/process error, a
        // closed stream, Stop, or the background ceiling ends this turn.
        let ending = loop {
            tokio::select! {
                _ = interrupts.recv() => {
                    lock(&self.conversation).set_phase(Phase::Cancelling);
                    emit(&self.on_update, SessionUpdate::Changed);
                    break TurnOutcome::Cancelled { thread_id };
                }
                // Claude sits idle with tasks running, and has for as long as
                // its own setting allows: stop waiting, as it would.
                () = until(background.deadline()) => {
                    self.stop_waiting(background.ceiling.unwrap_or_default());
                    stop_group = true;
                    let stop_reason = background.stop_reason.clone().unwrap_or_default();
                    break TurnOutcome::Completed { thread_id, stop_reason };
                }
                response = responses.recv(), if responses_open => {
                    let Some(response) = response else { responses_open = false; continue; };
                    let id = response.id().to_owned();
                    if !waiting.contains(&id) { continue; }
                    let result = match response {
                        UserResponse::Approval { approved, .. } => transport.approve(&id, approved).await,
                        UserResponse::Questions { answers, .. } => transport.answer_questions(&id, &answers).await,
                        UserResponse::CancelQuestions { .. } => transport.decline_questions(&id).await,
                    };
                    match result {
                        Ok(()) => { waiting.remove(&id); self.resolve(id); }
                        // The wrong kind of answer leaves the request open, as
                        // Codex's does: Claude is still blocked on it, and the
                        // panel sends only the kind it shows.
                        Err(ClaudeTransportError::NotAControl { .. }) => {}
                        // Claude holds no such request, so nobody is waiting.
                        Err(ClaudeTransportError::UnknownControl { .. }) => { waiting.remove(&id); self.resolve(id); }
                        // The answer could not be delivered, so Claude will
                        // never get it. Left open, the request kept both of the
                        // panel's buttons disabled with nothing left to clear
                        // them; the turn fails and says why instead.
                        Err(error) => break TurnOutcome::Failed { thread_id, reason: format!("{ANSWER_FAILED} {error}") },
                    }
                }
                // Guidance is one more user message on the live stream-json
                // input: Claude reads stdin for the whole turn, and for as
                // long as a background task runs after it.
                Some(steer) = steers.recv() => {
                    let result = if background.input_closed {
                        Err(STEER_AFTER_INPUT_CLOSED.to_string())
                    } else {
                        match attachment_files(&steer.attachments) {
                            Ok(files) => transport
                                .send_message(steer.id, &steer.text, &files)
                                .await
                                .map_err(|error| error.to_string()),
                            Err(error) => Err(error),
                        }
                    };
                    if result.is_ok() {
                        background.steered();
                    }
                    self.steer_finished(steer, result);
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
                            // Any frame that answers the prompt proves Claude read
                            // it: thinking, usage and lifecycle frames as much as
                            // text. Counting only content let a Stop during long
                            // thinking hand back a prompt Claude already had.
                            if !accepted && transport.prompt_consumed().await {
                                accepted = true; emit(&self.on_update, SessionUpdate::Accepted);
                            }
                            if let ProviderEvent::Approval(ref request) = event {
                                let auto = if self.approvals == ApprovalPolicy::Ask {
                                    (self.access == AgentAccess::FullAccess).then_some(true)
                                } else { Some(self.approvals.allows_claude_tool(&request.tool_name, &request.input)) };
                                if let Some(approved) = auto {
                                    if let Err(error) = transport.approve(&request.id, approved).await {
                                        break TurnOutcome::Failed { thread_id, reason: error.to_string() };
                                    }
                                    continue;
                                }
                                waiting.insert(request.id.clone());
                            } else if let ProviderEvent::UserInput(ref request) = event {
                                waiting.insert(request.id.clone());
                            }
                            match event {
                                ProviderEvent::Done { stop_reason } => match background.result(stop_reason.clone(), transport.input_open().await) {
                                    AfterResult::End => {
                                        self.apply(ProviderEvent::Done { stop_reason: stop_reason.clone() });
                                        break TurnOutcome::Completed { thread_id, stop_reason };
                                    }
                                    // The prose so far is whole, but the thread is
                                    // still working: what the tasks found comes
                                    // back as a turn of its own.
                                    AfterResult::Wait | AfterResult::Drain => self.apply(ProviderEvent::TextEnd),
                                },
                                ProviderEvent::Error(reason) => {
                                    self.apply(ProviderEvent::Error(reason.clone()));
                                    break TurnOutcome::Failed { thread_id, reason };
                                }
                                ProviderEvent::Task(update) => {
                                    background.task(&update);
                                    self.apply(ProviderEvent::Task(update));
                                }
                                // `init` opens every turn, an acknowledgement
                                // that asks the model nothing included, so it is
                                // not a sign a follow-up is under way.
                                event @ ProviderEvent::Lifecycle(_) => self.apply(event),
                                event => {
                                    background.content();
                                    self.apply(event);
                                }
                            }
                        }
                        Some(StreamItem::Exited(exit)) => match background.completed_at_exit() {
                            Some(stop_reason) => break TurnOutcome::Completed { thread_id, stop_reason },
                            None => break TurnOutcome::Failed { thread_id, reason: exit.reason() },
                        },
                        None => match background.completed_at_exit() {
                            Some(stop_reason) => break TurnOutcome::Completed { thread_id, stop_reason },
                            None => break TurnOutcome::Failed { thread_id, reason: "Claude Code ended before reporting a result.".into() },
                        },
                    }
                }
            }
        };
        // The frames that proved it may have arrived without an event of their
        // own — thinking, mostly — so the question is asked once more here.
        if !accepted && transport.prompt_consumed().await {
            emit(&self.on_update, SessionUpdate::Accepted);
        }
        match ending {
            TurnOutcome::Completed { .. } if !stop_group => transport.close().await,
            // Interrupted first, so Claude can end its own turn and tools;
            // then the process group, for whatever did not.
            TurnOutcome::Completed { .. } | TurnOutcome::Cancelled { .. } => {
                let _ = transport.interrupt().await;
                transport.terminate().await;
            }
            TurnOutcome::Failed { .. } => transport.terminate().await,
        }
        self.abandon_steers(&mut Steering::default(), &mut steers)
            .await;
        match ending {
            TurnOutcome::Failed { reason, .. } => self.fail("", reason),
            outcome => self.settle(&outcome),
        }
    }

    // ── Steering ───────────────────────────────────────────────────────────

    /// Start one `turn/steer` beside the running turn.
    fn begin_codex_steer(&self, steer: Steer, steerer: &TurnSteerer, steering: &mut Steering) {
        let files = match attachment_files(&steer.attachments) {
            Ok(files) => files,
            Err(error) => return self.steer_finished(steer, Err(error)),
        };
        let paths = file_paths(&files);
        steering.files.extend(files);
        let steerer = steerer.clone();
        let id = steer.id;
        let text = steer.text.clone();
        steering.pending.insert(id, steer);
        steering.requests.push(Box::pin(async move {
            let result = steerer
                .steer(&id.to_string(), &text, &paths)
                .await
                .map_err(|error| error.to_string());
            (id, result)
        }));
    }

    /// One steer request came back.
    fn steer_settled(&self, steering: &mut Steering, id: uuid::Uuid, result: Result<(), String>) {
        if let Some(steer) = steering.pending.remove(&id) {
            self.steer_finished(steer, result);
        }
    }

    /// Report guidance the turn ended before accepting.
    ///
    /// Every steer gets an answer: the UI holds the draft until it does, and a
    /// steer that was never answered would hold it forever.
    async fn abandon_steers(
        &self,
        steering: &mut Steering,
        steers: &mut tokio::sync::mpsc::UnboundedReceiver<Steer>,
    ) {
        // A request already sent may still be accepted; give it a moment
        // rather than reporting a failure Codex would then contradict.
        let _ = tokio::time::timeout(STEER_SETTLE, async {
            while steering.in_flight() {
                let (id, result) = steering.next_answer().await;
                self.steer_settled(steering, id, result);
            }
        })
        .await;
        steering.requests.clear();
        let mut late: Vec<Steer> = steering.pending.drain().map(|(_, steer)| steer).collect();
        while let Ok(steer) = steers.try_recv() {
            late.push(steer);
        }
        for steer in late {
            self.steer_finished(
                steer,
                Err("The turn ended before the guidance could be accepted.".into()),
            );
        }
    }

    /// Hold accepted guidance for the transcript, then report the outcome.
    ///
    /// Accepted is not read. Both providers accept a steer at once and read
    /// it at their next request, after whatever they are saying or running,
    /// and name it by the id it was sent with when they do. The row waits for
    /// that, drawn after the live reply, so it lands where the provider's own
    /// history puts it rather than splitting a sentence finished before it.
    fn steer_finished(&self, steer: Steer, result: Result<(), String>) {
        if result.is_ok() {
            let message = fx_core::ConversationMessage {
                id: steer.id,
                ..crate::attachments::user_message(&steer.text, &steer.attachments)
            };
            lock(&self.conversation).hold_steer(message);
        }
        emit(
            &self.on_update,
            SessionUpdate::Steered {
                id: steer.id,
                result,
            },
        );
        emit(&self.on_update, SessionUpdate::Changed);
    }

    // ── Thread controls ────────────────────────────────────────────────────

    async fn run_control(
        &self,
        control: ThreadControl,
        interrupts: tokio::sync::mpsc::UnboundedReceiver<()>,
    ) {
        let resume = lock(&self.conversation).session_id().map(str::to_owned);
        if matches!(control, ThreadControl::Compact(_)) && resume.is_none() {
            lock(&self.conversation)
                .record_activity("Nothing to compact: this thread has no session yet.", false);
            return self.settle(&TurnOutcome::Completed {
                thread_id: String::new(),
                stop_reason: "skipped".into(),
            });
        }
        match &self.provider {
            RuntimeProvider::Codex(provider) => {
                self.run_codex_control(provider, control, resume, interrupts)
                    .await
            }
            // Claude compacts through its own `/compact` prompt, which the UI
            // sends as a turn, and it has no goals.
            _ => self.fail(
                "This provider does not support that command.",
                "Thread goals are available for Codex threads.",
            ),
        }
    }

    async fn run_codex_control(
        &self,
        provider: &CodexProvider,
        control: ThreadControl,
        resume: Option<String>,
        mut interrupts: tokio::sync::mpsc::UnboundedReceiver<()>,
    ) {
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
        let thread_id = session.thread_id().to_string();
        if resume.as_deref() != Some(thread_id.as_str()) {
            lock(&self.conversation).set_session_id(thread_id.clone());
            emit(
                &self.on_update,
                SessionUpdate::Started {
                    thread_id: thread_id.clone(),
                },
            );
        }
        let outcome = {
            let mut work = Box::pin(self.perform_control(&mut session, &control));
            tokio::select! {
                biased;
                outcome = &mut work => Some(outcome),
                _ = interrupts.recv() => None,
            }
        };
        session.close().await;
        match outcome {
            Some(Ok(())) => self.settle(&TurnOutcome::Completed {
                thread_id,
                stop_reason: "control".into(),
            }),
            Some(Err(error)) => self.fail("FlowX could not run that command.", error.to_string()),
            None => self.settle(&TurnOutcome::Cancelled { thread_id }),
        }
    }

    async fn perform_control(
        &self,
        session: &mut CodexSession,
        control: &ThreadControl,
    ) -> Result<(), fx_agent::ProviderError> {
        let goal = match control {
            ThreadControl::Compact(_) => {
                lock(&self.conversation).set_phase(Phase::Compacting);
                emit(&self.on_update, SessionUpdate::Changed);
                return session.compact(|event| self.apply(event)).await;
            }
            ThreadControl::GoalView => session.goal().await?,
            ThreadControl::GoalSet(objective) => Some(
                session
                    .set_goal(Some(objective), Some(fx_core::models::GoalStatus::Active))
                    .await?,
            ),
            ThreadControl::GoalStatus(status) => Some(session.set_goal(None, Some(*status)).await?),
            ThreadControl::GoalClear => {
                session.clear_goal().await?;
                None
            }
        };
        let summary = match (control, &goal) {
            (ThreadControl::GoalClear, _) => "Goal cleared".to_string(),
            (_, None) => "This thread has no goal.".to_string(),
            (ThreadControl::GoalSet(_), Some(goal)) => format!("Goal set: {}", goal.objective),
            (_, Some(goal)) => format!(
                "Goal {}: {}",
                goal.status.label().to_lowercase(),
                goal.objective
            ),
        };
        lock(&self.conversation).record_activity(summary, false);
        emit(&self.on_update, SessionUpdate::GoalChanged { goal });
        Ok(())
    }

    fn cancel_before_start(&self) {
        let thread_id = lock(&self.conversation)
            .session_id()
            .unwrap_or_default()
            .to_owned();
        self.settle(&TurnOutcome::Cancelled { thread_id });
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

    /// Give up on background tasks that outlived Claude's own wait, and say so
    /// where the reader looks for them.
    fn stop_waiting(&self, ceiling: Duration) {
        let note = format!(
            "Still running after {} of waiting, so FlowX stopped it, as Claude Code would.",
            spoken_duration(ceiling)
        );
        let mut conversation = lock(&self.conversation);
        let running = conversation.running_task_count();
        conversation.stop_running_tasks(&note);
        // Nothing running means the wait was for the answer to one that ended.
        let waited_for = match running {
            0 => "a finished background task's answer".to_string(),
            1 => "1 background task".to_string(),
            _ => format!("{running} background tasks"),
        };
        conversation.record_activity(
            format!(
                "Stopped waiting for {waited_for} after {}",
                spoken_duration(ceiling)
            ),
            false,
        );
        drop(conversation);
        emit(&self.on_update, SessionUpdate::Changed);
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
        // Guidance no request read is still the reader's, and stays where they
        // can see it: at the end, after the turn's last words.
        conversation.place_steers();
        // The process is gone, and the tasks with it. A spinner left on one
        // would never stop.
        conversation.stop_running_tasks(match outcome {
            TurnOutcome::Cancelled { .. } => "Stopped with the turn.",
            _ => "Ended with the turn.",
        });
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
        conversation.place_steers();
        conversation.stop_running_tasks("Ended when the turn failed.");
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

/// How long a steer already sent may take to be answered once its turn ends.
const STEER_SETTLE: Duration = Duration::from_secs(2);

/// The variable Claude Code reads for how long a print-mode run waits on
/// background tasks, in milliseconds; `0` waits indefinitely.
const BACKGROUND_CEILING_VARIABLE: &str = "CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS";

/// How long a Claude turn waits on background tasks, or `None` for as long as
/// they run.
///
/// Only a ceiling the user set in the environment applies. The CLI's own
/// default of ten minutes is for a `-p` run that has to end with nobody
/// watching; FlowX keeps the input open while tasks run, the tasks are on
/// screen, and Stop ends them. Applying that default here stopped six fix
/// agents mid-edit ten minutes after the turn's last reply, with nothing
/// wrong. Like the CLI, `0` and a value that does not parse mean no ceiling.
fn background_ceiling<'a>(
    mut environment: impl Iterator<Item = (&'a String, &'a String)>,
) -> Option<Duration> {
    environment
        .find(|(key, _)| key.as_str() == BACKGROUND_CEILING_VARIABLE)
        .and_then(|(_, value)| value.trim().parse::<u64>().ok())
        .filter(|milliseconds| *milliseconds > 0)
        .map(Duration::from_millis)
}

/// What a Claude turn knows about the background work it started, in the order
/// the stream told it.
///
/// Claude reports a background task's end as a turn of its own, so a `result`
/// is not the end of the user's turn while one runs. Kept apart from the loop
/// so the rules can be read, and tested, without a process.
#[derive(Debug)]
struct BackgroundWait {
    running: std::collections::HashSet<String>,
    /// A background task started during this turn. Its notification may have
    /// arrived before a `result` and still be answered after it, so a turn
    /// that started one reads to the process's exit rather than stopping it.
    started_any: bool,
    /// The last `result`'s stop reason.
    stop_reason: Option<String>,
    /// No turn is in progress: the last thing Claude said was a `result`.
    between_turns: bool,
    /// The transport closed stdin at a `result` with nothing outstanding, and
    /// Claude will exit once it has answered what it still has queued.
    input_closed: bool,
    /// Guidance went in during this turn. One written near the end may be
    /// answered after the `result` FlowX is reading, as a turn of its own:
    /// stopping the process at that `result` killed the answer to a steer
    /// that had been reported accepted.
    steered: bool,
    /// The last `result` left something to wait for: a running task, or the
    /// answer to one that has ended.
    waiting: bool,
    ceiling: Option<Duration>,
    /// When FlowX stops waiting, while Claude sits idle with work outstanding.
    deadline: Option<tokio::time::Instant>,
}

/// What a `result` means for the user's turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterResult {
    /// Nothing was started in the background: the turn is over.
    End,
    /// A task is still running; its end will be a turn of its own.
    Wait,
    /// Nothing is running, but a follow-up may still be queued. Claude's input
    /// is closed, so it exits once it is done.
    Drain,
}

impl BackgroundWait {
    fn new(ceiling: Option<Duration>) -> Self {
        Self {
            running: std::collections::HashSet::new(),
            started_any: false,
            stop_reason: None,
            between_turns: false,
            input_closed: false,
            steered: false,
            waiting: false,
            ceiling,
            deadline: None,
        }
    }

    /// One report about a task.
    fn task(&mut self, update: &fx_agent::TaskUpdate) {
        if update.status.is_running() {
            self.started_any = true;
            self.running.insert(update.id.clone());
        } else if self.running.remove(&update.id) && self.between_turns {
            // Claude answers a task's end as a turn of its own, so the wait
            // is measured again from here. Left at the last idle `result`, an
            // answer that began just short of the ceiling was cut off.
            self.arm();
        }
    }

    /// Guidance was written to Claude's input.
    ///
    /// Its answer is a turn in progress, not idleness, so the ceiling is off
    /// until that turn's `result`.
    fn steered(&mut self) {
        self.steered = true;
        self.between_turns = false;
        self.deadline = None;
    }

    /// Something a turn says: a follow-up, or a steer's answer, is under way.
    fn content(&mut self) {
        self.between_turns = false;
        self.deadline = None;
    }

    /// Start the ceiling's clock, as the CLI does each time it goes idle.
    fn arm(&mut self) {
        self.deadline = self
            .ceiling
            .map(|ceiling| tokio::time::Instant::now() + ceiling);
    }

    /// What a `result` means, given whether the transport kept Claude's input
    /// open past it: it does while a task runs or an ended one's answer is
    /// still owed.
    fn result(&mut self, stop_reason: String, input_open: bool) -> AfterResult {
        self.stop_reason = Some(stop_reason);
        self.between_turns = true;
        let more = self.started_any || self.steered;
        self.waiting = !self.running.is_empty() || (more && input_open);
        if self.waiting {
            self.arm();
            return AfterResult::Wait;
        }
        self.deadline = None;
        if more {
            self.input_closed = true;
            AfterResult::Drain
        } else {
            AfterResult::End
        }
    }

    /// The stop reason to complete with when the process ends, or `None` when
    /// its ending is a failure: before any `result`, part-way through a
    /// follow-up, or with tasks running and nobody having asked it to stop.
    fn completed_at_exit(&self) -> Option<String> {
        if !self.input_closed || !self.between_turns {
            return None;
        }
        self.stop_reason.clone()
    }

    fn deadline(&self) -> Option<tokio::time::Instant> {
        self.deadline.filter(|_| self.between_turns && self.waiting)
    }
}

/// Wait for a deadline, or forever when there is none.
async fn until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// A short, readable span for a note the user reads.
fn spoken_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match seconds {
        0 => format!("{} ms", duration.as_millis()),
        1..=59 => format!("{seconds} s"),
        _ if seconds.is_multiple_of(60) => format!("{} min", seconds / 60),
        _ => format!("{} min {} s", seconds / 60, seconds % 60),
    }
}

type SteerRequest =
    std::pin::Pin<Box<dyn std::future::Future<Output = (uuid::Uuid, Result<(), String>)> + Send>>;

/// Guidance in flight for one Codex turn, and the files its images use.
///
/// The requests are polled by the loop driving the turn rather than spawned:
/// in one task, an answered steer can be taken before the reply that follows
/// it, which a separately scheduled task cannot promise.
#[derive(Default)]
struct Steering {
    requests: Vec<SteerRequest>,
    pending: std::collections::HashMap<uuid::Uuid, Steer>,
    /// Kept until the turn ends: Codex does not say when it has read a path.
    files: Vec<tempfile::TempPath>,
}

impl Steering {
    fn in_flight(&self) -> bool {
        !self.requests.is_empty()
    }

    /// The next steer Codex has answered.
    async fn next_answer(&mut self) -> (uuid::Uuid, Result<(), String>) {
        std::future::poll_fn(|cx| {
            for index in 0..self.requests.len() {
                if let std::task::Poll::Ready(answer) = self.requests[index].as_mut().poll(cx) {
                    drop(self.requests.swap_remove(index));
                    return std::task::Poll::Ready(answer);
                }
            }
            std::task::Poll::Pending
        })
        .await
    }
}

/// Write each image to a private file for the length of a turn.
///
/// Both providers take an image by path. The files are `0600` in the user's
/// temporary directory and are removed when the returned paths are dropped.
fn attachment_files(
    attachments: &[fx_core::Attachment],
) -> Result<Vec<tempfile::TempPath>, String> {
    use std::io::Write;
    attachments
        .iter()
        .map(|attachment| {
            let extension = fx_core::services::images::sniff(&attachment.data)
                .map(|format| format.extension())
                .unwrap_or("img");
            let failed = |error: std::io::Error| {
                format!("FlowX could not prepare {}: {error}", attachment.filename)
            };
            let mut file = tempfile::Builder::new()
                .prefix("flowx-attachment-")
                .suffix(&format!(".{extension}"))
                .tempfile()
                .map_err(failed)?;
            file.write_all(&attachment.data).map_err(failed)?;
            file.flush().map_err(failed)?;
            Ok(file.into_temp_path())
        })
        .collect()
}

fn file_paths(files: &[tempfile::TempPath]) -> Vec<String> {
    files
        .iter()
        .map(|file| file.to_string_lossy().into_owned())
        .collect()
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

    /// Kills a process when dropped, so a failed assertion does not leave a
    /// fixture's `sleep 60` running.
    struct KillOnDrop(libc::pid_t);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            if is_running(self.0) {
                // SAFETY: no memory effects.
                unsafe { libc::kill(self.0, libc::SIGKILL) };
            }
        }
    }

    fn is_running(pid: libc::pid_t) -> bool {
        // SAFETY: signal 0 only checks existence.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Wait for a file the fixture writes, and read it.
    fn wait_for_file(path: &std::path::Path) -> String {
        let deadline = Instant::now() + TURN_BUDGET;
        loop {
            if let Ok(text) = std::fs::read_to_string(path)
                && !text.is_empty()
            {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "{} never appeared",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn stopping_a_codex_turn_ends_the_tool_it_was_running() {
        // Stop used to kill the server alone. It was not in a group of its
        // own, so the tool it had started kept running after the turn ended.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let conversation = conversation();
        let (runner, recorder) = start(
            &conversation,
            provider,
            "spawn-and-hang",
            ApprovalPolicy::AllowAll,
        );
        let pid: libc::pid_t = wait_for_file(&server.log.with_extension("child"))
            .trim()
            .parse()
            .unwrap();
        let _reap = KillOnDrop(pid);
        wait_for_phase(&conversation, Phase::Running);
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
        let deadline = Instant::now() + Duration::from_secs(5);
        while is_running(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!is_running(pid), "the tool outlived the stopped turn");
    }

    #[test]
    fn a_stop_before_codex_answers_turn_start_keeps_the_prompt_accepted() {
        // Codex has the prompt once `turn/start` is written. A Stop before its
        // reply used to drop the reply: the prompt was offered back for sending
        // again, and the interrupt named no turn.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let conversation = conversation();
        let (runner, recorder) = start(&conversation, provider, "slow-ack", ApprovalPolicy::Ask);
        let marker = server.log.with_extension("turn-start");
        let deadline = Instant::now() + TURN_BUDGET;
        while !marker.exists() {
            assert!(Instant::now() < deadline, "turn/start never arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
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
        assert!(updates.contains(&SessionUpdate::Accepted), "{updates:?}");
        let interrupts = wait_for_file(&server.log.with_extension("interrupts"));
        let interrupt: Value = serde_json::from_str(interrupts.lines().next().unwrap()).unwrap();
        assert_eq!(interrupt["turnId"], "turn-1");
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
    fn codex_uses_the_agents_model_effort_access_and_instructions_without_workflow_overrides() {
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
        assert_eq!(params["collaborationMode"]["mode"], "default");
        assert_eq!(
            params["collaborationMode"]["settings"]["model"],
            "fixture-model"
        );
        assert_eq!(
            params["collaborationMode"]["settings"]["reasoning_effort"],
            "high"
        );
        assert!(params["collaborationMode"]["settings"]["developer_instructions"].is_null());
        assert!(params.get("additionalContext").is_none());
        assert_eq!(params["approvalPolicy"], "untrusted");
        assert_eq!(params["sandboxPolicy"]["type"], "workspaceWrite");
        assert_eq!(
            server.thread_calls()[0].1["developerInstructions"],
            "fixture instructions"
        );
    }

    #[test]
    fn a_resumed_codex_thread_leaves_its_previously_saved_planning_workflow() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let conversation = conversation();
        lock(&conversation).set_session_id("thread-previous-plan");
        let mut agent = Agent::new("Resumed");
        agent.configuration.agent_access = Some(AgentAccess::Supervised);
        let recorder = Recorder::default();
        let sink = recorder.clone();
        let _runner = TurnRunner::launch_with(
            conversation.clone(),
            "/tmp/project".into(),
            agent,
            "echo-workflow".into(),
            ApprovalPolicy::Ask,
            provider,
            move |update| sink.record(update),
        );
        let updates = recorder.wait_for_end(TURN_BUDGET);
        assert!(
            matches!(updates.last(), Some(SessionUpdate::Finished { outcome }) if outcome.is_completed())
        );
        assert_eq!(rows(&conversation), vec!["text: default"]);
        let calls = server.thread_calls();
        assert_eq!(calls[0].0, "thread/resume");
        assert_eq!(calls[0].1["threadId"], "thread-previous-plan");
        assert_eq!(calls[0].1["approvalPolicy"], "untrusted");
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
import json, sys, time
from pathlib import Path
if sys.argv[1:] == ["agents", "--json"]:
    marker = Path(__file__).with_name("active-session")
    active = marker.read_text() if marker.exists() else None
    print(json.dumps([{"sessionId":active,"status":"idle"}] if active else []))
    sys.exit(0)

import subprocess, threading
SESSION="11111111-2222-4333-8444-555555555555"
# Background tasks write from their own threads, so a line is never split.
LOCK=threading.Lock()
workers=[]
def send(value):
    with LOCK: print(json.dumps(value), flush=True)
def record(value):
    with open("received.jsonl", "a") as log: log.write(json.dumps(value) + "\n")
def finish(text, origin=None):
    send({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":text}}})
    result={"type":"result","subtype":"end_turn","session_id":SESSION,"usage":{"input_tokens":5,"output_tokens":2}}
    if origin: result.update({"origin":{"kind":origin},"num_turns":1})
    send(result)
def init():
    send({"type":"system","subtype":"init","session_id":SESSION})
def turn():
    # A turn that asks the model something opens with a request, which is
    # where Claude reads what was queued for it, steers included. A zero-turn
    # acknowledgement makes none.
    init()
    send({"type":"system","subtype":"status","status":"requesting","session_id":SESSION})
# The origin of the turn a permission's answer completes, when it is a
# follow-up's.
ANSWER_ORIGIN=[]
def task_ended():
    send({"type":"system","subtype":"task_progress","task_id":"task-1","tool_use_id":"tool-bg","description":"review the transport","subagent_type":"general-purpose","usage":{"total_tokens":900,"tool_uses":3,"duration_ms":40},"last_tool_name":"Grep"})
    send({"type":"system","subtype":"task_notification","task_id":"task-1","tool_use_id":"tool-bg","status":"completed","output_file":"/tmp/task-1","summary":"Agent \"review the transport\" completed","usage":{"total_tokens":1200,"tool_uses":4,"duration_ms":60}})
def follow_up():
    # Claude answers a task's notification as a turn of its own.
    turn()
    finish("finished", "task-notification")
def background(prompt):
    # The shape Claude Code 2.1.283 writes for an `Agent` call with
    # `run_in_background`, as captured from a real run.
    turn()
    send({"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool-bg","name":"Agent","input":{"description":"review the transport","subagent_type":"general-purpose","run_in_background":True}}]}})
    send({"type":"system","subtype":"task_started","task_id":"task-1","tool_use_id":"tool-bg","description":"review the transport","subagent_type":"general-purpose","is_backgrounded":True,"task_type":"local_agent"})
    send({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-bg","content":"Async agent launched successfully."}]}})
    if prompt=="background-spawn":
        # In a session of its own, as the real CLI starts its tools.
        child=subprocess.Popen(["sleep","60"],start_new_session=True)
        Path("background.pid").write_text(str(child.pid))
    if prompt=="background-quick":
        # The task ends before the turn's own `result`, and its follow-up
        # still comes after it: what a real run did.
        task_ended()
        finish("started")
        def later():
            time.sleep(0.2)
            follow_up()
        worker=threading.Thread(target=later)
    elif prompt=="background-permission":
        # As above, and the follow-up needs a permission, whose answer has to
        # reach Claude through its input.
        task_ended()
        finish("started")
        def later():
            time.sleep(0.2)
            turn()
            ANSWER_ORIGIN.append("task-notification")
            send({"type":"control_request","request_id":"control-f","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-f","input":{"command":"cargo test"}}})
        worker=threading.Thread(target=later)
    else:
        def later():
            while not Path("release-background").exists(): time.sleep(0.005)
            task_ended()
            follow_up()
        worker=threading.Thread(target=later, daemon=True)
        finish("started")
    worker.start()
    workers.append(worker)
record({"args":sys.argv[1:]})
for line in sys.stdin:
    message=json.loads(line)
    record(message)
    if message.get("type")=="user":
        prompt=message["message"]["content"][0]["text"]
        if prompt=="think":
            # Thinking, with no init and nothing shown: proof of reading alone.
            send({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"considering"}}})
            Path(__file__).with_name("thinking").write_text("1")
            continue
        if prompt=="resumed":
            # What a resumed session whose background tasks were killed writes
            # before it reads the prompt: a `stopped` notification per lost
            # task, then an `init` and a zero-turn `result` per batch.
            for lost in ("lost-1","lost-2","lost-3"):
                send({"type":"system","subtype":"task_notification","task_id":lost,"status":"stopped","session_id":SESSION})
            for _ in range(2):
                init()
                send({"type":"result","subtype":"success","is_error":False,"num_turns":0,"duration_api_ms":0,"result":"","stop_reason":None,"origin":{"kind":"task-notification"},"usage":{"input_tokens":0,"output_tokens":0},"session_id":SESSION})
            Path("preamble-sent").write_text("1")
            while not Path("release-resumed").exists(): time.sleep(0.005)
            init()
            send({"type":"system","subtype":"status","status":"requesting","session_id":SESSION})
            finish("the real reply")
            continue
        if prompt.startswith("background"):
            background(prompt)
            continue
        turn()
        if prompt=="hang": continue
        if prompt=="late-steer":
            # The race: a steer written just before a request goes out is not
            # in that request. Claude Code 2.1.283 names the message it takes
            # into a turn, so the steer is read where it says, not at the
            # first request after it was written.
            send({"type":"command_lifecycle","state":"queued","command_uuid":message.get("uuid")})
            send({"type":"command_lifecycle","state":"started","command_uuid":message.get("uuid")})
            send({"type":"assistant","message":{"content":[{"type":"text","text":"Checking."}]}})
            send({"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool-1","name":"Read","input":{"file_path":"a.toml"}}]}})
            steer=json.loads(sys.stdin.readline())
            record(steer)
            send({"type":"command_lifecycle","state":"queued","command_uuid":steer.get("uuid")})
            send({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","content":"first"}]}})
            send({"type":"system","subtype":"status","status":"requesting","session_id":SESSION})
            send({"type":"assistant","message":{"content":[{"type":"text","text":"Reading the second file."}]}})
            send({"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool-2","name":"Read","input":{"file_path":"b.toml"}}]}})
            send({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-2","content":"second"}]}})
            send({"type":"command_lifecycle","state":"started","command_uuid":steer.get("uuid")})
            send({"type":"system","subtype":"status","status":"requesting","session_id":SESSION})
            finish("Using staging.")
            continue
        if prompt=="mid-sentence":
            # Guidance written while Claude is halfway through a sentence is
            # read at its next request, after the sentence and the call it
            # leads to, which is where its history records it.
            send({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Let me check th"}}})
            record(json.loads(sys.stdin.readline()))
            send({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"e config file."}}})
            send({"type":"assistant","message":{"content":[{"type":"text","text":"Let me check the config file."}]}})
            send({"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool-c","name":"Read","input":{"file_path":"config.toml"}}]}})
            send({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-c","content":"env = production"}]}})
            send({"type":"system","subtype":"status","status":"requesting","session_id":SESSION})
            finish("Switching to staging.")
            continue
        if prompt=="read":
            send({"type":"control_request","request_id":"control-1","request":{"subtype":"can_use_tool","tool_name":"Read","tool_use_id":"tool-1","input":{"file_path":"Cargo.toml"}}})
            continue
        if prompt=="closed-input":
            # Stops reading its input after the first request, so the answer
            # to the second one cannot be delivered.
            send({"type":"control_request","request_id":"control-a","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-a","input":{"command":"cargo test"}}})
            import os
            os.close(0)
            time.sleep(0.5)
            send({"type":"control_request","request_id":"control-b","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-b","input":{"command":"cargo build"}}})
            while True: time.sleep(1)
        if prompt in ("quiet", "quiet-tool", "quiet-exit"):
            if prompt=="quiet-tool":
                send({"type":"assistant","message":{"content":[{"type":"tool_use","id":"long-tool","name":"Bash","input":{"command":"long-running build"}}]}})
            while not Path("release-quiet-turn").exists(): time.sleep(0.005)
            if prompt=="quiet-exit":
                print("fixture process failed", file=sys.stderr, flush=True)
                sys.exit(7)
            if prompt=="quiet-tool":
                send({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"long-tool","content":"Build finished"}]}})
            finish("finished after quiet work")
            continue
        if prompt in ("tool", "questions"):
            tool="Bash" if prompt=="tool" else "AskUserQuestion"
            data={"command":"cargo test"} if prompt=="tool" else {"questions":[
                {"header":"Database","question":"Which database?","options":[{"label":"SQLite"}]},
                {"header":"Region","question":"Which region?","options":[]}]}
            send({"type":"control_request","request_id":"control-1","request":{"subtype":"can_use_tool","tool_name":tool,"tool_use_id":"tool-1","input":data}})
        else: finish(prompt)
    elif message.get("type")=="control_response":
        finish("answered", ANSWER_ORIGIN.pop() if ANSWER_ORIGIN else None)
# A closed input still waits for running tasks, as Claude's print mode does.
for worker in workers: worker.join()
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
            self.start_with(conversation, prompt, access, model, ApprovalPolicy::Ask)
        }

        fn start_with(
            &self,
            conversation: &Arc<Mutex<Conversation>>,
            prompt: &str,
            access: AgentAccess,
            model: &str,
            approvals: ApprovalPolicy,
        ) -> (TurnRunner, Recorder) {
            self.start_in(
                conversation,
                prompt,
                access,
                model,
                approvals,
                RuntimeDiscovery::default().without_extra_directories(),
            )
        }

        /// A turn whose CLI is launched with one more environment variable.
        fn start_with_variable(
            &self,
            conversation: &Arc<Mutex<Conversation>>,
            prompt: &str,
            (key, value): (&str, &str),
        ) -> (TurnRunner, Recorder) {
            let mut environment: std::collections::BTreeMap<String, String> =
                std::env::vars().collect();
            environment.insert(key.into(), value.into());
            self.start_in(
                conversation,
                prompt,
                AgentAccess::FullAccess,
                "fixture-model",
                ApprovalPolicy::Ask,
                RuntimeDiscovery::default()
                    .without_extra_directories()
                    .with_environment(environment),
            )
        }

        fn start_in(
            &self,
            conversation: &Arc<Mutex<Conversation>>,
            prompt: &str,
            access: AgentAccess,
            model: &str,
            approvals: ApprovalPolicy,
            discovery: RuntimeDiscovery,
        ) -> (TurnRunner, Recorder) {
            let mut agent = Agent::new("Claude");
            agent.configuration.provider_id = Some("claude".into());
            agent.configuration.model_id = Some(model.into());
            agent.configuration.effort = Some("high".into());
            agent.configuration.system_prompt = Some("fixture instructions".into());
            agent.configuration.agent_access = Some(access);
            let provider = RuntimeProvider::Claude {
                discovery,
                spec: BinarySpec::new("claude", "Claude Code", vec![FakeServer::python().unwrap()])
                    .with_arguments(vec![self.script.display().to_string()]),
            };
            let recorder = Recorder::default();
            let sink = recorder.clone();
            let runner = TurnRunner::launch(
                conversation.clone(),
                self.dir.path().display().to_string(),
                agent,
                Work::prompt(prompt),
                approvals,
                provider,
                move |update| sink.record(update),
            );
            (runner, recorder)
        }

        /// A job that runs in the caller's own runtime, for a test that has
        /// to see what the turn does to the thread it runs on. The caller
        /// hands `run` its own channels; the ones held here only keep the
        /// job's shape.
        fn job(
            &self,
            conversation: &Arc<Mutex<Conversation>>,
            prompt: &str,
            on_update: impl Fn(SessionUpdate) + Send + 'static,
        ) -> Job {
            let (interrupt, _) = tokio::sync::mpsc::unbounded_channel();
            let (responses, _) = tokio::sync::mpsc::unbounded_channel();
            Job {
                conversation: conversation.clone(),
                provider: RuntimeProvider::Claude {
                    discovery: RuntimeDiscovery::default().without_extra_directories(),
                    spec: BinarySpec::new(
                        "claude",
                        "Claude Code",
                        vec![FakeServer::python().unwrap()],
                    )
                    .with_arguments(vec![self.script.display().to_string()]),
                },
                project_root: self.dir.path().display().to_string(),
                prompt: prompt.into(),
                attachments: Vec::new(),
                control: None,
                steers: Mutex::new(None),
                approvals: ApprovalPolicy::Ask,
                access: AgentAccess::Supervised,
                model: "fixture-model".into(),
                effort: None,
                system_prompt: None,
                on_update: Arc::new(Mutex::new(Box::new(on_update))),
                interrupt,
                responses,
            }
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
        assert_eq!(
            lock(&conversation).session_id(),
            Some("11111111-2222-4333-8444-555555555555")
        );
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
        assert!(!args.iter().any(|arg| arg == "plan"), "{args:?}");
        assert!(
            launches[1]
                .as_array()
                .unwrap()
                .windows(2)
                .any(|window| window[0] == "--resume"
                    && window[1] == "11111111-2222-4333-8444-555555555555")
        );
    }

    #[test]
    fn claude_refuses_an_external_writer_before_sending_the_prompt() {
        let Some(server) = ClaudeFixture::new() else {
            return;
        };
        std::fs::write(
            server.dir.path().join("active-session"),
            "99999999-2222-4333-8444-555555555555",
        )
        .unwrap();
        let conversation = conversation();
        lock(&conversation).set_session_id("99999999-2222-4333-8444-555555555555");
        let (_runner, recorder) =
            server.start(&conversation, "keep this draft", AgentAccess::Supervised);
        let updates = recorder.wait_for_end(TURN_BUDGET);
        assert!(updates.iter().any(|update| matches!(update, SessionUpdate::Failed { reason } if reason.contains("still open"))), "{updates:?}");
        assert!(!updates.contains(&SessionUpdate::Accepted));
        assert!(
            !server.log.exists(),
            "the resumed provider must never receive a prompt"
        );
        assert_eq!(
            lock(&conversation).session_id(),
            Some("99999999-2222-4333-8444-555555555555")
        );
    }

    #[test]
    fn claude_unknown_default_capabilities_preserve_the_explicit_saved_effort() {
        let Some(server) = ClaudeFixture::new() else {
            return;
        };
        let conversation = conversation();
        let (_runner, recorder) =
            server.start_with_model(&conversation, "hello", AgentAccess::Supervised, "default");
        let updates = recorder.wait_for_end(TURN_BUDGET);
        assert!(
            matches!(updates.last(), Some(SessionUpdate::Finished { outcome }) if outcome.is_completed()),
            "{updates:?}"
        );
        let logs = server.received();
        let args = logs[0]["args"].as_array().unwrap();
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--effort" && pair[1] == "high"),
            "{args:?}"
        );
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
    fn claude_explicit_full_access_approves_tools_without_workflow_overrides() {
        let Some(server) = ClaudeFixture::new() else {
            return;
        };
        let conversation = conversation();
        let (_runner, recorder) = server.start(&conversation, "tool", AgentAccess::FullAccess);
        let updates = recorder.wait_for_end(TURN_BUDGET);
        assert!(
            matches!(updates.last(), Some(SessionUpdate::Finished { outcome }) if outcome.is_completed())
        );
        assert!(
            !updates
                .iter()
                .any(|update| matches!(update, SessionUpdate::Interaction { .. }))
        );
        let response = server
            .received()
            .into_iter()
            .find(|item| item["type"] == "control_response")
            .unwrap();
        assert_eq!(response["response"]["response"]["behavior"], "allow");
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
        // Stop asks Claude itself first, before its process group is signalled.
        assert!(
            server.received().iter().any(|message| {
                message["type"] == "control_request" && message["request"]["subtype"] == "interrupt"
            }),
            "Claude was never sent an interrupt"
        );
    }

    #[test]
    fn claude_read_only_policy_allows_reads_by_claudes_tool_names() {
        // The policy knew only Codex's names, so `Read` was refused like any
        // unknown tool.
        for (prompt, behavior) in [("read", "allow"), ("tool", "deny")] {
            let Some(server) = ClaudeFixture::new() else {
                return;
            };
            let conversation = conversation();
            let (_runner, recorder) = server.start_with(
                &conversation,
                prompt,
                AgentAccess::Supervised,
                "fixture-model",
                ApprovalPolicy::DenyUnlessReadOnly,
            );
            let updates = recorder.wait_for_end(TURN_BUDGET);
            assert!(
                matches!(updates.last(), Some(SessionUpdate::Finished { outcome }) if outcome.is_completed()),
                "{prompt}: {updates:?}"
            );
            let response = server
                .received()
                .into_iter()
                .find(|item| item["type"] == "control_response")
                .unwrap();
            assert_eq!(
                response["response"]["response"]["behavior"], behavior,
                "{prompt}"
            );
        }
    }

    #[test]
    fn claude_fails_the_turn_when_an_answer_cannot_be_delivered() {
        // An answer that failed to write left its request open: the panel's
        // buttons stayed disabled and nothing would ever clear them.
        let Some(server) = ClaudeFixture::new() else {
            return;
        };
        let conversation = conversation();
        let (runner, recorder) =
            server.start(&conversation, "closed-input", AgentAccess::Supervised);
        let ProviderEvent::Approval(first) = wait_for_interaction(&recorder) else {
            panic!("approval expected")
        };
        assert!(runner.respond(UserResponse::Approval {
            id: first.id.clone(),
            approved: true,
        }));
        let deadline = Instant::now() + TURN_BUDGET;
        let second = loop {
            let found = recorder
                .updates()
                .into_iter()
                .find_map(|update| match update {
                    SessionUpdate::Interaction {
                        event: ProviderEvent::Approval(request),
                    } if request.id != first.id => Some(request),
                    _ => None,
                });
            if let Some(request) = found {
                break request;
            }
            assert!(Instant::now() < deadline, "{:?}", recorder.updates());
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(runner.respond(UserResponse::Approval {
            id: second.id.clone(),
            approved: true,
        }));

        let updates = recorder.wait_for_end(TURN_BUDGET);
        assert!(
            matches!(updates.last(), Some(SessionUpdate::Failed { reason }) if reason.starts_with(ANSWER_FAILED)),
            "{updates:?}"
        );
        assert!(updates.contains(&SessionUpdate::InteractionResolved { id: second.id }));
        assert!(lock(&conversation).pending_approvals().is_empty());
    }

    #[test]
    fn claude_stopped_while_thinking_does_not_hand_the_prompt_back() {
        // Thinking produces no event, so a Stop during it used to restore a
        // prompt Claude had already read, and sending it again duplicated it.
        let Some(server) = ClaudeFixture::new() else {
            return;
        };
        let conversation = conversation();
        let (runner, recorder) = server.start(&conversation, "think", AgentAccess::Supervised);
        let marker = server.dir.path().join("thinking");
        let deadline = Instant::now() + TURN_BUDGET;
        while !marker.exists() {
            assert!(Instant::now() < deadline, "the fixture never thought");
            std::thread::sleep(Duration::from_millis(5));
        }
        // The frame is on the pipe; give the reader a moment to take it.
        std::thread::sleep(Duration::from_millis(200));
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
        assert!(updates.contains(&SessionUpdate::Accepted), "{updates:?}");
    }

    #[test]
    fn claude_does_not_spin_once_the_response_channel_closes() {
        // A closed channel is ready forever. Selecting on it again and again
        // burned a whole CPU for the rest of the turn.
        let Some(server) = ClaudeFixture::new() else {
            return;
        };
        fn thread_cpu() -> Duration {
            let mut now = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: writes only into `now`.
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) };
            Duration::new(now.tv_sec as u64, now.tv_nsec as u32)
        }
        let conversation = conversation();
        let (updates, mut received) = tokio::sync::mpsc::unbounded_channel();
        let job = server.job(&conversation, "hang", move |update| {
            let _ = updates.send(update);
        });
        // Everything runs on this one thread, so its CPU time is the turn's.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let used = runtime.block_on(async {
            let (interrupt, interrupts) = tokio::sync::mpsc::unbounded_channel();
            let (closed, responses) = tokio::sync::mpsc::unbounded_channel();
            drop(closed);
            let task = tokio::spawn(job.run(interrupts, responses));
            tokio::time::timeout(TURN_BUDGET, async {
                while !matches!(
                    received.recv().await,
                    Some(SessionUpdate::Started { .. }) | None
                ) {}
            })
            .await
            .expect("the turn started");
            let before = thread_cpu();
            tokio::time::sleep(Duration::from_millis(300)).await;
            let used = thread_cpu() - before;
            interrupt.send(()).unwrap();
            tokio::time::timeout(TURN_BUDGET, task)
                .await
                .expect("the turn stopped")
                .unwrap();
            used
        });
        assert!(
            used < Duration::from_millis(150),
            "an idle turn used {used:?} of CPU in 300 ms"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn claude_quiet_work_survives_two_minutes_and_still_completes_cancels_or_reports_exit() {
        // A real child and pipes, with only Tokio's clock accelerated. The gate
        // keeps the child quiet until the runner has crossed its old cutoff.
        for (prompt, cancel) in [
            ("quiet", false),
            ("quiet-tool", false),
            ("quiet", true),
            ("quiet-exit", false),
        ] {
            let Some(server) = ClaudeFixture::new() else {
                return;
            };
            let conversation = conversation();
            lock(&conversation).set_phase(Phase::Starting);
            let (interrupt, interrupts) = tokio::sync::mpsc::unbounded_channel();
            let (response, responses) = tokio::sync::mpsc::unbounded_channel();
            let (updates, mut received) = tokio::sync::mpsc::unbounded_channel();
            let job = Job {
                conversation: conversation.clone(),
                provider: RuntimeProvider::Claude {
                    discovery: RuntimeDiscovery::default().without_extra_directories(),
                    spec: BinarySpec::new(
                        "claude",
                        "Claude Code",
                        vec![FakeServer::python().unwrap()],
                    )
                    .with_arguments(vec![server.script.display().to_string()]),
                },
                project_root: server.dir.path().display().to_string(),
                prompt: prompt.into(),
                attachments: Vec::new(),
                control: None,
                steers: Mutex::new(None),
                approvals: ApprovalPolicy::Ask,
                access: AgentAccess::FullAccess,
                model: "fixture-model".into(),
                effort: None,
                system_prompt: None,
                on_update: Arc::new(Mutex::new(Box::new(move |update| {
                    let _ = updates.send(update);
                }))),
                interrupt: interrupt.clone(),
                responses: response,
            };
            let task = tokio::spawn(job.run(interrupts, responses));
            tokio::time::timeout(TURN_BUDGET, async {
                while let Some(update) = received.recv().await {
                    match update {
                        SessionUpdate::Started { .. } if prompt != "quiet-tool" => break,
                        SessionUpdate::Changed
                            if prompt == "quiet-tool"
                                && lock(&conversation).message_count() > 0 =>
                        {
                            break;
                        }
                        SessionUpdate::Failed { reason } => {
                            panic!("fixture failed to start: {reason}")
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("the child reaches its quiet work");

            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(121)).await;
            // Poll the runner before releasing the child, so an elapsed timeout
            // cannot race a result and occasionally escape the regression test.
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            tokio::time::advance(Duration::from_secs(3)).await;
            tokio::time::resume();

            if cancel {
                interrupt.send(()).expect("Stop remains connected");
            } else {
                std::fs::write(server.dir.path().join("release-quiet-turn"), "continue").unwrap();
            }
            tokio::time::timeout(TURN_BUDGET, task)
                .await
                .expect("quiet work can settle")
                .unwrap();
            let mut ending = None;
            while let Ok(update) = received.try_recv() {
                if matches!(
                    update,
                    SessionUpdate::Finished { .. } | SessionUpdate::Failed { .. }
                ) {
                    ending = Some(update);
                }
            }
            match (prompt, cancel, &ending) {
                (
                    _,
                    true,
                    Some(SessionUpdate::Finished {
                        outcome: TurnOutcome::Cancelled { .. },
                    }),
                ) => {}
                ("quiet-exit", false, Some(SessionUpdate::Failed { reason })) => {
                    assert!(reason.contains("fixture process failed"), "{reason}")
                }
                (_, false, Some(SessionUpdate::Finished { outcome })) if outcome.is_completed() => {
                    assert_eq!(
                        rows(&conversation).last().map(String::as_str),
                        Some("text: finished after quiet work")
                    )
                }
                _ => panic!("{prompt}, cancel={cancel}: {ending:?}"),
            }
        }
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

def wait_for_steer():
    # Answers the next `turn/steer`, as codex-cli 0.164 does, and returns its
    # params. The steer is recorded later, when Codex reads it.
    while True:
        waiting = sys.stdin.readline()
        if not waiting:
            sys.exit(0)
        if not waiting.strip():
            continue
        request = json.loads(waiting)
        if request.get("method") == "turn/steer":
            params = request.get("params") or {}
            with open("THREAD_LOG", "a") as log:
                log.write(json.dumps({"method": "turn/steer", "params": params}) + "\n")
            send({"jsonrpc": "2.0", "id": request.get("id"),
                  "result": {"turnId": params.get("expectedTurnId")}})
            return params

def record_steer(params):
    # Codex's record of a steer it has read, under the id it was sent with.
    item = {"type": "userMessage", "id": "user-2",
            "clientId": params.get("clientUserMessageId"),
            "content": params.get("input") or []}
    send({"jsonrpc": "2.0", "method": "item/started", "params": {"item": item}})
    send({"jsonrpc": "2.0", "method": "item/completed", "params": {"item": item}})

workflow = "default"
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
        # Codex restores this from its saved thread context on resume.
        workflow = "plan" if requested == "thread-previous-plan" else "default"
        send({"jsonrpc": "2.0", "id": mid, "result": {"thread": {
            "id": requested or "thread-new"}}})
    elif method == "turn/start":
        workflow = ((msg.get("params") or {}).get("collaborationMode") or {}).get("mode", workflow)
        prompt = ""
        for item in (msg.get("params") or {}).get("input") or []:
            if item.get("type") == "text":
                prompt = item.get("text") or ""
        if prompt == "slow-ack":
            # Codex has the prompt; its reply is still on the way when Stop lands.
            open("THREAD_LOG" + ".turn-start", "w").close()
            time.sleep(0.5)
        send({"jsonrpc": "2.0", "id": mid, "result": {"turn": {"id": "turn-1"}}})

        if prompt == "spawn-and-hang":
            # A tool that is still running when the user presses Stop, in a
            # process group of its own as the real app-server starts one.
            import subprocess
            child = subprocess.Popen(["sleep", "60"], preexec_fn=os.setpgrp)
            with open("THREAD_LOG" + ".child", "w") as pid:
                pid.write(str(child.pid))
            send({"jsonrpc": "2.0", "method": "turn/started",
                  "params": {"turn": {"id": "turn-1"}}})
        elif prompt == "slow-ack":
            send({"jsonrpc": "2.0", "method": "turn/started",
                  "params": {"turn": {"id": "turn-1"}}})
        elif prompt == "fail-now":
            send({"jsonrpc": "2.0", "method": "turn/completed",
                  "params": {"turn": {"id": "turn-1", "status": "failed",
                                      "error": {"message": "the model refused"}}}})
        elif prompt in ("echo-params", "echo-workflow"):
            send({"jsonrpc": "2.0", "method": "turn/started", "params": {"turn": {"id": "turn-1"}}})
            echoed = json.dumps(msg.get("params")) if prompt == "echo-params" else workflow
            send({"jsonrpc": "2.0", "method": "item/agentMessage/delta", "params": {"delta": echoed}})
            send({"jsonrpc": "2.0", "method": "turn/completed", "params": {"turn": {"id": "turn-1", "status": "completed"}}})
        elif prompt == "echo-images":
            # Reads each image the way Codex does, from its path, and reports
            # what it found there.
            seen = []
            for item in (msg.get("params") or {}).get("input") or []:
                if item.get("type") == "localImage":
                    with open(item.get("path"), "rb") as image:
                        seen.append([len(image.read()), item.get("path")])
            send({"jsonrpc": "2.0", "method": "turn/started", "params": {"turn": {"id": "turn-1"}}})
            send({"jsonrpc": "2.0", "method": "item/agentMessage/delta", "params": {"delta": json.dumps(seen)}})
            send({"jsonrpc": "2.0", "method": "turn/completed", "params": {"turn": {"id": "turn-1", "status": "completed"}}})
        elif prompt == "steer-mid-sentence":
            # What codex-cli 0.164 did with a steer written mid-sentence: the
            # sentence and the command it led to finish, and only then is the
            # steer recorded, at the next request.
            send({"jsonrpc": "2.0", "method": "turn/started", "params": {"turn": {"id": "turn-1"}}})
            send({"jsonrpc": "2.0", "method": "item/started", "params": {"item": {"type": "agentMessage", "id": "msg-1", "text": ""}}})
            send({"jsonrpc": "2.0", "method": "item/agentMessage/delta", "params": {"itemId": "msg-1", "delta": "Let me check th"}})
            params = wait_for_steer()
            send({"jsonrpc": "2.0", "method": "item/agentMessage/delta", "params": {"itemId": "msg-1", "delta": "e config file."}})
            send({"jsonrpc": "2.0", "method": "item/completed", "params": {"item": {"type": "agentMessage", "id": "msg-1", "text": "Let me check the config file."}}})
            command = {"type": "commandExecution", "id": "cmd-1", "command": "cat config.toml"}
            send({"jsonrpc": "2.0", "method": "item/started", "params": {"item": command}})
            send({"jsonrpc": "2.0", "method": "item/completed", "params": {"item": dict(command, aggregatedOutput="env = production", exitCode=0, status="completed")}})
            record_steer(params)
            send({"jsonrpc": "2.0", "method": "item/agentMessage/delta", "params": {"itemId": "msg-2", "delta": "Switching to staging."}})
            send({"jsonrpc": "2.0", "method": "turn/completed", "params": {"turn": {"id": "turn-1", "status": "completed"}}})
        elif prompt == "wait-for-steer":
            # Starts, then waits for guidance and answers it within the turn.
            send({"jsonrpc": "2.0", "method": "turn/started", "params": {"turn": {"id": "turn-1"}}})
            while True:
                waiting = sys.stdin.readline()
                if not waiting:
                    sys.exit(0)
                if not waiting.strip():
                    continue
                request = json.loads(waiting)
                if request.get("method") == "turn/steer":
                    params = request.get("params") or {}
                    with open("THREAD_LOG", "a") as log:
                        log.write(json.dumps({"method": "turn/steer", "params": params}) + "\n")
                    send({"jsonrpc": "2.0", "id": request.get("id"),
                          "result": {"turnId": params.get("expectedTurnId")}})
                    record_steer(params)
                    guidance = ""
                    for item in params.get("input") or []:
                        if item.get("type") == "text":
                            guidance = item.get("text") or ""
                    send({"jsonrpc": "2.0", "method": "item/agentMessage/delta",
                          "params": {"delta": "Steered: " + guidance}})
                    send({"jsonrpc": "2.0", "method": "turn/completed",
                          "params": {"turn": {"id": "turn-1", "status": "completed"}}})
                    break
                if request.get("method") == "turn/interrupt":
                    send({"jsonrpc": "2.0", "id": request.get("id"), "result": {}})
                    break
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
        with open("THREAD_LOG" + ".interrupts", "a") as log:
            log.write(json.dumps(msg.get("params")) + "\n")
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
    elif method == "thread/compact/start":
        params = msg.get("params") or {}
        with open("THREAD_LOG", "a") as log:
            log.write(json.dumps({"method": method, "params": params}) + "\n")
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
        thread = params.get("threadId")
        send({"jsonrpc": "2.0", "method": "item/started", "params": {
            "threadId": thread, "item": {"id": "compact-1", "type": "contextCompaction"}}})
        send({"jsonrpc": "2.0", "method": "thread/compacted", "params": {"threadId": thread}})
    elif method in ("thread/goal/set", "thread/goal/get", "thread/goal/clear"):
        params = msg.get("params") or {}
        with open("THREAD_LOG", "a") as log:
            log.write(json.dumps({"method": method, "params": params}) + "\n")
        if method == "thread/goal/clear":
            send({"jsonrpc": "2.0", "id": mid, "result": {}})
        elif method == "thread/goal/get":
            send({"jsonrpc": "2.0", "id": mid, "result": {"goal": None}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {"goal": {
                "threadId": params.get("threadId"),
                "objective": params.get("objective", "kept objective"),
                "status": params.get("status", "active"),
                "tokensUsed": 1200, "tokenBudget": 5000}}})
    else:
        send({"jsonrpc": "2.0", "id": mid,
              "error": {"code": -32601, "message": "unknown method"}})
"#;

    // ── Images, steering and thread controls ─────────────────────────────────

    fn start_work(
        conversation: &Arc<Mutex<Conversation>>,
        provider: CodexProvider,
        work: Work,
    ) -> (TurnRunner, Recorder) {
        let recorder = Recorder::default();
        let sink = recorder.clone();
        let runner = TurnRunner::launch_work_with(
            conversation.clone(),
            "/tmp/project".to_string(),
            Agent::new("Runner"),
            work,
            ApprovalPolicy::AllowAll,
            provider,
            move |update| sink.record(update),
        );
        (runner, recorder)
    }

    /// Bytes that sniff as a PNG, which is all the attachment path reads.
    fn png_bytes(len: usize) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.resize(len, 7);
        bytes
    }

    #[test]
    fn images_reach_codex_as_private_files_that_last_only_for_the_turn() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let conversation = conversation();
        let images = vec![
            fx_core::Attachment::new(png_bytes(40), "image/png", "a.png"),
            fx_core::Attachment::new(png_bytes(64), "image/png", "b.png"),
        ];
        let (_runner, recorder) = start_work(
            &conversation,
            provider,
            Work::prompt("echo-images").with_attachments(images),
        );
        let updates = recorder.wait_for_end(TURN_BUDGET);
        assert!(
            matches!(updates.last(), Some(SessionUpdate::Finished { outcome }) if outcome.is_completed()),
            "{updates:?}"
        );
        let seen: Vec<(usize, String)> =
            serde_json::from_str(&lock(&conversation).last_text().unwrap()).unwrap();
        assert_eq!(
            seen.iter().map(|(len, _)| *len).collect::<Vec<_>>(),
            vec![40, 64],
            "Codex did not read the images it was sent, in order"
        );
        for (_, path) in seen {
            assert!(path.ends_with(".png"), "{path}");
            assert!(
                !std::path::Path::new(&path).exists(),
                "{path} outlived the turn"
            );
        }
    }

    #[test]
    fn a_steer_claude_takes_mid_sentence_lands_where_its_history_puts_it() {
        // Claude reads guidance at its next request. Placed when it was
        // written, the row split the sentence streaming at the time, and the
        // provider's history, which has the sentence whole and the guidance
        // after the call, never matched the thread again.
        let Some(server) = ClaudeFixture::new() else {
            return;
        };
        let conversation = conversation();
        let (runner, recorder) =
            server.start(&conversation, "mid-sentence", AgentAccess::FullAccess);
        wait_until(&recorder, "the sentence to start", || {
            lock(&conversation).streaming_text() == "Let me check th"
        });
        let id = Uuid::new_v4();
        assert!(runner.steer(id, "use staging".into(), Vec::new()));
        let updates = recorder.wait_for_end(TURN_BUDGET);

        assert!(
            updates.contains(&SessionUpdate::Steered { id, result: Ok(()) }),
            "{updates:?}"
        );
        assert_eq!(
            rows(&conversation),
            [
                "text: Let me check the config file.",
                "call: Read",
                "result",
                "text: use staging",
                "text: Switching to staging."
            ],
            "the guidance must sit where Claude read it"
        );
    }

    #[test]
    fn a_steer_written_as_a_request_goes_out_lands_where_claude_took_it() {
        // Written just before a request, a steer is not in that request but
        // the one after. Placed at the first request after it was written, it
        // sat a step early, ahead of prose Claude's history has before it.
        let Some(server) = ClaudeFixture::new() else {
            return;
        };
        let conversation = conversation();
        let (runner, recorder) = server.start(&conversation, "late-steer", AgentAccess::FullAccess);
        wait_until(&recorder, "the first call", || {
            rows(&conversation).iter().any(|row| row == "call: Read")
        });
        let id = Uuid::new_v4();
        assert!(runner.steer(id, "use staging".into(), Vec::new()));
        let updates = recorder.wait_for_end(TURN_BUDGET);

        assert!(
            updates.contains(&SessionUpdate::Steered { id, result: Ok(()) }),
            "{updates:?}"
        );
        assert_eq!(
            rows(&conversation),
            [
                "text: Checking.",
                "call: Read",
                "result",
                "text: Reading the second file.",
                "call: Read",
                "result",
                "text: use staging",
                "text: Using staging."
            ],
            "the guidance must sit where Claude took it"
        );
        // Sent under the steer's own id, which is how Claude names it.
        let sent: Vec<serde_json::Value> = std::fs::read_to_string(&server.log)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        assert!(
            sent.iter().any(|message| message["uuid"] == id.to_string()),
            "{sent:?}"
        );
    }

    #[test]
    fn steering_a_running_codex_turn_lands_the_guidance_where_codex_took_it() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let conversation = conversation();
        let (runner, recorder) = start(
            &conversation,
            provider,
            "wait-for-steer",
            ApprovalPolicy::AllowAll,
        );
        wait_for_phase(&conversation, Phase::Running);
        let id = Uuid::new_v4();
        assert!(runner.steer(id, "use zsh".into(), Vec::new()));
        let updates = recorder.wait_for_end(TURN_BUDGET);

        assert!(
            updates.contains(&SessionUpdate::Steered { id, result: Ok(()) }),
            "{updates:?}"
        );
        assert_eq!(
            rows(&conversation),
            vec!["text: use zsh", "text: Steered: use zsh"],
            "the guidance must sit before the reply it produced"
        );
        let steer = server
            .thread_calls()
            .into_iter()
            .find(|(method, _)| method == "turn/steer")
            .expect("Codex was steered");
        assert_eq!(steer.1["threadId"], "thread-new");
        assert_eq!(steer.1["expectedTurnId"], "turn-1");
        // The id Codex names the steer by when it records it.
        assert_eq!(steer.1["clientUserMessageId"], id.to_string());
    }

    #[test]
    fn a_steer_codex_takes_mid_sentence_lands_where_codex_recorded_it() {
        // Codex accepted the steer at once and recorded it at its next
        // request, after the sentence and the command. Placed on acceptance,
        // the row split the sentence, and the history never matched.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let conversation = conversation();
        let (runner, recorder) = start(
            &conversation,
            provider,
            "steer-mid-sentence",
            ApprovalPolicy::AllowAll,
        );
        wait_until(&recorder, "the sentence to start", || {
            lock(&conversation).streaming_text() == "Let me check th"
        });
        let id = Uuid::new_v4();
        assert!(runner.steer(id, "use staging".into(), Vec::new()));
        let updates = recorder.wait_for_end(TURN_BUDGET);

        assert!(
            updates.contains(&SessionUpdate::Steered { id, result: Ok(()) }),
            "{updates:?}"
        );
        assert_eq!(
            rows(&conversation),
            [
                "text: Let me check the config file.",
                "call: commandExecution",
                "result",
                "text: use staging",
                "text: Switching to staging."
            ],
            "the guidance must sit where Codex recorded it"
        );
    }

    #[test]
    fn guidance_a_turn_never_takes_is_answered_rather_than_dropped() {
        // Whichever way this races — before the turn is acknowledged, while
        // it finishes, or after — the composer is holding the draft until it
        // hears back, so it must hear back.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let conversation = conversation();
        let (runner, recorder) = start(&conversation, provider, "hello", ApprovalPolicy::AllowAll);
        let id = Uuid::new_v4();
        let sent = runner.steer(id, "too late".into(), Vec::new());
        let updates = recorder.wait_for_end(TURN_BUDGET);
        if sent {
            assert!(
                updates.iter().any(|update| matches!(
                    update,
                    SessionUpdate::Steered { id: steered, result: Err(_) } if *steered == id
                )),
                "{updates:?}"
            );
        }
        assert!(!rows(&conversation).contains(&"text: too late".to_string()));
    }

    #[test]
    fn compacting_resumes_the_thread_and_waits_for_codex_to_finish() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let conversation = conversation();
        lock(&conversation).set_session_id("thread-old");
        let (_runner, recorder) = start_work(
            &conversation,
            provider,
            Work::control(ThreadControl::Compact(String::new())),
        );
        let updates = recorder.wait_for_end(TURN_BUDGET);
        assert!(
            matches!(updates.last(), Some(SessionUpdate::Finished { outcome }) if outcome.is_completed()),
            "{updates:?}"
        );
        let calls = server.thread_calls();
        assert_eq!(calls[0].0, "thread/resume");
        assert_eq!(calls[1].0, "thread/compact/start");
        assert_eq!(calls[1].1["threadId"], "thread-old");
        let conversation = lock(&conversation);
        assert!(
            conversation
                .activities()
                .any(|activity| activity.summary == "Context compacted"),
            "the compaction was not waited for"
        );
        assert_eq!(conversation.phase(), Phase::Idle);
        assert_eq!(conversation.message_count(), 0, "a control adds no rows");
    }

    #[test]
    fn compacting_a_thread_with_no_session_starts_nothing() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let conversation = conversation();
        let (_runner, recorder) = start_work(
            &conversation,
            provider,
            Work::control(ThreadControl::Compact(String::new())),
        );
        recorder.wait_for_end(TURN_BUDGET);
        assert!(server.thread_calls().is_empty(), "Codex was started anyway");
        assert!(
            lock(&conversation)
                .activities()
                .any(|activity| activity.summary.starts_with("Nothing to compact"))
        );
    }

    #[test]
    fn a_goal_set_from_the_composer_reaches_codex_and_reports_back() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(provider) = server.provider() else {
            return;
        };
        let conversation = conversation();
        lock(&conversation).set_session_id("thread-old");
        let (_runner, recorder) = start_work(
            &conversation,
            provider,
            Work::control(ThreadControl::GoalSet("Ship the release".into())),
        );
        let updates = recorder.wait_for_end(TURN_BUDGET);
        let goal = updates
            .iter()
            .find_map(|update| match update {
                SessionUpdate::GoalChanged { goal } => goal.clone(),
                _ => None,
            })
            .expect("the goal was reported");
        assert_eq!(goal.objective, "Ship the release");
        assert_eq!(goal.status, fx_core::models::GoalStatus::Active);
        assert_eq!(goal.usage_text().as_deref(), Some("1.2k/5.0k"));
        let set = server
            .thread_calls()
            .into_iter()
            .find(|(method, _)| method == "thread/goal/set")
            .unwrap();
        assert_eq!(
            set.1,
            serde_json::json!({
                "threadId": "thread-old",
                "objective": "Ship the release",
                "status": "active",
            })
        );
    }

    #[test]
    fn claude_takes_guidance_on_its_live_stream_json_input() {
        let Some(server) = ClaudeFixture::new() else {
            return;
        };
        let conversation = conversation();
        let (runner, recorder) = server.start(&conversation, "hang", AgentAccess::FullAccess);
        let deadline = Instant::now() + TURN_BUDGET;
        while !recorder
            .updates()
            .iter()
            .any(|update| matches!(update, SessionUpdate::Started { .. }))
        {
            assert!(Instant::now() < deadline, "{:?}", recorder.updates());
            std::thread::sleep(Duration::from_millis(5));
        }
        let id = Uuid::new_v4();
        assert!(runner.steer(id, "use zsh".into(), Vec::new()));
        let updates = recorder.wait_for_end(TURN_BUDGET);
        assert!(
            updates.contains(&SessionUpdate::Steered { id, result: Ok(()) }),
            "{updates:?}"
        );
        assert_eq!(rows(&conversation), vec!["text: use zsh", "text: use zsh"]);
        let received = server.received();
        let users: Vec<&Value> = received
            .iter()
            .filter(|message| message["type"] == "user")
            .collect();
        assert_eq!(users.len(), 2, "{received:?}");
        assert_eq!(users[1]["message"]["content"][0]["text"], "use zsh");
    }

    /// Poll until `ready`, failing with the updates so far if it never is.
    fn wait_until(recorder: &Recorder, what: &str, ready: impl Fn() -> bool) {
        let deadline = Instant::now() + TURN_BUDGET;
        while !ready() {
            assert!(
                Instant::now() < deadline,
                "{what} never happened: {:?}",
                recorder.updates()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn ended(recorder: &Recorder) -> bool {
        recorder.updates().iter().any(|update| {
            matches!(
                update,
                SessionUpdate::Finished { .. } | SessionUpdate::Failed { .. }
            )
        })
    }

    /// Wait until the first `result` has landed with the task still running.
    fn wait_for_the_background_wait(conversation: &Mutex<Conversation>, recorder: &Recorder) {
        wait_until(recorder, "the first result with a task running", || {
            let conversation = lock(conversation);
            conversation.has_running_tasks()
                && conversation.last_text().as_deref() == Some("started")
        });
        // Long enough for a runner that wrongly ended at that `result` to say so.
        std::thread::sleep(Duration::from_millis(300));
    }

    #[test]
    fn claude_keeps_the_turn_through_a_background_task_and_lands_its_follow_up() {
        // The turn used to end at the first `result` and stop Claude, which
        // killed the six reviews it had started in the background. Their
        // results never came back and the thread read as ready.
        let Some(server) = ClaudeFixture::new() else {
            return;
        };
        let conversation = conversation();
        let (runner, recorder) = server.start(&conversation, "background", AgentAccess::FullAccess);
        wait_for_the_background_wait(&conversation, &recorder);
        assert!(!ended(&recorder), "the turn ended with a task running");
        assert_eq!(lock(&conversation).phase(), Phase::Running);

        // Waiting is not over, so guidance still goes to the live input.
        let id = Uuid::new_v4();
        assert!(runner.steer(id, "use zsh".into(), Vec::new()));
        wait_until(&recorder, "the steer's answer", || {
            recorder
                .updates()
                .contains(&SessionUpdate::Steered { id, result: Ok(()) })
                && rows(&conversation)
                    .iter()
                    .filter(|row| *row == "text: use zsh")
                    .count()
                    == 2
        });
        assert!(!ended(&recorder), "the steer's result ended the turn");

        std::fs::write(server.dir.path().join("release-background"), "go").unwrap();
        let updates = recorder.wait_for_end(TURN_BUDGET);
        assert!(
            matches!(updates.last(), Some(SessionUpdate::Finished { outcome }) if outcome.is_completed()),
            "{updates:?}"
        );
        let rows = rows(&conversation);
        assert_eq!(
            rows[rows.len() - 4..],
            [
                "text: started",
                "text: use zsh",
                "text: use zsh",
                "text: finished"
            ],
            "{rows:?}"
        );
        let conversation = lock(&conversation);
        assert_eq!(conversation.phase(), Phase::Idle);
        let [task] = conversation.tasks() else {
            panic!("{:?}", conversation.tasks())
        };
        assert_eq!(task.status, fx_agent::TaskStatus::Completed);
        assert_eq!(task.subagent_type.as_deref(), Some("general-purpose"));
        assert_eq!(task.tool_uses, Some(4));
        assert!(
            task.summary
                .as_deref()
                .is_some_and(|summary| summary.contains("completed"))
        );
        // Every turn's usage: the first, the steer's, and the follow-up's.
        assert_eq!(conversation.usage().input_tokens, 15);
    }

    #[test]
    fn claude_reads_past_a_result_to_the_follow_up_of_a_task_that_already_ended() {
        // What a real run did: the task ended before the turn's `result`, and
        // Claude answered its notification after it. Stopping at that
        // `result` lost the answer.
        let Some(server) = ClaudeFixture::new() else {
            return;
        };
        let conversation = conversation();
        let (_runner, recorder) =
            server.start(&conversation, "background-quick", AgentAccess::FullAccess);
        let updates = recorder.wait_for_end(TURN_BUDGET);
        assert!(
            matches!(updates.last(), Some(SessionUpdate::Finished { outcome }) if outcome.is_completed()),
            "{updates:?}"
        );
        assert_eq!(
            rows(&conversation).last().map(String::as_str),
            Some("text: finished"),
            "{:?}",
            rows(&conversation)
        );
    }

    #[test]
    fn a_follow_up_can_still_be_given_a_permission_after_the_turns_result() {
        // The task ended before the turn's `result`, which closed Claude's
        // input, so the answer to the follow-up's permission request failed
        // with "input is closed" and the turn with it.
        let Some(server) = ClaudeFixture::new() else {
            return;
        };
        let conversation = conversation();
        let (_runner, recorder) = server.start(
            &conversation,
            "background-permission",
            AgentAccess::FullAccess,
        );
        let updates = recorder.wait_for_end(TURN_BUDGET);
        assert!(
            matches!(updates.last(), Some(SessionUpdate::Finished { outcome }) if outcome.is_completed()),
            "{updates:?}"
        );
        assert_eq!(
            rows(&conversation).last().map(String::as_str),
            Some("text: answered"),
            "{:?}",
            rows(&conversation)
        );
    }

    #[test]
    fn stopping_claude_while_it_waits_on_a_background_task_ends_the_process_group() {
        let Some(server) = ClaudeFixture::new() else {
            return;
        };
        let conversation = conversation();
        let (runner, recorder) =
            server.start(&conversation, "background-spawn", AgentAccess::FullAccess);
        let pid: libc::pid_t = wait_for_file(&server.dir.path().join("background.pid"))
            .trim()
            .parse()
            .unwrap();
        let _reap = KillOnDrop(pid);
        wait_for_the_background_wait(&conversation, &recorder);
        assert!(!ended(&recorder), "{:?}", recorder.updates());

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
        let deadline = Instant::now() + Duration::from_secs(5);
        while is_running(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!is_running(pid), "the background task outlived Stop");
        let conversation = lock(&conversation);
        assert!(!conversation.has_running_tasks());
        assert_eq!(
            conversation.tasks()[0].summary.as_deref(),
            Some("Stopped with the turn.")
        );
    }

    #[test]
    fn claude_stops_waiting_on_background_tasks_at_the_clis_own_ceiling() {
        let Some(server) = ClaudeFixture::new() else {
            return;
        };
        let conversation = conversation();
        let (_runner, recorder) = server.start_with_variable(
            &conversation,
            "background-spawn",
            (BACKGROUND_CEILING_VARIABLE, "400"),
        );
        let pid: libc::pid_t = wait_for_file(&server.dir.path().join("background.pid"))
            .trim()
            .parse()
            .unwrap();
        let _reap = KillOnDrop(pid);
        let updates = recorder.wait_for_end(TURN_BUDGET);
        // Graceful: the turn's answer stands, and the note says what happened.
        assert!(
            matches!(updates.last(), Some(SessionUpdate::Finished { outcome }) if outcome.is_completed()),
            "{updates:?}"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while is_running(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!is_running(pid), "the task outlived the ceiling");
        let conversation = lock(&conversation);
        let task = &conversation.tasks()[0];
        assert_eq!(task.status, fx_agent::TaskStatus::Stopped);
        assert!(
            task.summary
                .as_deref()
                .is_some_and(|note| note.contains("400 ms")),
            "{:?}",
            task.summary
        );
        assert!(conversation.activities().any(|activity| {
            activity
                .summary
                .contains("Stopped waiting for 1 background task")
        }),);
    }

    #[test]
    fn a_resumed_claude_session_ends_at_the_real_result_not_at_a_lost_tasks_acknowledgement() {
        // Resuming a session whose background tasks were killed opens with a
        // `stopped` notification per task and a zero-turn `result` per batch.
        // The first of those used to end the turn before Claude had read the
        // prompt, so the reply that followed landed nowhere.
        let Some(server) = ClaudeFixture::new() else {
            return;
        };
        let conversation = conversation();
        let (_runner, recorder) = server.start(&conversation, "resumed", AgentAccess::FullAccess);
        wait_for_file(&server.dir.path().join("preamble-sent"));
        std::thread::sleep(Duration::from_millis(300));
        let updates = recorder.updates();
        assert!(!ended(&recorder), "{updates:?}");
        assert!(
            !updates.contains(&SessionUpdate::Accepted),
            "the preamble was taken for Claude reading the prompt"
        );
        assert!(
            lock(&conversation).tasks().is_empty(),
            "a task the previous process lost was shown as this turn's"
        );

        std::fs::write(server.dir.path().join("release-resumed"), "go").unwrap();
        let updates = recorder.wait_for_end(TURN_BUDGET);
        assert!(
            matches!(updates.last(), Some(SessionUpdate::Finished { outcome }) if outcome.is_completed()),
            "{updates:?}"
        );
        assert!(updates.contains(&SessionUpdate::Accepted));
        assert_eq!(rows(&conversation), vec!["text: the real reply"]);
    }

    #[test]
    fn a_background_wait_ends_only_when_nothing_is_left_to_answer() {
        let task = |id: &str, status| fx_agent::TaskUpdate {
            id: id.into(),
            status,
            ..Default::default()
        };
        // A plain turn ends at its result.
        let mut wait = BackgroundWait::new(Some(Duration::from_secs(1)));
        assert_eq!(wait.result("end_turn".into(), false), AfterResult::End);

        // A running task holds it, and arms the ceiling while Claude is idle.
        let mut wait = BackgroundWait::new(Some(Duration::from_secs(1)));
        wait.task(&task("a", fx_agent::TaskStatus::Running));
        assert_eq!(wait.result("end_turn".into(), true), AfterResult::Wait);
        assert!(wait.deadline().is_some());
        assert_eq!(
            wait.completed_at_exit(),
            None,
            "an exit with a task running and the input open is a failure"
        );
        // A follow-up under way is progress, not idleness.
        wait.content();
        assert!(wait.deadline().is_none());
        wait.task(&task("a", fx_agent::TaskStatus::Completed));
        assert_eq!(wait.result("end_turn".into(), false), AfterResult::Drain);
        assert!(wait.deadline().is_none());
        assert_eq!(wait.completed_at_exit().as_deref(), Some("end_turn"));
        // Dying part-way through a follow-up is not a completion.
        wait.content();
        assert_eq!(wait.completed_at_exit(), None);

        // No ceiling configured means waiting indefinitely.
        let mut wait = BackgroundWait::new(None);
        wait.task(&task("a", fx_agent::TaskStatus::Running));
        wait.result("end_turn".into(), true);
        assert!(wait.deadline().is_none());
    }

    #[test]
    fn an_ended_tasks_answer_is_still_waited_for_and_still_bounded() {
        let task = |status| fx_agent::TaskUpdate {
            id: "a".into(),
            status,
            ..Default::default()
        };
        let mut wait = BackgroundWait::new(Some(Duration::from_secs(1)));
        wait.task(&task(fx_agent::TaskStatus::Running));
        wait.task(&task(fx_agent::TaskStatus::Completed));
        // The task ended before the `result`, and the transport kept the input
        // open for its answer: not the end, and not a drain either.
        assert_eq!(wait.result("end_turn".into(), true), AfterResult::Wait);
        assert!(!wait.input_closed, "steering is still possible");
        assert!(
            wait.deadline().is_some(),
            "waiting on an answer still has a ceiling"
        );
    }

    #[test]
    fn a_tasks_end_measures_the_ceiling_again() {
        let task = |id: &str, status| fx_agent::TaskUpdate {
            id: id.into(),
            status,
            ..Default::default()
        };
        let mut wait = BackgroundWait::new(Some(Duration::from_secs(60)));
        wait.task(&task("a", fx_agent::TaskStatus::Running));
        wait.task(&task("b", fx_agent::TaskStatus::Running));
        wait.result("end_turn".into(), true);
        let first = wait.deadline().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        wait.task(&task("a", fx_agent::TaskStatus::Completed));
        assert!(
            wait.deadline().unwrap() > first,
            "the answer to a task's end was given only what was left of the old wait"
        );
        // A task this turn never started moves nothing.
        let second = wait.deadline().unwrap();
        wait.task(&task("lost", fx_agent::TaskStatus::Stopped));
        assert_eq!(wait.deadline(), Some(second));
    }

    #[test]
    fn a_steer_is_waited_for_past_the_result_it_raced() {
        // A steer written just before the `result` is answered after it. The
        // turn used to end there, and its process was stopped 750 ms later,
        // in the middle of the answer to a steer reported as accepted.
        let mut wait = BackgroundWait::new(Some(Duration::from_secs(1)));
        wait.steered();
        assert_eq!(wait.result("end_turn".into(), false), AfterResult::Drain);
        assert!(wait.input_closed);
        assert_eq!(wait.completed_at_exit().as_deref(), Some("end_turn"));

        // While Claude idles on a task, a steer's answer is a turn in
        // progress: the ceiling is off until its `result`.
        let mut wait = BackgroundWait::new(Some(Duration::from_secs(1)));
        wait.task(&fx_agent::TaskUpdate {
            id: "a".into(),
            status: fx_agent::TaskStatus::Running,
            ..Default::default()
        });
        wait.result("end_turn".into(), true);
        assert!(wait.deadline().is_some());
        wait.steered();
        assert!(
            wait.deadline().is_none(),
            "the ceiling could cut the answer off"
        );
        wait.result("end_turn".into(), true);
        assert!(wait.deadline().is_some());
    }

    #[test]
    fn background_tasks_are_waited_for_unless_a_ceiling_was_set() {
        let read = |pairs: &[(&str, &str)]| {
            let environment: std::collections::BTreeMap<String, String> = pairs
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect();
            background_ceiling(environment.iter())
        };
        // The CLI's own ten-minute default is for an unattended `-p` run. Here
        // it stopped agents that were still working.
        assert_eq!(read(&[]), None);
        assert_eq!(
            read(&[(BACKGROUND_CEILING_VARIABLE, "1500")]),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(read(&[(BACKGROUND_CEILING_VARIABLE, "0")]), None);
        assert_eq!(read(&[(BACKGROUND_CEILING_VARIABLE, "soon")]), None);
        assert_eq!(spoken_duration(Duration::from_secs(600)), "10 min");
        assert_eq!(spoken_duration(Duration::from_millis(400)), "400 ms");
    }

    #[test]
    fn only_compact_and_goal_are_slash_commands() {
        use fx_core::models::GoalStatus;
        assert_eq!(
            ThreadControl::parse(" /compact "),
            Some(ThreadControl::Compact(String::new()))
        );
        assert_eq!(
            ThreadControl::parse("/Compact"),
            Some(ThreadControl::Compact(String::new()))
        );
        // What to keep goes with it: Claude's `/compact` takes instructions.
        let focused = ThreadControl::parse("/compact  keep the API decisions ").unwrap();
        assert_eq!(
            focused,
            ThreadControl::Compact("keep the API decisions".into())
        );
        assert_eq!(
            focused.claude_prompt().as_deref(),
            Some("/compact keep the API decisions")
        );
        assert_eq!(
            ThreadControl::Compact(String::new())
                .claude_prompt()
                .as_deref(),
            Some("/compact")
        );
        assert!(focused.instructions_supported_by("claude"));
        assert!(!focused.instructions_supported_by("codex"));
        assert!(ThreadControl::Compact(String::new()).instructions_supported_by("codex"));
        assert_eq!(ThreadControl::parse("/goal"), Some(ThreadControl::GoalView));
        assert_eq!(
            ThreadControl::parse("/goal pause"),
            Some(ThreadControl::GoalStatus(GoalStatus::Paused))
        );
        assert_eq!(
            ThreadControl::parse("/goal resume"),
            Some(ThreadControl::GoalStatus(GoalStatus::Active))
        );
        assert_eq!(
            ThreadControl::parse("/goal CLEAR"),
            Some(ThreadControl::GoalClear)
        );
        assert_eq!(
            ThreadControl::parse("/goal  Ship the\nrelease "),
            Some(ThreadControl::GoalSet("Ship the\nrelease".into()))
        );
        // A provider's own command, and prose that mentions one, are prompts.
        assert_eq!(ThreadControl::parse("/review the diff"), None);
        assert_eq!(ThreadControl::parse("please /compact"), None);
        assert_eq!(ThreadControl::parse("/compactor"), None);
        assert!(ThreadControl::Compact(String::new()).supported_by("claude"));
        assert!(!ThreadControl::GoalView.supported_by("claude"));
        assert!(ThreadControl::GoalClear.supported_by("codex"));
    }

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
