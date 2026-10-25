//! The Claude transport: one `claude` process and the stream-JSON conversation
//! FlowX holds with it.
//!
//! Codex speaks JSON-RPC, so the half of its transport worth testing on its own
//! is the correlator: which reply belongs to which request. Claude has no ids at
//! all — it narrates, one JSON object per line, and every line belongs to the
//! turn that is running. The half worth testing on its own here is therefore the
//! *outgoing* half: what goes into stdin, in what order, and whether stdin may
//! still be written at all.
//!
//! Four rules carry this file. Each of them is a bug that had to be written
//! first.
//!
//! - **There is no handshake.** `claude -p --input-format stream-json` reads the
//!   user's message as its first input and starts talking at once, so there is
//!   nothing to wait for and nothing to send first. In particular the transport
//!   does *not* report the turn as started when it writes the prompt: the
//!   `system`/`init` frame the process emits on its own already does that, and a
//!   second report would show the spinner twice.
//! - **`result` is the only completion signal.** A tool call, a gap in the
//!   stream, or a process that has gone quiet all mean the turn is still running.
//!   The process ending first is a failure that carries stderr, because a turn
//!   that stops without saying it was done reads as a model that gave up. The
//!   converse holds too: a `result` wins over whatever the process does
//!   afterwards. The Swift original failed a turn whose exit status was non-zero
//!   *after* a complete answer, discarding a finished response over a status
//!   code.
//! - **stdin closes at a `result` with no background task running.** That is
//!   not tidiness, it is what makes the process exit: a `-p` run that has
//!   answered and still holds its input pipe open never terminates, so the turn
//!   would hang after its own answer. Every write after that point is refused
//!   rather than dropped into a closed pipe, where it would vanish with no error
//!   anywhere. While a task runs the input stays open: Claude answers the task's
//!   notification as a turn of its own, and an open input is what lets the user
//!   steer meanwhile. Once it closes, Claude still runs any follow-up it has
//!   queued, waits for tasks a follow-up started, and then exits.
//! - **stderr is captured, not inherited.** A CLI that cannot start says why
//!   there, and that text is the only useful diagnostic for a turn that failed
//!   before saying anything.
//!
//! Framing is [`crate::codex::framing::LineFramer`], shared with the Codex
//! transport. Reusing the framer does not mean reusing its limits: Claude's line
//! budget is its own, below.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use fx_core::AgentAccess;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::claude::stream::ClaudeStreamParser;
use crate::codex::framing::LineFramer;
use crate::provider::ProviderEvent;

/// The largest single line accepted from Claude, in bytes.
///
/// The Swift original gave Claude its own 32 MB budget rather than reusing
/// Codex's 64 MB. Reusing the framer does not mean reusing the number: Claude's
/// line is a turn of a conversation, and 64 MB of one is a conversation nobody
/// can render.
pub const MAXIMUM_LINE_BYTES: usize = 32 * 1024 * 1024;

/// Stderr kept for diagnostics.
///
/// 256 KB, as in the Swift original, and larger than the Codex transport's
/// 64 KB. This stream is the only thing that explains a CLI that could not start
/// or a turn that died before its first word, so it is worth keeping whole.
pub const MAX_STDERR_BYTES: usize = 256 * 1024;

/// The most image bytes FlowX will put into one message.
///
/// The Swift original refused anything `ProviderAttachmentStore` would not have
/// prepared (25 MB), because the bytes are base64'd into a single JSON line that
/// the reader and the writer both hold at once.
pub const MAX_IMAGE_BYTES: usize = 25 * 1024 * 1024;

/// How long the process gets to exit, and its stderr to drain, once stdout ends.
///
/// Bounded because stdout can close without the process exiting, and a reader
/// that waits for one that never leaves reports nothing at all — which is the
/// failure this transport exists to avoid.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// How long a closing process is given to end on its own before it is killed.
const CLOSE_GRACE: Duration = Duration::from_millis(750);

/// How long a stopped turn's process group gets to exit on SIGTERM before it
/// is killed.
const TERMINATE_GRACE: Duration = Duration::from_millis(500);

/// The id FlowX gives its interrupt control request. One process serves one
/// turn, so one id is enough.
const INTERRUPT_REQUEST_ID: &str = "flowx-interrupt";

/// Why a Claude turn could not be driven.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("Claude Code could not be started: {0}")]
    Spawn(String),
    #[error("Claude Code's input is closed, so this turn can no longer be steered")]
    InputClosed,
    #[error("there is nothing to send to Claude Code: the prompt is empty and no image was given")]
    NothingToSend,
    #[error("Claude Code cannot send the prepared image format '{0}'")]
    UnsupportedImageFormat(String),
    #[error(
        "Claude Code cannot send the image '{path}': {bytes} bytes is empty or over the {MAX_IMAGE_BYTES} byte limit"
    )]
    ImageTooLarge { path: String, bytes: usize },
    #[error("no Claude request is open with the id '{id}'")]
    UnknownControl { id: String },
    #[error(
        "'{id}' is not a Claude {expected}, and asking the parser to answer it as one has already consumed it"
    )]
    NotAControl { id: String, expected: &'static str },
    #[error("{reason}")]
    Vanished { reason: String },
    #[error("{reason}")]
    Failed { reason: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("could not encode a message for Claude Code: {0}")]
    Encode(#[from] serde_json::Error),
}

/// What ended a Claude process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exit {
    /// Whatever stderr managed to say.
    pub stderr: String,
    /// The exit status, when the process could be waited for.
    pub code: Option<i32>,
}

impl Exit {
    /// The sentence shown to the user for a turn that did not finish.
    ///
    /// Every branch produces something. A killed or OOM'd process leaves stderr
    /// empty and no status to report, and silence there would make the turn look
    /// like one that simply ran out of things to say.
    pub fn reason(&self) -> String {
        let stderr = self.stderr.trim();
        if !stderr.is_empty() {
            return format!("Claude Code exited: {stderr}");
        }
        match self.code {
            Some(code) => format!("Claude Code exited with status {code}."),
            None => "Claude Code exited unexpectedly.".to_string(),
        }
    }
}

/// How a turn ended.
///
/// The split exists because the two endings are not the same event: one is Claude
/// saying it is done, the other is Claude not saying anything at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEnd {
    /// Claude sent `result`. What the process did afterwards is irrelevant.
    Completed { stop_reason: String },
    /// Claude sent a `result` that reported an error, and this is its reason.
    Failed { reason: String },
    /// The process ended without one.
    Vanished { reason: String },
}

impl TurnEnd {
    /// Decide from what the stream reported and what the process did.
    pub fn decide(stop_reason: Option<String>, exit: Option<&Exit>) -> Self {
        match stop_reason {
            Some(stop_reason) => Self::Completed { stop_reason },
            None => Self::Vanished {
                reason: exit
                    .map(Exit::reason)
                    .unwrap_or_else(|| "Claude Code ended before reporting a result.".to_string()),
            },
        }
    }

    /// The stop reason, or the failure to report instead.
    pub fn into_result(self) -> Result<String, TransportError> {
        match self {
            Self::Completed { stop_reason } => Ok(stop_reason),
            Self::Failed { reason } => Err(TransportError::Failed { reason }),
            Self::Vanished { reason } => Err(TransportError::Vanished { reason }),
        }
    }

    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }
}

/// One thing read from a running turn.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamItem {
    /// Something Claude said, already decoded by the stream parser.
    Event(ProviderEvent),
    /// The process ended, with whatever it left behind.
    ///
    /// Sent exactly once, and always last, so a caller that reads to the end
    /// learns why the stream stopped rather than finding the channel closed.
    Exited(Exit),
}

/// What changes one turn's command line.
///
/// This is the Swift `buildArguments`, as data, so the flags can be asserted
/// without spawning anything — which is the only way to catch a typo in a flag
/// Claude rejects at startup.
#[derive(Debug, Clone, Default)]
pub struct TurnRequest {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub system_prompt: Option<String>,
    /// `None` means the caller has no access mode to express, which is a
    /// different thing from every mode there is.
    pub access: Option<AgentAccess>,
    pub resume_session_id: Option<String>,
    /// The prepared attachment directory, when there are attachments.
    pub attachment_directory: Option<String>,
}

impl TurnRequest {
    /// The command line, in the order Claude reads it.
    ///
    /// None of the fixed flags are decoration. `-p` is what makes this a
    /// non-interactive run at all; the two `--*-format` flags are what turn
    /// stdio into the stream-JSON conversation this transport reads;
    /// `--permission-prompt-tool stdio` is what routes every tool request to
    /// FlowX as a `control_request` instead of a prompt nobody can answer; and
    /// `--include-partial-messages` produces nothing at all without `--verbose`.
    pub fn arguments(&self) -> Vec<String> {
        let mut arguments = vec![
            "-p".to_string(),
            "--input-format".to_string(),
            "stream-json".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--permission-prompt-tool".to_string(),
            "stdio".to_string(),
            "--verbose".to_string(),
            "--include-partial-messages".to_string(),
        ];
        if let Some(model) = non_empty(self.model.as_deref()) {
            arguments.push("--model".into());
            arguments.push(model);
        }
        if let Some(effort) = non_empty(self.effort.as_deref()) {
            arguments.push("--effort".into());
            // Claude's flag is lower-case; the settings file that carries the
            // user's choice is not.
            arguments.push(effort.to_lowercase());
        }
        if let Some(system_prompt) = non_empty(self.system_prompt.as_deref()) {
            arguments.push("--append-system-prompt".into());
            arguments.push(system_prompt);
        }
        match self.access {
            Some(AgentAccess::AcceptEdits) => push_permission_mode(&mut arguments, "acceptEdits"),
            // Full Access is `manual`, and deliberately not
            // `--dangerously-skip-permissions`: with permissions skipped Claude
            // answers `AskUserQuestion` itself and never emits a `can_use_tool`,
            // so FlowX cannot show the question. The caller auto-approves the
            // ordinary tool calls instead.
            Some(AgentAccess::Supervised) | Some(AgentAccess::FullAccess) => {
                push_permission_mode(&mut arguments, "manual")
            }
            None => {}
        }
        if let Some(session) = non_empty(self.resume_session_id.as_deref()) {
            arguments.push("--resume".into());
            arguments.push(session);
        }
        if let Some(directory) = non_empty(self.attachment_directory.as_deref()) {
            arguments.push("--add-dir".into());
            arguments.push(directory);
        }
        arguments
    }
}

fn push_permission_mode(arguments: &mut Vec<String>, mode: &str) {
    arguments.push("--permission-mode".into());
    arguments.push(mode.into());
}

/// How to launch one Claude turn.
#[derive(Debug, Clone)]
pub struct Launch {
    /// The `claude` binary, as resolved by runtime discovery.
    pub executable: String,
    /// The command line, from [`TurnRequest::arguments`].
    pub args: Vec<String>,
    /// The workspace the turn runs in, when the caller has one.
    pub working_directory: Option<String>,
    /// The environment to launch with, replacing this process's own.
    ///
    /// Replaced rather than extended, for the reason runtime discovery gives: a
    /// GUI app cannot trust its own `PATH`, so the search rebuilds it around the
    /// directory the binary was found in.
    pub environment: Vec<(String, String)>,
}

impl Launch {
    /// The standard invocation for one turn.
    pub fn claude(executable: impl Into<String>, request: &TurnRequest) -> Self {
        Self {
            executable: executable.into(),
            args: request.arguments(),
            working_directory: None,
            environment: Vec::new(),
        }
    }
}

/// Everything FlowX writes to one turn: the payloads, and the gate that stops
/// them once the turn is over.
///
/// Pure state, deliberately. What goes into stdin, and whether stdin may still
/// be written at all, are the rules a mocked stream cannot reach and the rules
/// that lose a turn silently when they are wrong: by the time a late steering
/// message arrives, Claude has exited, and the bytes land in a pipe nobody reads.
#[derive(Debug)]
pub struct TurnInput {
    open: bool,
}

impl TurnInput {
    pub fn new() -> Self {
        Self { open: true }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The envelope for one user message: the prompt, then any images.
    ///
    /// The empty `session_id` and the null `parent_tool_use_id` are the shape
    /// Claude reads rather than placeholders: a user frame claiming a session
    /// Claude never handed out is not a frame it will act on.
    pub fn user_message(
        &mut self,
        prompt: &str,
        images: &[impl AsRef<Path>],
    ) -> Result<Value, TransportError> {
        self.gate()?;
        let mut content: Vec<Value> = Vec::new();
        if !prompt.is_empty() {
            content.push(json!({ "type": "text", "text": prompt }));
        }
        for image in images {
            content.push(image_content_block(image.as_ref())?);
        }
        // A user frame with no content is a protocol error, and Claude reports it
        // as one without any of the context that would say it was FlowX's
        // mistake. Refusing it here names the side at fault.
        if content.is_empty() {
            return Err(TransportError::NothingToSend);
        }
        Ok(json!({
            "type": "user",
            "session_id": "",
            "message": { "role": "user", "content": content },
            "parent_tool_use_id": Value::Null,
        }))
    }

    /// A reply to something Claude asked: an approval, a denial, an answer.
    ///
    /// Takes the envelope rather than building one. The parser owns the shapes
    /// Claude accepts and the ids it hands out; this half only decides whether one
    /// may still be sent.
    pub fn answer(&mut self, response: Value) -> Result<Value, TransportError> {
        self.gate()?;
        Ok(response)
    }

    /// Close stdin. `false` when it was already closed.
    pub fn close(&mut self) -> bool {
        std::mem::replace(&mut self.open, false)
    }

    fn gate(&self) -> Result<(), TransportError> {
        if self.open {
            Ok(())
        } else {
            Err(TransportError::InputClosed)
        }
    }
}

impl Default for TurnInput {
    fn default() -> Self {
        Self::new()
    }
}

/// One inline image, in the shape Claude reads.
///
/// The media type comes from the extension rather than from sniffing the bytes,
/// and an extension Claude does not accept is an error rather than a guess. It
/// takes four formats — `imageContentBlock` in the Swift original enumerated
/// exactly those — and quietly sending a fifth produces a request Claude answers
/// with a complaint about an image nobody asked it to look at.
fn image_content_block(path: &Path) -> Result<Value, TransportError> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let media_type = match extension.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        // Named by file name when there is no extension, because "the prepared
        // image format ''" explains nothing.
        _ => {
            return Err(TransportError::UnsupportedImageFormat(
                path.file_name()
                    .and_then(|name| name.to_str())
                    .filter(|name| !name.is_empty())
                    .unwrap_or("unnamed")
                    .to_string(),
            ));
        }
    };

    let bytes = std::fs::read(path)?;
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return Err(TransportError::ImageTooLarge {
            path: path.display().to_string(),
            bytes: bytes.len(),
        });
    }
    Ok(json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": media_type,
            "data": base64(&bytes),
        },
    }))
}

/// What the writer task has been asked to do.
#[derive(Debug)]
enum Write {
    /// One message, already encoded and terminated.
    Message(Vec<u8>),
    /// Close stdin.
    ///
    /// Sent as a message rather than done by dropping the sender, because the
    /// transport shares one sender with the reader task: closing has to be
    /// ordered after the last refusal, and dropping would close it at once.
    Close,
}

/// State the transport and its reader task both reach.
///
/// One holder rather than four cells passed separately, because there is exactly
/// one reader and it must see the same write gate the transport holds: the gate
/// is what stops the UI steering a turn whose stdin the reader has already closed.
#[derive(Debug)]
struct Inner {
    /// The process, taken once it is reaped.
    child: Mutex<Option<Child>>,
    /// The process's own group, which the tools it runs inherit. Kept apart
    /// from `child` because the group outlives a reaped leader.
    group: Option<crate::process::Group>,
    /// The process's place in the list Quit ends, until it closes.
    running: std::sync::Mutex<Option<crate::process::Running>>,
    /// The parser, shared so an approval can be answered from a task other than
    /// the one reading the stream.
    parser: Mutex<ClaudeStreamParser>,
    /// The write gate, shared because the reader closes it at `result`.
    input: Mutex<TurnInput>,
    /// What the reader saw when the process ended.
    exited: Mutex<Option<Exit>>,
}

/// The live connection to one `claude` process.
///
/// The reader end sits behind a lock and every method takes `&self`, which is a
/// deliberate difference from the Codex transport. A Claude turn is not
/// self-answering: an approval waits for the *user*, who answers from a different
/// task than the one reading the stream. Keeping the receiver shared is what lets
/// one `Arc<ClaudeTransport>` serve both.
#[derive(Debug)]
pub struct ClaudeTransport {
    inner: Arc<Inner>,
    stdin: mpsc::Sender<Write>,
    events: Mutex<mpsc::Receiver<StreamItem>>,
}

impl ClaudeTransport {
    /// Native session id captured from the stream, never a locally invented id.
    pub async fn session_id(&self) -> Option<String> {
        self.inner
            .parser
            .lock()
            .await
            .session_id()
            .map(str::to_owned)
    }

    /// Start a process for one turn.
    ///
    /// There is nothing to wait for. Claude answers the first prompt it is given,
    /// so a caller that wants a turn started sends one.
    pub async fn start(launch: Launch) -> Result<Self, TransportError> {
        let mut command = Command::new(&launch.executable);
        command
            .args(&launch.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Captured rather than inherited, for the reason the Codex transport
            // captures it: a CLI that cannot start explains itself here.
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Its own group, so Stop can reach the tools it runs as well as itself.
        crate::process::in_own_group(&mut command);
        if let Some(directory) = &launch.working_directory {
            command.current_dir(directory);
        }
        command.env_clear();
        for (key, value) in &launch.environment {
            command.env(key, value);
        }

        let mut child = command
            .spawn()
            .map_err(|error| TransportError::Spawn(format!("{}: {error}", launch.executable)))?;
        let group = crate::process::Group::led_by(&child);

        let stdout_pipe = child.stdout.take();
        let stdout = take_pipe(&mut child, stdout_pipe, "stdout")?;
        let stderr_pipe = child.stderr.take();
        let stderr = take_pipe(&mut child, stderr_pipe, "stderr")?;
        let stdin_pipe = child.stdin.take();
        let stdin = take_pipe(&mut child, stdin_pipe, "stdin")?;

        // Bounded, for the same reason the Codex one is: a transport outlives
        // its turn, and an unbounded queue on either side grows the whole time.
        let (stdin_tx, stdin_rx) = mpsc::channel::<Write>(WRITE_QUEUE_DEPTH);
        tokio::spawn(write_stdin_loop(stdin, stdin_rx));

        let (stderr_tx, stderr_rx) = oneshot::channel::<String>();
        tokio::spawn(async move {
            // Raw bytes, capped as they are read. A line reader stopped at the
            // first byte that was not UTF-8, which closed the pipe under a CLI
            // still writing to it, and `\r`-only progress output was one line
            // that grew without bound before any cap applied.
            let mut stderr = stderr;
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 8 * 1024];
            while let Ok(read) = stderr.read(&mut chunk).await {
                if read == 0 {
                    break;
                }
                crate::process::keep_tail(&mut buffer, &chunk[..read], MAX_STDERR_BYTES);
            }
            let _ = stderr_tx.send(crate::process::decode(&buffer));
        });

        let inner = Arc::new(Inner {
            child: Mutex::new(Some(child)),
            group,
            running: std::sync::Mutex::new(group.map(crate::process::Running::register)),
            parser: Mutex::new(ClaudeStreamParser::new()),
            input: Mutex::new(TurnInput::new()),
            exited: Mutex::new(None),
        });
        let (events_tx, events_rx) = mpsc::channel::<StreamItem>(EVENT_QUEUE_DEPTH);

        let reader = tokio::spawn(read_stdout(ReadParts {
            stdout,
            stderr: stderr_rx,
            inner: inner.clone(),
            stdin: stdin_tx.clone(),
            events: events_tx.clone(),
        }));

        // The reader is the only task that reports an exit, so it cannot also
        // report one twice. This covers the one case it does not: if the reader
        // itself dies, a closed channel would read as a turn that ended normally.
        let supervisor = inner.clone();
        let supervisor_events = events_tx.clone();
        tokio::spawn(async move {
            if let Err(error) = reader.await {
                let mut slot = supervisor.exited.lock().await;
                if slot.is_none() {
                    let exit = Exit {
                        stderr: format!("Claude Code's output reader stopped: {error}"),
                        code: None,
                    };
                    *slot = Some(exit.clone());
                    let _ = supervisor_events.send(StreamItem::Exited(exit)).await;
                }
            }
        });
        // The last sender is dropped here, so the stream ends when the tasks do.
        drop(events_tx);

        Ok(Self {
            inner,
            stdin: stdin_tx,
            events: Mutex::new(events_rx),
        })
    }

    /// Send a prompt, with any images the user attached.
    ///
    /// The Swift original had two entry points for this — the initial prompt and
    /// a mid-turn steer — that differed in nothing but their name, so there is one
    /// here. Steering is the same write: Claude is blocked on a reply, not on a
    /// new turn.
    pub async fn send_prompt(
        &self,
        prompt: &str,
        images: &[impl AsRef<Path>],
    ) -> Result<(), TransportError> {
        // The gate stays locked until the write is queued. Checked under the
        // lock and queued after it, a close could land in between: its `Close`
        // went first, the writer stopped there, and the prompt the caller was
        // told had been sent was discarded.
        let mut input = self.inner.input.lock().await;
        let envelope = input.user_message(prompt, images)?;
        self.write(envelope).await
    }

    /// Answer a tool Claude asked to run.
    pub async fn approve(&self, id: &str, approved: bool) -> Result<(), TransportError> {
        self.reply(id, "approval", |parser| parser.approve(id, approved))
            .await
    }

    /// Answer questions Claude asked.
    ///
    /// `answers` pairs the question ids the app exposed with the chosen labels,
    /// which is what lets a multi-select or a typed answer reach the right
    /// question instead of a positional index.
    pub async fn answer_questions(
        &self,
        id: &str,
        answers: &[(String, String)],
    ) -> Result<(), TransportError> {
        self.reply(id, "question", |parser| {
            parser.answer_questions(id, answers)
        })
        .await
    }

    /// Decline a question, which is how a turn that cannot be answered ends.
    pub async fn decline_questions(&self, id: &str) -> Result<(), TransportError> {
        self.reply(id, "question", |parser| parser.decline_questions(id))
            .await
    }

    /// Write a `control_response` envelope the caller built itself.
    ///
    /// The path for a request shape the parser does not model:
    /// [`crate::claude::stream::refuse_control`] produces one.
    pub async fn respond(&self, response: Value) -> Result<(), TransportError> {
        // One critical section with the queueing, as in `send_prompt`.
        let mut input = self.inner.input.lock().await;
        let envelope = input.answer(response)?;
        self.write(envelope).await
    }

    /// Ask Claude to stop the turn it is running.
    ///
    /// The SDK's own interrupt control request, which is what lets Claude end
    /// its turn and its tools itself before the process is signalled.
    pub async fn interrupt(&self) -> Result<(), TransportError> {
        self.respond(json!({
            "type": "control_request",
            "request_id": INTERRUPT_REQUEST_ID,
            "request": { "subtype": "interrupt" },
        }))
        .await
    }

    /// Whether Claude has shown it read the prompt — any frame that answers
    /// it, including thinking, usage and lifecycle frames that are never shown.
    pub async fn prompt_consumed(&self) -> bool {
        self.inner.parser.lock().await.prompt_consumed()
    }

    /// The control requests still waiting on an answer.
    ///
    /// A caller ending a turn refuses each of these *before* closing stdin.
    /// Closing first is what hangs: Claude blocks on every control request it
    /// makes, and once its input is gone it can never be given the reply it is
    /// waiting for.
    pub async fn pending_controls(&self) -> Vec<String> {
        self.inner.parser.lock().await.pending_ids()
    }

    /// Take the stream reader.
    ///
    /// Reading is exclusive, and this is what makes that true by construction
    /// rather than by convention: there is one reader, and taking a second one is
    /// impossible. Without that, a second reader splits the stream
    /// nondeterministically and then waits on a channel that will never produce
    /// its `Done` — and the guard held across `recv().await` serialises it behind
    /// whichever task is already waiting.
    ///
    /// Answering an approval is a different concern and stays on the transport,
    /// because that genuinely happens from another task while this one reads.
    pub async fn reader(&mut self) -> StreamReader {
        // The old receiver is moved out and a closed one left behind, so a second
        // `reader()` yields an empty reader rather than a second consumer of the
        // same channel.
        let (_sender, empty) = mpsc::channel::<StreamItem>(1);
        let taken = std::mem::replace(&mut *self.events.lock().await, empty);
        StreamReader {
            events: Mutex::new(taken),
            inner: self.inner.clone(),
        }
    }
}

/// Stream items held before the reader blocks.
pub const EVENT_QUEUE_DEPTH: usize = 1024;

/// Pending writes held before a write blocks.
pub const WRITE_QUEUE_DEPTH: usize = 64;

/// The single consumer of a Claude turn's stream.
#[derive(Debug)]
pub struct StreamReader {
    events: Mutex<mpsc::Receiver<StreamItem>>,
    /// Shared so a channel that closes without an `Exited` item can still say
    /// why, rather than reporting that the reader stopped for no reason.
    inner: Arc<Inner>,
}

impl StreamReader {
    /// The next thing the stream said, if anything.
    ///
    /// Takes `&mut self` for the same reason `reader()` is exclusive: there is
    /// exactly one consumer, so nothing can take the `Done` this call is waiting
    /// for. The channel is not behind a lock at all, which is what removed the
    /// guard that used to be held across the whole wait.
    pub async fn next_event(&self) -> Option<StreamItem> {
        self.events.lock().await.recv().await
    }

    /// Read the stream until the turn ends.
    ///
    /// Stops at the `result` frame, which is the parser's completion signal and
    /// the only one. A process that ends first is reported as a failure carrying
    /// stderr, because a turn that stops without saying it was done looks to the
    /// user like a model that gave up halfway.
    pub async fn wait_for_end(&self) -> TurnEnd {
        loop {
            match self.next_event().await {
                Some(StreamItem::Event(ProviderEvent::Done { stop_reason })) => {
                    return TurnEnd::decide(Some(stop_reason), None);
                }
                // An errored `result` is the end of the turn too. Waiting past
                // it for the process to exit reported the exit status instead of
                // the reason Claude gave.
                Some(StreamItem::Event(ProviderEvent::Error(reason))) => {
                    return TurnEnd::Failed { reason };
                }
                Some(StreamItem::Event(_)) => {}
                Some(StreamItem::Exited(exit)) => return TurnEnd::decide(None, Some(&exit)),
                None => {
                    // The reader is gone without having reported an exit, which
                    // only its own supervisor can do. Something is reported here
                    // rather than a completion nobody sent.
                    let exit = self.inner.exited.lock().await.clone();
                    return TurnEnd::decide(None, exit.as_ref());
                }
            }
        }
    }
}

impl ClaudeTransport {
    /// What ended the process, once it has ended.
    pub async fn exit_reason(&self) -> Option<Exit> {
        self.inner.exited.lock().await.clone()
    }

    /// Whether stdin is still open. Settled by the time a `result`'s events
    /// are read: the reader closes it, when it does, before delivering them.
    pub async fn input_open(&self) -> bool {
        self.inner.input.lock().await.is_open()
    }

    /// Close stdin, which is how Claude is told the turn is over.
    pub async fn close_input(&self) {
        close_stdin(&self.inner.input, &self.stdin).await;
    }

    /// Stop the process.
    ///
    /// The input is closed first and the process is given a moment to end on its
    /// own before it is killed. The order is not tidiness: the refusals written
    /// just before stdin closed are still sitting in the pipe, and killing on top
    /// of them loses the answers to whatever Claude is blocked on. The Swift
    /// original escalated the same way — interrupt, then terminate, then kill —
    /// over about a second and a quarter.
    pub async fn close(&self) {
        self.close_input().await;
        let mut slot = self.inner.child.lock().await;
        if let Some(mut child) = slot.take()
            && tokio::time::timeout(CLOSE_GRACE, child.wait())
                .await
                .is_err()
        {
            crate::process::stop_leader(&mut child, CLOSE_GRACE).await;
        }
        drop(slot);
        self.leave_running();
    }

    /// Stop the process and everything it started, for Stop or a failed turn.
    ///
    /// The input is closed first, for the reason `close` gives, and the
    /// process gets the same moment to end on its own. Then the whole tree
    /// gets SIGTERM, a grace period, and SIGKILL — whether or not Claude itself
    /// has gone, because a tool it started can outlive it. The tree is taken
    /// before any of that: Claude starts each `Bash` call in a session of its
    /// own, outside its group, and once Claude has exited nothing but that
    /// record still leads to the tool. A caller stopping a turn sends
    /// [`Self::interrupt`] before this.
    pub async fn terminate(&self) {
        let tree = self.inner.group.map(crate::process::Group::tree);
        self.close_input().await;
        let mut slot = self.inner.child.lock().await;
        if let Some(child) = slot.as_mut() {
            let _ = tokio::time::timeout(CLOSE_GRACE, child.wait()).await;
        }
        match (tree, slot.as_mut()) {
            (Some(tree), leader) => tree.terminate(leader, TERMINATE_GRACE).await,
            (None, Some(child)) => crate::process::stop_leader(child, TERMINATE_GRACE).await,
            (None, None) => {}
        }
        slot.take();
        drop(slot);
        self.leave_running();
    }

    /// Leave the list Quit ends: what is left now is not FlowX's to stop.
    fn leave_running(&self) {
        self.inner
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    async fn write(&self, envelope: Value) -> Result<(), TransportError> {
        let mut bytes = serde_json::to_vec(&envelope)?;
        // One terminator, and that is all: `serde_json` escapes newlines inside
        // strings, so a multi-line prompt or a base64 blob cannot introduce a
        // second line into the stream.
        bytes.push(b'\n');
        // A closed channel means the writer task is gone, which means the process
        // is gone. Both are "the input is closed" as far as a caller can act.
        self.stdin
            .send(Write::Message(bytes))
            .await
            .map_err(|_| TransportError::InputClosed)
    }

    /// One answer, sent, or refused with a reason the caller can act on.
    async fn reply(
        &self,
        id: &str,
        expected: &'static str,
        build: impl FnOnce(&mut ClaudeStreamParser) -> Option<Value>,
    ) -> Result<(), TransportError> {
        let (envelope, kind) = {
            let mut parser = self.inner.parser.lock().await;
            // Read the kind before answering. It is what separates "no such
            // request" from "that request is not this kind" — two different
            // mistakes for the caller, and guessing between them means either
            // losing a pending question or reporting a wrong reason.
            let kind = parser.kind_of(id);
            (build(&mut parser), kind)
        };
        let Some(envelope) = envelope else {
            return Err(match kind {
                // The id is held, so it is the wrong kind of request. Refusing
                // here matters: Claude is still blocked on it, and nothing else
                // will answer it.
                Some(_) => TransportError::NotAControl {
                    id: id.to_string(),
                    expected,
                },
                None => TransportError::UnknownControl { id: id.to_string() },
            });
        };
        self.respond(envelope).await
    }
}

impl Drop for ClaudeTransport {
    fn drop(&mut self) {
        // `kill_on_drop` covers the common case; this is the belt to that braces,
        // for a transport dropped outside an async context where the runtime
        // cannot reap the child.
        if let Ok(mut slot) = self.inner.child.try_lock()
            && let Some(child) = slot.as_mut()
        {
            let _ = child.start_kill();
        }
    }
}

/// The pipes one reader task needs, gathered so the spawn function stays
/// readable without asking a process handle to be cloneable.
struct ReadParts {
    stdout: tokio::process::ChildStdout,
    stderr: oneshot::Receiver<String>,
    inner: Arc<Inner>,
    stdin: mpsc::Sender<Write>,
    events: mpsc::Sender<StreamItem>,
}

/// Take a pipe that `Stdio::piped()` promised.
///
/// The child is killed on the way out rather than leaked: this is the one path
/// where a spawn half-succeeds, and a process nobody waits for is a process that
/// outlives the error reporting it.
fn take_pipe<T>(
    child: &mut Child,
    pipe: Option<T>,
    name: &'static str,
) -> Result<T, TransportError> {
    match pipe {
        Some(pipe) => Ok(pipe),
        None => {
            let _ = child.start_kill();
            Err(TransportError::Spawn(format!(
                "Claude Code {name} was not piped."
            )))
        }
    }
}

/// Forward everything queued for stdin, until stdin is closed.
async fn write_stdin_loop(
    mut stdin: tokio::process::ChildStdin,
    mut requests: mpsc::Receiver<Write>,
) {
    while let Some(request) = requests.recv().await {
        match request {
            Write::Message(bytes) => {
                if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
            // Dropping the handle is what the child sees as end of input, and
            // what lets a `-p` run exit once its turn has been answered.
            Write::Close => break,
        }
    }
}

/// Wait for the process, so its exit status is known.
///
/// Bounded, and a process still running after the grace period is killed:
/// stdout can close without the process exiting, and a reader waiting for one
/// that never leaves reports nothing at all. `None` when the transport has
/// already waited for the process itself, which is what `close` does: the status
/// was collected there and there is nothing left to take.
async fn reap(child: &Mutex<Option<Child>>) -> Option<i32> {
    let mut slot = child.lock().await;
    let mut process = slot.take()?;
    match tokio::time::timeout(SHUTDOWN_GRACE, process.wait()).await {
        Ok(Ok(status)) => status.code(),
        Ok(Err(error)) => {
            tracing::debug!(%error, "Claude Code could not be waited for");
            None
        }
        Err(_) => {
            let _ = process.start_kill();
            None
        }
    }
}

/// Standard base64 with padding.
///
/// Written out rather than depended on: `fx-agent` has no base64 dependency, and
/// adding one is a change to `Cargo.toml`, which belongs to whoever owns the
/// crate's dependencies. The vectors in the tests are RFC 4648's own.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let packed = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        encoded.push(char::from(ALPHABET[(packed >> 18) as usize & 0x3F]));
        encoded.push(char::from(ALPHABET[(packed >> 12) as usize & 0x3F]));
        encoded.push(if chunk.len() > 1 {
            char::from(ALPHABET[(packed >> 6) as usize & 0x3F])
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            char::from(ALPHABET[packed as usize & 0x3F])
        } else {
            '='
        });
    }
    encoded
}

/// Close stdin, once.
///
/// The `Close` is queued with the gate still locked, so no write can be told
/// it was accepted and then land behind it.
async fn close_stdin(input: &Mutex<TurnInput>, stdin: &mpsc::Sender<Write>) {
    let mut input = input.lock().await;
    if input.close() {
        let _ = stdin.send(Write::Close).await;
    }
}

/// Read stdout to the end, then report why the process is gone.
async fn read_stdout(parts: ReadParts) {
    let ReadParts {
        stdout,
        stderr,
        inner,
        stdin,
        events,
    } = parts;

    let mut reader = BufReader::new(stdout);
    let mut framer = LineFramer::with_limit(MAXIMUM_LINE_BYTES);
    let mut chunk = [0u8; 16 * 1024];
    let mut failure: Option<String> = None;

    'read: loop {
        let read = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) => {
                failure = Some(format!("Claude Code's output could not be read: {error}"));
                break 'read;
            }
        };

        // The framer owns the size limit, so an oversized line is refused while
        // it is still a partial buffer rather than after it has been read whole.
        let lines = match framer.push(&chunk[..read]) {
            Ok(lines) => lines,
            Err(error) => {
                tracing::warn!(%error, "a Claude stream line was refused");
                // The turn cannot continue: nothing reads Claude's output any
                // more, so its approvals could never be answered and the run
                // would hang rather than fail.
                failure = Some(format!(
                    "Claude emitted a stream event larger than FlowX's {} MB safety limit.",
                    MAXIMUM_LINE_BYTES / (1024 * 1024)
                ));
                break 'read;
            }
        };
        for line in lines {
            deliver_line(&line, &inner, &stdin, &events).await;
        }
    }

    if failure.is_none()
        && let Some(line) = framer.finish()
    {
        // A partial final line is still worth reading: a process killed
        // mid-write is otherwise completely silent.
        deliver_line(&line, &inner, &stdin, &events).await;
    }

    // Reaped before stderr is read, because the process's death is what closes
    // that pipe — and the kill on timeout is what guarantees it closes.
    let code = reap(&inner.child).await;
    let captured = match tokio::time::timeout(SHUTDOWN_GRACE, stderr).await {
        Ok(Ok(captured)) => captured,
        Ok(Err(_)) => {
            tracing::debug!("the stderr reader stopped before it reported");
            String::new()
        }
        Err(_) => {
            tracing::debug!("Claude Code's stderr did not close; reporting without it");
            String::new()
        }
    };
    let exit = match failure {
        // The framer's reason stands in for stderr because it is the actionable
        // one; what the process said is only context for it.
        Some(reason) => {
            tracing::debug!(%captured, "Claude Code said this while its stream was refused");
            Exit {
                stderr: reason,
                code,
            }
        }
        None => Exit {
            stderr: captured,
            code,
        },
    };
    *inner.exited.lock().await = Some(exit.clone());
    let _ = events.send(StreamItem::Exited(exit)).await;
}

/// Read one line into the stream.
async fn deliver_line(
    line: &str,
    inner: &Inner,
    stdin: &mpsc::Sender<Write>,
    events: &mpsc::Sender<StreamItem>,
) {
    if line.trim().is_empty() {
        return;
    }
    let (produced, refusal, finished) = {
        let mut parser = inner.parser.lock().await;
        let produced = parser.events(line);
        // A control request this parser cannot serve must be *answered*, not
        // merely recorded. Claude blocks on every one of them, so a refusal that
        // is produced but never written leaves the turn hanging forever with no
        // error and no way for the user to tell.
        let refusal = parser.take_refusal();
        // Completion is the parser's flag and nothing else. A background task
        // still running will be reported as a turn of its own, and one that
        // has ended is still owed an answer: the input is what lets those
        // turns be steered and their permissions answered, so it stays open
        // until a `result` leaves nothing outstanding. Closing is idempotent,
        // so every line after that may ask again.
        let finished = parser.did_finish() && !parser.holds_input_open();
        (produced, refusal, finished)
    };
    if let Some(envelope) = refusal {
        // Encoded inline rather than through the transport's own `write`, which
        // needs `&self`; the reader task only has `Inner`.
        if let Ok(mut bytes) = serde_json::to_vec(&envelope) {
            bytes.push(b'\n');
            let _ = stdin.send(Write::Message(bytes)).await;
        }
    }
    if finished {
        // Closing here is what lets the process exit: a `-p` run that has
        // answered still holds this pipe open until it does not. Closed before
        // the line's events go out, so a reader handed this `result` already
        // sees the input as it will stay.
        close_stdin(&inner.input, stdin).await;
    }
    for event in produced {
        // Awaited: the queue is bounded, so `send` is a future. Discarding it
        // compiles and delivers nothing.
        let _ = events.send(StreamItem::Event(event)).await;
    }
}

/// A trimmed, non-empty string, or `None`.
///
/// Blank settings are dropped rather than passed on: `--model ""` is a value
/// Claude rejects, and the caller meant to leave it out.
fn non_empty(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The pure half: the arguments, the payloads, and the rules about when stdin
/// may still be written. None of these need a process, which is what makes them
/// worth pinning here rather than through a child.
#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> TurnRequest {
        TurnRequest::default()
    }

    /// The flags that must be present whatever the turn is.
    fn required_flags() -> Vec<String> {
        vec![
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--permission-prompt-tool",
            "stdio",
            "--verbose",
            "--include-partial-messages",
        ]
        .into_iter()
        .map(String::from)
        .collect()
    }

    #[test]
    fn the_fixed_flags_are_the_streaming_json_protocol() {
        assert_eq!(request().arguments(), required_flags());
    }

    #[test]
    fn the_fixed_flags_come_first_and_in_order() {
        // A test that only checked for presence would pass with these reversed,
        // and Claude reads them in order.
        let arguments = request().arguments();
        let fixed = &arguments[..required_flags().len()];
        assert_eq!(fixed, &required_flags()[..]);
    }

    #[test]
    fn optional_arguments_appear_only_when_set() {
        let bare = request().arguments();
        assert_eq!(
            bare.len(),
            required_flags().len(),
            "an unset option was added"
        );

        let full = TurnRequest {
            model: Some("claude-opus-4-8".into()),
            effort: Some("high".into()),
            system_prompt: Some("be terse".into()),
            access: Some(AgentAccess::Supervised),
            resume_session_id: Some("session-9".into()),
            attachment_directory: Some("/tmp/attachments".into()),
        }
        .arguments();
        assert_eq!(&full[..bare.len()], &bare[..], "a default flag moved");
        assert!(
            contains_pair(&full, "--model", "claude-opus-4-8"),
            "{full:?}"
        );
        assert!(contains_pair(&full, "--effort", "high"), "{full:?}");
        assert!(
            contains_pair(&full, "--append-system-prompt", "be terse"),
            "{full:?}"
        );
        assert!(
            contains_pair(&full, "--permission-mode", "manual"),
            "{full:?}"
        );
        assert!(contains_pair(&full, "--resume", "session-9"), "{full:?}");
        assert!(
            contains_pair(&full, "--add-dir", "/tmp/attachments"),
            "{full:?}"
        );
    }

    #[test]
    fn effort_is_lower_cased_and_blank_settings_are_dropped() {
        // The user's settings file carries `High`; Claude's flag does not take it.
        let loud = TurnRequest {
            effort: Some(" HIGH ".into()),
            model: Some("   ".into()),
            ..request()
        };
        let arguments = loud.arguments();
        assert!(
            contains_pair(&arguments, "--effort", "high"),
            "{arguments:?}"
        );
        assert!(
            !arguments.iter().any(|argument| argument == "--model"),
            "a blank model was passed on: {arguments:?}"
        );
    }

    #[test]
    fn full_access_asks_for_permission_rather_than_skipping_it() {
        // The bug this avoids: with `--dangerously-skip-permissions` Claude
        // answers AskUserQuestion itself and never emits `can_use_tool`, so
        // FlowX cannot show the question at all.
        let arguments = TurnRequest {
            access: Some(AgentAccess::FullAccess),
            ..request()
        }
        .arguments();
        assert!(
            !arguments.iter().any(|a| a.contains("dangerously")),
            "Full Access skipped the permission channel: {arguments:?}"
        );
        assert!(contains_pair(&arguments, "--permission-mode", "manual"));
    }

    #[test]
    fn execution_permissions_follow_only_the_selected_access() {
        for (access, permission) in [
            (AgentAccess::Supervised, "manual"),
            (AgentAccess::AcceptEdits, "acceptEdits"),
            (AgentAccess::FullAccess, "manual"),
        ] {
            let arguments = TurnRequest {
                access: Some(access),
                ..request()
            }
            .arguments();
            assert!(
                contains_pair(&arguments, "--permission-mode", permission),
                "{arguments:?}"
            );
            assert!(!arguments.iter().any(|argument| argument == "plan"));
            assert!(
                !arguments
                    .iter()
                    .any(|argument| argument == "--append-system-prompt")
            );
        }
    }

    #[test]
    fn accept_edits_uses_claudes_own_spelling() {
        // `acceptedits` is not the flag; Claude rejects it at startup.
        let arguments = TurnRequest {
            access: Some(AgentAccess::AcceptEdits),
            ..request()
        }
        .arguments();
        assert!(contains_pair(
            &arguments,
            "--permission-mode",
            "acceptEdits"
        ));
    }

    #[test]
    fn no_access_mode_means_no_permission_flag_at_all() {
        let arguments = request().arguments();
        assert!(
            !arguments
                .iter()
                .any(|argument| argument == "--permission-mode"),
            "a permission mode was invented: {arguments:?}"
        );
    }

    #[test]
    fn the_standard_launch_carries_the_request_arguments() {
        let wanted = TurnRequest {
            model: Some("claude-sonnet-5".into()),
            ..request()
        };
        let launch = Launch::claude("/usr/local/bin/claude", &wanted);
        assert_eq!(launch.executable, "/usr/local/bin/claude");
        assert_eq!(launch.args, wanted.arguments());
        assert_eq!(launch.working_directory, None);
        assert!(launch.environment.is_empty());
    }

    #[test]
    fn a_user_message_is_shaped_the_way_claude_reads() {
        let mut input = TurnInput::new();
        let message = input
            .user_message("hello", &[] as &[&Path])
            .expect("a message");
        assert_eq!(message["type"], "user");
        assert_eq!(message["session_id"], "");
        assert_eq!(message["message"]["role"], "user");
        assert!(message["parent_tool_use_id"].is_null());
        assert_eq!(message["message"]["content"][0]["type"], "text");
        assert_eq!(message["message"]["content"][0]["text"], "hello");
        assert!(input.is_open(), "building a message closed the input");
    }

    #[test]
    fn a_blank_prompt_is_still_sent_as_text() {
        // Only a truly empty prompt is omitted. A prompt of spaces is what the
        // user typed, and dropping it silently would be worse than an empty turn.
        let mut input = TurnInput::new();
        let message = input
            .user_message("   ", &[] as &[&Path])
            .expect("a message");
        assert_eq!(message["message"]["content"][0]["text"], "   ");
    }

    #[test]
    fn there_is_nothing_to_send_without_a_prompt_or_an_image() {
        // The shape Claude rejects with a message that does not say FlowX built
        // it, so it is refused here instead.
        let mut input = TurnInput::new();
        let error = input
            .user_message("", &[] as &[&Path])
            .expect_err("nothing to send");
        assert!(matches!(error, TransportError::NothingToSend), "{error:?}");
        assert!(input.is_open(), "a refused message closed the input");
    }

    #[test]
    fn an_image_alone_is_a_complete_user_turn() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let png = dir.path().join("shot.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n").expect("wrote the image");
        let mut input = TurnInput::new();
        let message = input.user_message("", &[&png]).expect("a message");
        assert_eq!(
            message["message"]["content"].as_array().map(Vec::len),
            Some(1)
        );
        assert_eq!(message["message"]["content"][0]["type"], "image");
    }

    #[test]
    fn an_image_is_sent_as_a_base64_block() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let png = dir.path().join("shot.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n").expect("wrote the image");

        let mut input = TurnInput::new();
        let message = input.user_message("look", &[&png]).expect("a message");
        let blocks = message["message"]["content"].as_array().expect("blocks");
        // The prompt first, then the image: Claude reads them in order and the
        // question belongs to the text above it.
        assert_eq!(blocks[0]["type"], "text");
        let source = &blocks[1]["source"];
        assert_eq!(source["type"], "base64");
        assert_eq!(source["media_type"], "image/png");
        // RFC 4648's encoding of the PNG signature.
        assert_eq!(source["data"], "iVBORw0KGgo=");
    }

    #[test]
    fn every_image_format_claude_takes_is_recognised() {
        let dir = tempfile::tempdir().expect("a temp dir");
        for (name, media_type) in [
            ("a.jpg", "image/jpeg"),
            ("a.JPEG", "image/jpeg"),
            ("a.png", "image/png"),
            ("a.webp", "image/webp"),
            ("a.gif", "image/gif"),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, b"x").expect("wrote the image");
            let block = image_content_block(&path).expect("accepted");
            assert_eq!(block["source"]["media_type"], media_type, "{name}");
        }
    }

    #[test]
    fn an_image_format_claude_does_not_take_is_refused() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let tiff = dir.path().join("a.tiff");
        std::fs::write(&tiff, b"x").expect("wrote the image");
        let error = image_content_block(&tiff).expect_err("refused");
        match error {
            TransportError::UnsupportedImageFormat(format) => assert_eq!(format, "a.tiff"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_image_is_refused_rather_than_sent_as_nothing() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("empty.png");
        std::fs::write(&path, b"").expect("wrote the image");
        let error = image_content_block(&path).expect_err("refused");
        assert!(
            matches!(error, TransportError::ImageTooLarge { bytes: 0, .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_missing_image_is_reported_as_such() {
        let error = image_content_block(Path::new("/nonexistent/a.png")).expect_err("refused");
        assert!(matches!(error, TransportError::Io(_)), "{error:?}");
    }

    #[test]
    fn base64_matches_the_rfc_4648_vectors() {
        // Written out rather than depended on, so this is the whole of its
        // evidence that the padding and the alphabet are right.
        for (bytes, expected) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(bytes), expected, "{bytes:?}");
        }
        // Every alphabet symbol, which is where a transposed character shows.
        // 255 bytes is a whole number of three-byte blocks, so no padding.
        let all = base64(&(0u8..=254).collect::<Vec<u8>>());
        assert!(all.contains('+') && all.contains('/'), "{all}");
        assert!(!all.contains('='), "a full block needs no padding");
    }

    #[test]
    fn a_closed_input_refuses_a_prompt() {
        let mut input = TurnInput::new();
        assert!(input.close());
        let error = input
            .user_message("too late", &[] as &[&Path])
            .expect_err("refused");
        assert!(matches!(error, TransportError::InputClosed), "{error:?}");
    }

    #[test]
    fn a_closed_input_refuses_an_answer() {
        // The rule that matters: Claude has exited, so a refusal written now
        // would go into a pipe nobody reads and report nothing.
        let mut input = TurnInput::new();
        input
            .answer(json!({ "type": "control_response" }))
            .expect("open");
        input.close();
        let error = input
            .answer(json!({ "type": "control_response" }))
            .expect_err("refused");
        assert!(matches!(error, TransportError::InputClosed), "{error:?}");
    }

    #[test]
    fn closing_twice_is_harmless_and_ordered() {
        // The reader closes at `result` and a caller closing at the same moment
        // must not send a second close: the second one would be what a later
        // refusal was written through.
        let mut input = TurnInput::new();
        assert!(input.close(), "the first close did nothing");
        assert!(!input.close(), "the input was closed twice");
        assert!(!input.is_open());
    }

    #[test]
    fn an_exit_with_stderr_quotes_it() {
        let exit = Exit {
            stderr: "  Error: not logged in  ".into(),
            code: Some(1),
        };
        assert_eq!(exit.reason(), "Claude Code exited: Error: not logged in");
    }

    #[test]
    fn an_exit_with_only_a_status_reports_the_status() {
        let exit = Exit {
            stderr: String::new(),
            code: Some(7),
        };
        assert_eq!(exit.reason(), "Claude Code exited with status 7.");
    }

    #[test]
    fn a_silent_exit_still_explains_itself() {
        // A killed or OOM'd process leaves nothing to report, and saying nothing
        // would make the turn look like one that simply ran out of things to say.
        let exit = Exit {
            stderr: "  ".into(),
            code: None,
        };
        assert_eq!(exit.reason(), "Claude Code exited unexpectedly.");
    }

    #[test]
    fn a_result_beats_a_non_zero_exit_status() {
        // What the Swift original got wrong: it failed a turn whose answer had
        // already arrived, because the process exited non-zero afterwards.
        let end = TurnEnd::decide(
            Some("end_turn".into()),
            Some(&Exit {
                stderr: String::new(),
                code: Some(1),
            }),
        );
        assert_eq!(
            end,
            TurnEnd::Completed {
                stop_reason: "end_turn".into()
            }
        );
        assert!(end.is_completed());
        assert_eq!(end.into_result().expect("a stop reason"), "end_turn");
    }

    #[test]
    fn a_turn_that_never_reported_a_result_is_a_failure() {
        let exit = Exit {
            stderr: "claude: killed".into(),
            code: Some(9),
        };
        let end = TurnEnd::decide(None, Some(&exit));
        assert_eq!(
            end,
            TurnEnd::Vanished {
                reason: "Claude Code exited: claude: killed".into()
            }
        );
        assert!(!end.is_completed());
        assert!(
            matches!(end.into_result(), Err(TransportError::Vanished { .. })),
            "a vanished turn reported completion"
        );
    }

    #[test]
    fn a_turn_that_vanished_with_nothing_to_show_still_has_a_reason() {
        let end = TurnEnd::decide(None, None);
        assert_eq!(
            end,
            TurnEnd::Vanished {
                reason: "Claude Code ended before reporting a result.".into()
            }
        );
    }

    fn contains_pair(arguments: &[String], flag: &str, value: &str) -> bool {
        arguments
            .windows(2)
            .any(|pair| pair[0] == flag && pair[1] == value)
    }
}

/// Tests against a real child process.
///
/// These are the only tests that prove what actually reaches stdin, that the
/// process ends when FlowX closes its input, and what a caller sees when the
/// process dies mid-turn. Everything above is pure.
#[cfg(test)]
mod process {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::Value;

    use super::*;
    use crate::claude::fake_claude::FakeClaude;

    /// A transport against the fake server, launched the way a real turn is.
    ///
    /// The request's own arguments go *after* the script, so the fake can report
    /// the command line it was actually given.
    async fn start(server: &FakeClaude, request: &TurnRequest) -> Option<ClaudeTransport> {
        let mut launch = server.launch();
        launch.args.extend(request.arguments());
        ClaudeTransport::start(launch).await.ok()
    }

    /// A transport with the default request, or `None` to skip.
    async fn connect(server: &FakeClaude) -> Option<ClaudeTransport> {
        start(server, &TurnRequest::default()).await
    }

    /// The control request a turn is blocked on.
    enum Control {
        Approval(String),
        Question(String),
    }

    impl Control {
        fn id(&self) -> &str {
            match self {
                Self::Approval(id) | Self::Question(id) => id,
            }
        }
    }

    /// Read until the turn asks for something, which is the only way a caller
    /// learns there is an id to answer.
    async fn next_control(reader: &StreamReader) -> Control {
        loop {
            let item = tokio::time::timeout(Duration::from_secs(5), reader.next_event())
                .await
                .expect("the fake server asked in time")
                .expect("the stream was open");
            match item {
                StreamItem::Event(ProviderEvent::Approval(approval)) => {
                    return Control::Approval(approval.id);
                }
                StreamItem::Event(ProviderEvent::UserInput(request)) => {
                    return Control::Question(request.id);
                }
                StreamItem::Event(_) => {}
                other => panic!("expected a control request, got {other:?}"),
            }
        }
    }

    /// Every event the turn produced, up to and including its end.
    async fn run_to_end(reader: &StreamReader) -> Vec<ProviderEvent> {
        let mut events = Vec::new();
        loop {
            match reader.next_event().await {
                Some(StreamItem::Event(event)) => {
                    let done = matches!(event, ProviderEvent::Done { .. });
                    events.push(event);
                    if done {
                        return events;
                    }
                }
                Some(StreamItem::Exited(exit)) => {
                    panic!("the turn ended without a result: {}", exit.reason());
                }
                None => panic!("the stream closed before the turn ended"),
            }
        }
    }

    /// The refusals the server was sent, polled until `count` have arrived.
    async fn control_responses(server: &FakeClaude, count: usize) -> Vec<Value> {
        server
            .received_matching(count, Duration::from_secs(5), |message| {
                message["type"] == "control_response"
            })
            .await
    }

    #[tokio::test]
    async fn the_initial_prompt_reaches_the_process_intact() {
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let transport = connect(&server).await.expect("started");
        transport
            .send_prompt("hello there", &[] as &[&Path])
            .await
            .expect("written");

        let received = server.received_within(2, Duration::from_secs(5)).await;
        let message = received
            .iter()
            .find(|message| message["type"] == "user")
            .expect("the prompt arrived");
        assert_eq!(message["message"]["role"], "user");
        assert_eq!(message["message"]["content"][0]["text"], "hello there");
        assert_eq!(message["session_id"], "");
        assert!(message["parent_tool_use_id"].is_null());
    }

    #[tokio::test]
    async fn the_process_is_launched_with_the_streaming_flags() {
        // The flags are what make this stream-JSON at all, and a wrong one is a
        // CLI that starts and then says nothing useful.
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let request = TurnRequest {
            model: Some("claude-sonnet-5".into()),
            access: Some(AgentAccess::Supervised),
            ..TurnRequest::default()
        };
        let transport = start(&server, &request).await.expect("started");
        transport
            .send_prompt("hi", &[] as &[&Path])
            .await
            .expect("written");

        // argv[0] is the script; everything after it is what FlowX asked for.
        let argv = server.argv().await;
        assert!(
            argv.first()
                .is_some_and(|script| script.ends_with("run.py")),
            "argv did not start with the script: {argv:?}"
        );
        assert_eq!(&argv[1..], &request.arguments()[..], "{argv:?}");
        transport.close().await;
    }

    #[tokio::test]
    async fn the_launch_environment_reaches_the_process() {
        // The transport clears the environment, so anything a caller added has
        // to actually be set rather than merely merged.
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let mut launch = server.launch_with_env(&[("FLOWX_TEST", "42")]);
        launch.args.extend(TurnRequest::default().arguments());
        let transport = ClaudeTransport::start(launch).await.expect("started");
        transport
            .send_prompt("hi", &[] as &[&Path])
            .await
            .expect("written");

        let received = server.received_within(2, Duration::from_secs(5)).await;
        let started = received
            .iter()
            .find(|message| message["type"] == "__started__")
            .expect("the launch was recorded");
        assert_eq!(started["env"]["FLOWX_TEST"], "42");
        transport.close().await;
    }

    #[tokio::test]
    async fn only_one_reader_can_exist() {
        // The reader is exclusive by construction rather than by convention.
        // With two readers, whichever got there first takes the `Done` and the
        // other waits on a channel that will never produce another one — a turn
        // that hangs with no error.
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let mut transport = connect(&server).await.expect("started");
        let first = transport.reader().await;
        let second = transport.reader().await;

        transport
            .send_prompt("hello", &[] as &[&Path])
            .await
            .expect("written");

        // The second reader gets nothing, and knows it.
        let empty = tokio::time::timeout(Duration::from_millis(500), second.next_event()).await;
        assert!(
            empty.is_err() || empty.unwrap().is_none(),
            "a second reader consumed from the same channel"
        );
        drop(first);
    }

    #[tokio::test]
    async fn a_whole_turn_ends_on_the_result_frame() {
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let mut transport = connect(&server).await.expect("started");
        let reader = transport.reader().await;
        transport
            .send_prompt("hello", &[] as &[&Path])
            .await
            .expect("written");

        let events = run_to_end(&reader).await;
        // The init frame reports the turn as started; the transport does not
        // report it as well, or the spinner would appear twice.
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ProviderEvent::Lifecycle(_)))
                .count(),
            1,
            "{events:?}"
        );
        // Streamed once and assembled once, and the assembled copy dropped.
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ProviderEvent::TextDelta(_)))
                .count(),
            1,
            "{events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ProviderEvent::Text(_))),
            "the assembled message was not suppressed: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderEvent::Usage(_))),
            "{events:?}"
        );
        transport.close().await;
    }

    #[tokio::test]
    async fn an_errored_result_is_reported_as_an_error_and_not_as_completion() {
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let mut transport = connect(&server).await.expect("started");
        let reader = transport.reader().await;
        transport
            .send_prompt("!error", &[] as &[&Path])
            .await
            .expect("written");

        // Claude's own reason, not the exit status the process left behind
        // once it had given it.
        let end = tokio::time::timeout(Duration::from_secs(5), reader.wait_for_end())
            .await
            .expect("the stream ended");
        assert_eq!(
            end,
            TurnEnd::Failed {
                reason: "hit a limit".into()
            }
        );
        assert!(
            matches!(end.into_result(), Err(TransportError::Failed { reason }) if reason == "hit a limit")
        );
    }

    #[tokio::test]
    async fn stopping_a_turn_interrupts_claude_and_ends_the_tools_it_started() {
        // Stop used to close input and kill Claude alone, with no interrupt,
        // and Claude was not in a group of its own: the tool it was running
        // kept going after the turn ended.
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let mut transport = connect(&server).await.expect("started");
        let reader = transport.reader().await;
        transport
            .send_prompt("!spawn-and-hang", &[] as &[&Path])
            .await
            .expect("written");
        let pid = server.child_pid().await.expect("the tool started");
        let _reap = crate::process::testing::KillOnDrop(pid);
        tokio::time::timeout(Duration::from_secs(5), reader.next_event())
            .await
            .expect("the init frame arrived");

        transport.interrupt().await.expect("interrupt written");
        transport.terminate().await;

        let interrupt = server
            .received_matching(1, Duration::from_secs(5), |message| {
                message["type"] == "control_request"
            })
            .await;
        assert_eq!(
            interrupt[0]["request"]["subtype"], "interrupt",
            "{interrupt:?}"
        );
        assert!(
            crate::process::testing::gone_within(pid, Duration::from_secs(5)).await,
            "the tool Claude started outlived the stop"
        );
    }

    #[tokio::test]
    async fn stderr_that_is_not_text_does_not_stop_its_capture() {
        // A line reader stopped at the first invalid byte and closed the pipe,
        // so the CLI's next diagnostic — the one that says why — failed to
        // write at all.
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let mut transport = connect(&server).await.expect("started");
        let reader = transport.reader().await;
        transport
            .send_prompt("!noisy-death", &[] as &[&Path])
            .await
            .expect("written");
        let end = tokio::time::timeout(Duration::from_secs(5), reader.wait_for_end())
            .await
            .expect("the stream ended");
        match end {
            TurnEnd::Vanished { reason } => {
                assert!(reason.contains("the real reason"), "{reason}")
            }
            other => panic!("expected the process's own reason, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_write_accepted_while_input_closes_is_delivered_before_the_close() {
        // The gate used to be checked under its lock and the write queued after
        // the lock was released. A close in between queued its `Close` first,
        // the writer stopped there, and the write the caller had been told was
        // sent was discarded. The window is the encoding of a large message,
        // and the writer is held up behind a pipe the process is not reading,
        // so the `Close` waits in the queue instead of ending it at once.
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let transport = Arc::new(connect(&server).await.expect("started"));
        transport
            .send_prompt("!stall", &[] as &[&Path])
            .await
            .expect("written");
        transport
            .send_prompt(&"x".repeat(1024 * 1024), &[] as &[&Path])
            .await
            .expect("queued behind the full pipe");
        let writer = transport.clone();
        let late = tokio::spawn(async move {
            writer
                .send_prompt(
                    &format!("late {}", "y".repeat(4 * 1024 * 1024)),
                    &[] as &[&Path],
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(5)).await;
        transport.close_input().await;
        let accepted = late.await.unwrap().is_ok();

        server
            .exited_within(Duration::from_secs(20))
            .await
            .expect("the process saw its input close");
        let delivered = server.received().iter().any(|message| {
            message["message"]["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.starts_with("late "))
        });
        assert!(
            !accepted || delivered,
            "a write the transport accepted never reached the process"
        );
    }

    #[tokio::test]
    async fn an_image_reaches_the_process_as_one_line() {
        // A base64 image is a large payload, and a prompt split across writes
        // would be two half-lines.
        let dir = tempfile::tempdir().expect("a temp dir");
        let png = dir.path().join("shot.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n").expect("wrote the image");

        let Some(server) = FakeClaude::new() else {
            return;
        };
        let transport = connect(&server).await.expect("started");
        transport
            .send_prompt("look at this", &[&png])
            .await
            .expect("written");

        let received = server.received_within(2, Duration::from_secs(5)).await;
        let message = received
            .iter()
            .find(|message| message["type"] == "user")
            .expect("the prompt arrived");
        let blocks = message["message"]["content"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 2, "{message}");
        assert_eq!(blocks[1]["source"]["data"], "iVBORw0KGgo=");
        transport.close().await;
    }

    #[tokio::test]
    async fn an_approval_from_another_task_unblocks_the_turn() {
        // The reason the reader end is shared: the user answers from somewhere
        // else than the task reading the stream, and both use one transport.
        let Some(server) = FakeClaude::new() else {
            return;
        };
        // The reader is taken *before* the transport is shared, which is the
        // whole shape of the design: reading is exclusive, answering is not.
        let mut transport = connect(&server).await.expect("started");
        let reader = transport.reader().await;
        let transport = Arc::new(transport);
        transport
            .send_prompt("!ask-tool", &[] as &[&Path])
            .await
            .expect("written");

        let Control::Approval(id) = next_control(&reader).await else {
            panic!("expected an approval");
        };
        assert!(transport.pending_controls().await.contains(&id));

        let answering = {
            let transport = transport.clone();
            tokio::spawn(async move { transport.approve(&id, true).await })
        };
        answering
            .await
            .expect("the task did not panic")
            .expect("answered");

        // The fake only finishes the turn once the refusal is written, so this
        // completing at all is the proof that the answer reached it.
        let end = tokio::time::timeout(Duration::from_secs(5), reader.wait_for_end())
            .await
            .expect("the turn ended");
        assert!(end.is_completed(), "{end:?}");

        let responses = control_responses(&server, 1).await;
        assert_eq!(responses.len(), 1, "{responses:?}");
        assert_eq!(responses[0]["response"]["request_id"], "req-1");
        assert_eq!(responses[0]["response"]["response"]["behavior"], "allow");
        assert!(transport.pending_controls().await.is_empty());
    }

    #[tokio::test]
    async fn a_denied_approval_still_unblocks_the_turn() {
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let mut transport = connect(&server).await.expect("started");
        let reader = transport.reader().await;
        transport
            .send_prompt("!ask-tool", &[] as &[&Path])
            .await
            .expect("written");

        let Control::Approval(id) = next_control(&reader).await else {
            panic!("expected an approval");
        };
        transport.approve(&id, false).await.expect("denied");

        let responses = control_responses(&server, 1).await;
        assert_eq!(responses[0]["response"]["response"]["behavior"], "deny");
        // And the turn ends rather than hanging on the refusal.
        let end = tokio::time::timeout(Duration::from_secs(5), reader.wait_for_end())
            .await
            .expect("the turn ended");
        assert!(end.is_completed(), "{end:?}");
    }

    #[tokio::test]
    async fn answering_a_question_echoes_claudes_own_text() {
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let mut transport = connect(&server).await.expect("started");
        let reader = transport.reader().await;
        transport
            .send_prompt("!ask-question", &[] as &[&Path])
            .await
            .expect("written");

        let Control::Question(id) = next_control(&reader).await else {
            panic!("expected a question");
        };
        transport
            .answer_questions(&id, &[("question-1".into(), "Postgres".into())])
            .await
            .expect("answered");

        let responses = control_responses(&server, 1).await;
        let input = &responses[0]["response"]["response"]["updatedInput"];
        let answers = input["answers"].as_array().expect("answers");
        assert_eq!(answers[0]["question"], "Which database?");
        assert_eq!(answers[0]["answer"], "Postgres");
        let end = tokio::time::timeout(Duration::from_secs(5), reader.wait_for_end())
            .await
            .expect("the turn ended");
        assert!(end.is_completed(), "{end:?}");
    }

    #[tokio::test]
    async fn declining_a_question_refuses_it() {
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let mut transport = connect(&server).await.expect("started");
        let reader = transport.reader().await;
        transport
            .send_prompt("!ask-question", &[] as &[&Path])
            .await
            .expect("written");

        let Control::Question(id) = next_control(&reader).await else {
            panic!("expected a question");
        };
        transport.decline_questions(&id).await.expect("declined");

        let responses = control_responses(&server, 1).await;
        assert_eq!(responses[0]["response"]["response"]["behavior"], "deny");
    }

    #[tokio::test]
    async fn answering_a_question_as_an_approval_is_reported() {
        // The parser drops the entry before it checks the kind, so this would
        // otherwise consume the question without answering it.
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let mut transport = connect(&server).await.expect("started");
        let reader = transport.reader().await;
        transport
            .send_prompt("!ask-question", &[] as &[&Path])
            .await
            .expect("written");

        let Control::Question(id) = next_control(&reader).await else {
            panic!("expected a question");
        };
        let error = transport
            .approve(&id, true)
            .await
            .expect_err("answered the wrong way");
        assert!(
            matches!(error, TransportError::NotAControl { .. }),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn an_answer_to_an_id_that_was_never_open_is_reported() {
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let transport = connect(&server).await.expect("started");
        let error = transport
            .approve("claude-approval-99", true)
            .await
            .expect_err("nothing to answer");
        assert!(
            matches!(error, TransportError::UnknownControl { .. }),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn cancelling_refuses_the_open_requests_before_closing_input() {
        // The order is the whole point: Claude blocks on every control request,
        // so closing its input first would leave two replies that can never be
        // delivered.
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let mut transport = connect(&server).await.expect("started");
        let reader = transport.reader().await;
        transport
            .send_prompt("!ask-two", &[] as &[&Path])
            .await
            .expect("written");

        let first = next_control(&reader).await;
        let second = next_control(&reader).await;
        assert_eq!(transport.pending_controls().await.len(), 2);
        assert_ne!(first.id(), second.id(), "two requests shared an id");

        transport.approve(first.id(), false).await.expect("denied");
        transport.approve(second.id(), false).await.expect("denied");
        transport.close().await;

        let responses = control_responses(&server, 2).await;
        assert_eq!(responses.len(), 2, "{responses:?}");
        for response in &responses {
            assert_eq!(response["response"]["response"]["behavior"], "deny");
        }
        // Closing input is what let the fake finish, which is the proof the
        // refusals were written first.
        assert!(
            server.exited_within(Duration::from_secs(5)).await.is_some(),
            "the process did not end when its input closed"
        );
    }

    #[tokio::test]
    async fn nothing_is_writable_once_the_turn_has_ended() {
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let mut transport = connect(&server).await.expect("started");
        let reader = transport.reader().await;
        transport
            .send_prompt("hello", &[] as &[&Path])
            .await
            .expect("written");
        run_to_end(&reader).await;

        // Claude has answered and exited; a steer written now would vanish into
        // a closed pipe with nothing to report it.
        let error = transport
            .send_prompt("one more thing", &[] as &[&Path])
            .await
            .expect_err("refused");
        assert!(matches!(error, TransportError::InputClosed), "{error:?}");
    }

    #[tokio::test]
    async fn the_process_exits_once_the_turn_ends() {
        // A `-p` run holds its input pipe open, so the transport closing stdin at
        // `result` is what lets it terminate. Without this the turn hangs after
        // its own answer.
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let mut transport = connect(&server).await.expect("started");
        let reader = transport.reader().await;
        transport
            .send_prompt("hello", &[] as &[&Path])
            .await
            .expect("written");
        let end = tokio::time::timeout(Duration::from_secs(5), reader.wait_for_end())
            .await
            .expect("the turn ended");
        assert!(end.is_completed(), "{end:?}");

        assert!(
            server.exited_within(Duration::from_secs(5)).await.is_some(),
            "the process outlived a completed turn"
        );
        // Waited for rather than sampled: the child records its own exit before
        // its stdout closes, so reading the log is not evidence that the reader
        // has finished with it.
        let exit = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(StreamItem::Exited(exit)) = reader.next_event().await {
                    break exit;
                }
            }
        })
        .await
        .expect("the exit was reported");
        assert_eq!(exit.code, Some(0), "{}", exit.reason());
        // Recorded before the item was sent, so a caller that has seen the exit
        // can always ask for it again.
        assert_eq!(transport.exit_reason().await, Some(exit));
    }

    #[tokio::test]
    async fn a_result_while_a_background_task_runs_keeps_the_process_for_its_follow_up() {
        // Claude reports a background task's end as a turn of its own. stdin
        // used to close at the first `result`, and the session then stopped
        // the process: the task, its notification and the turn that answered
        // it were all lost, and nothing said anything was still running.
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let mut transport = connect(&server).await.expect("started");
        let reader = transport.reader().await;
        transport
            .send_prompt("!background", &[] as &[&Path])
            .await
            .expect("written");

        let first = tokio::time::timeout(Duration::from_secs(5), run_to_end(&reader))
            .await
            .expect("the first result arrived");
        assert!(
            first.iter().any(|event| matches!(event,
                ProviderEvent::Task(task) if task.id == "task-1" && task.status.is_running())),
            "{first:?}"
        );
        assert!(
            transport.inner.input.lock().await.is_open(),
            "the input closed while a task was still running"
        );
        assert!(
            server
                .exited_within(Duration::from_millis(300))
                .await
                .is_none(),
            "the process ended with a task still running"
        );

        // The input is live, so the wait can still be steered.
        transport
            .send_prompt("while you wait", &[] as &[&Path])
            .await
            .expect("a steer is written while the task runs");
        let steered = tokio::time::timeout(Duration::from_secs(5), run_to_end(&reader))
            .await
            .expect("the steer was answered");
        assert!(
            steered.contains(&ProviderEvent::TextDelta("while you wait".into())),
            "{steered:?}"
        );
        assert!(transport.inner.input.lock().await.is_open());

        server.release_background();
        let follow_up = tokio::time::timeout(Duration::from_secs(5), run_to_end(&reader))
            .await
            .expect("the follow-up turn arrived");
        assert!(
            follow_up.iter().any(|event| matches!(event,
                ProviderEvent::Task(task) if task.status == crate::provider::TaskStatus::Completed
                    && task.summary.as_deref().is_some_and(|summary| summary.contains("completed")))),
            "{follow_up:?}"
        );
        assert!(
            follow_up.contains(&ProviderEvent::TextDelta("finished".into())),
            "{follow_up:?}"
        );
        assert!(
            follow_up.iter().any(
                |event| matches!(event, ProviderEvent::Usage(usage) if usage.input_tokens == 7)
            ),
            "the follow-up's usage was dropped: {follow_up:?}"
        );
        // Nothing is running now, so the input closes and Claude exits.
        assert!(
            server.exited_within(Duration::from_secs(5)).await.is_some(),
            "the process outlived its last task's follow-up"
        );
    }

    #[tokio::test]
    async fn a_quiet_process_is_not_taken_for_a_finished_turn() {
        // `init` and then nothing. Completion is the `result` frame and nothing
        // else, so this must not report a finished turn.
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let mut transport = connect(&server).await.expect("started");
        let reader = transport.reader().await;
        transport
            .send_prompt("!hang", &[] as &[&Path])
            .await
            .expect("written");

        let item = tokio::time::timeout(Duration::from_secs(5), reader.next_event())
            .await
            .expect("the init frame arrived")
            .expect("the stream was open");
        assert!(matches!(item, StreamItem::Event(_)), "{item:?}");

        let still_running = tokio::time::timeout(Duration::from_millis(500), reader.wait_for_end())
            .await
            .is_err();
        assert!(still_running, "a quiet process ended the turn");
        transport.close().await;
    }

    #[tokio::test]
    async fn claude_dying_mid_turn_reports_its_stderr() {
        // The case that decides whether a turn says why or vanishes.
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let mut transport = connect(&server).await.expect("started");
        let reader = transport.reader().await;
        transport
            .send_prompt("!die", &[] as &[&Path])
            .await
            .expect("written");

        let end = tokio::time::timeout(Duration::from_secs(5), reader.wait_for_end())
            .await
            .expect("the stream ended");
        assert_eq!(
            end,
            TurnEnd::Vanished {
                reason: "Claude Code exited: fake claude died mid-turn".into()
            }
        );
        assert!(matches!(
            end.into_result(),
            Err(TransportError::Vanished { .. })
        ));
    }

    #[tokio::test]
    async fn a_silent_death_still_explains_itself() {
        // An OOM kill leaves stderr empty. Saying nothing would look like a turn
        // that completed.
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let mut transport = connect(&server).await.expect("started");
        let reader = transport.reader().await;
        transport
            .send_prompt("!die-quietly", &[] as &[&Path])
            .await
            .expect("written");

        let end = tokio::time::timeout(Duration::from_secs(5), reader.wait_for_end())
            .await
            .expect("the stream ended");
        assert_eq!(
            end,
            TurnEnd::Vanished {
                reason: "Claude Code exited with status 7.".into()
            }
        );
    }

    #[tokio::test]
    async fn the_stream_ends_with_exactly_one_exit() {
        // A dead process must report itself once and then let the stream close,
        // so a caller that reads to the end learns it rather than blocking.
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let mut transport = connect(&server).await.expect("started");
        let reader = transport.reader().await;
        transport
            .send_prompt("!die", &[] as &[&Path])
            .await
            .expect("written");

        let mut exits = 0;
        let drained = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(item) = reader.next_event().await {
                if matches!(item, StreamItem::Exited(_)) {
                    exits += 1;
                }
            }
        })
        .await;
        assert!(drained.is_ok(), "the stream never ended");
        assert_eq!(exits, 1, "the process was reported {exits} times");
    }

    #[tokio::test]
    async fn an_endless_line_is_refused_rather_than_buffered() {
        // The safety limit, exercised for real, and specifically Claude's own:
        // the fake writes more than FlowX's Claude limit and less than Codex's,
        // so passing here means the right number is in force.
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let mut transport = connect(&server).await.expect("started");
        let reader = transport.reader().await;
        transport
            .send_prompt("!ramble", &[] as &[&Path])
            .await
            .expect("written");

        let end = tokio::time::timeout(Duration::from_secs(30), reader.wait_for_end())
            .await
            .expect("the reader gave up rather than buffering");
        match end {
            TurnEnd::Vanished { reason } => {
                assert!(reason.contains("32 MB safety limit"), "{reason}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_missing_executable_reports_where_it_looked() {
        let error = ClaudeTransport::start(Launch {
            executable: "/nonexistent/claude".into(),
            args: vec!["-p".into()],
            working_directory: None,
            environment: Vec::new(),
        })
        .await
        .expect_err("nothing to launch");
        match error {
            TransportError::Spawn(message) => {
                assert!(message.contains("/nonexistent/claude"), "{message}")
            }
            other => panic!("expected a spawn failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn closing_releases_the_process() {
        let Some(server) = FakeClaude::new() else {
            return;
        };
        let transport = connect(&server).await.expect("started");
        transport.close().await;
        // After close, writing reports rather than hanging on a dead pipe.
        let error = transport
            .send_prompt("anything", &[] as &[&Path])
            .await
            .expect_err("the transport is closed");
        assert!(matches!(error, TransportError::InputClosed), "{error:?}");
    }
}
