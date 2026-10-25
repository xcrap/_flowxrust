//! The Codex transport: a `codex app-server` process and its JSON-RPC stream.
//!
//! The Swift original ran this as a 4,000-line actor holding the process, the
//! pending-response map, the interrupt watchdog, and every protocol method in
//! one type. It is split here into two halves that can be tested independently:
//!
//! - [`PendingRequests`] is pure state — who is waiting on what, and how a
//!   failure is broadcast. No IO, so every failure path is reachable in a test.
//! - [`Transport`] owns the process and the two reader tasks.
//!
//! The ordering rules below are the ones that actually cost time in the original
//! and are worth stating outright:
//!
//! - `initialize` uses the reserved id 0; dynamic ids start at 10, matching the
//!   server's own convention that low ids are handshake traffic.
//! - `initialized` is sent only *after* `initialize` succeeds. Sending it earlier
//!   is the classic handshake race: the server may answer a control request
//!   before it has finished setting up.
//! - stderr is captured, not inherited. A server that fails to start says why on
//!   stderr, and an inherited pipe would send that to the FlowX log where nobody
//!   looks when a window fails to open.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::codex::framing::LineFramer;
use crate::codex::protocol::{
    Classified, FIRST_DYNAMIC_ID, INITIALIZE_ID, IncomingError, Outgoing, parse_line,
};

/// How long a control request may take before FlowX gives up on it.
///
/// The same value the Swift original used. Long enough for a cold model list on a
/// busy machine, short enough that a wedged server does not leave the composer
/// disabled indefinitely.
pub const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Events held before the reader blocks.
///
/// Bounded, because a `CodexSession` outlives its turns and nothing drains the
/// channel between them: an unbounded queue grows for as long as a thread sits
/// idle, which is exactly when nobody is watching memory. When it fills, the
/// reader stops reading, the server's pipe fills, and it waits — a truthful
/// signal that FlowX is not keeping up, rather than silent unbounded growth.
pub const EVENT_QUEUE_DEPTH: usize = 1024;

/// Stderr kept for diagnostics.
///
/// Bounded because a misbehaving server can write to stderr forever, and this is
/// only ever read when reporting a failure.
pub const MAX_STDERR_BYTES: usize = 64 * 1024;

/// How long the watcher waits for stderr to close once stdout has.
///
/// A process that exits closes both at once, so this is normally no wait at
/// all; the bound only matters when a child it spawned still holds stderr.
const STDERR_DRAIN: Duration = Duration::from_millis(250);

/// How long a stopping server, and then its process group, gets to exit on
/// SIGTERM before it is killed.
pub const TERMINATE_GRACE: Duration = Duration::from_millis(500);

/// Remember tool groups before a crashed server's children become orphans.
const TREE_REFRESH: Duration = Duration::from_millis(500);

/// Why a request did not produce a result.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("the Codex app-server exited{stderr}")]
    Exited { stderr: String },
    #[error("Codex did not answer `{method}` within {} seconds", CONTROL_REQUEST_TIMEOUT.as_secs())]
    TimedOut { method: String },
    #[error("Codex rejected `{method}`: {message} (code {code})")]
    Rejected {
        method: String,
        code: i64,
        message: String,
    },
    #[error("Codex answered `{method}` with a malformed response: {reason}")]
    Malformed { method: String, reason: String },
    #[error("the Codex app-server could not be started: {0}")]
    Spawn(String),
    #[error("the Codex app-server could not be written to: {0}")]
    Write(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Protocol(#[from] crate::codex::protocol::ProtocolError),
    #[error("could not encode a message for Codex: {0}")]
    Encode(#[from] serde_json::Error),
}

/// The waiting half of the transport.
///
/// Pure state, deliberately: correlating responses is where a subtle bug turns
/// into a hung turn, and a hung turn is not reachable from a test that has to
/// spawn a real server.
#[derive(Debug, Default)]
pub struct PendingRequests {
    next_id: i64,
    waiting: std::collections::HashMap<i64, Waiter>,
    /// Set once the process is gone.
    ///
    /// Held here rather than behind a second lock so that checking it and
    /// registering a waiter are one atomic step. Checked under one lock and
    /// registered under another, a request could slip in between the two and
    /// then sit for the full control timeout against a dead process.
    exit: Option<String>,
}

#[derive(Debug)]
struct Waiter {
    method: String,
    sender: oneshot::Sender<Outcome>,
}

/// How a waiting request finished.
///
/// The distinction matters to the caller: "Codex said no" and "Codex died" call
/// for different messages and different recovery. Funnelling both through one
/// error type is what made a dead server look like a rejected method.
#[derive(Debug)]
pub enum Outcome {
    /// The server answered.
    Answered(Result<Value, IncomingError>),
    /// The process went away with this request outstanding.
    Dead(String),
}

impl PendingRequests {
    pub fn new() -> Self {
        Self {
            // The server treats ids below 10 as handshake traffic.
            next_id: FIRST_DYNAMIC_ID,
            waiting: std::collections::HashMap::new(),
            exit: None,
        }
    }

    /// Take a reserved id, for the handshake.
    ///
    /// `register` allocates from the dynamic range, so the handshake cannot use
    /// it: `initialize` has to carry id 0 exactly.
    fn adopt(&mut self, id: i64, method: String, sender: oneshot::Sender<Outcome>) {
        self.waiting.insert(id, Waiter { method, sender });
    }

    /// Reserve an id and a slot to be filled by its response.
    pub fn register(&mut self, method: &str) -> (i64, oneshot::Receiver<Outcome>) {
        let id = Outgoing::next_id(&mut self.next_id);
        let (sender, receiver) = oneshot::channel();
        self.waiting.insert(
            id,
            Waiter {
                method: method.to_string(),
                sender,
            },
        );
        (id, receiver)
    }

    /// Deliver a response.
    ///
    /// Returns `false` for an id nobody is waiting on, which is not an error: a
    /// response can arrive after its request timed out, and dropping it is
    /// correct.
    pub fn resolve(&mut self, id: &Value, payload: Result<Value, IncomingError>) -> bool {
        let Some(id) = id.as_i64() else {
            return false;
        };
        self.waiting
            .remove(&id)
            .is_some_and(|waiter| waiter.sender.send(Outcome::Answered(payload)).is_ok())
    }

    /// Which method a caller is still waiting on, if any.
    pub fn waiting_for(&self, id: &Value) -> Option<&str> {
        let id = id.as_i64()?;
        self.waiting.get(&id).map(|waiter| waiter.method.as_str())
    }

    /// Give up on one request, because it timed out.
    pub fn abandon(&mut self, id: &Value) -> bool {
        match id.as_i64() {
            Some(id) => self.waiting.remove(&id).is_some(),
            None => false,
        }
    }

    /// How many callers are waiting.
    pub fn len(&self) -> usize {
        self.waiting.len()
    }

    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }

    /// Register a request, or report why one cannot be started.
    ///
    /// One critical section, deliberately: `register_or_dead` exists so the
    /// "is the process alive" check and the "record who is waiting" step cannot
    /// be separated by the watcher's `fail_all`.
    pub fn register_or_dead(
        &mut self,
        method: &str,
    ) -> Result<(i64, oneshot::Receiver<Outcome>), String> {
        if let Some(reason) = &self.exit {
            return Err(reason.clone());
        }
        Ok(self.register(method))
    }

    /// Record that the process has gone, and fail everyone waiting.
    ///
    /// Later registrations are refused by [`Self::register_or_dead`], so a
    /// request that arrives after the drain fails immediately instead of waiting
    /// out the timeout on a pipe nobody is reading.
    pub fn fail_all(&mut self, reason: &str) -> Vec<String> {
        self.exit = Some(reason.to_string());
        let methods: Vec<String> = self.waiting.values().map(|w| w.method.clone()).collect();
        for waiter in self.waiting.drain().map(|(_, waiter)| waiter) {
            let _ = waiter.sender.send(Outcome::Dead(reason.to_string()));
        }
        methods
    }

    /// The recorded exit reason, if the process is gone.
    pub fn exit_reason(&self) -> Option<&str> {
        self.exit.as_deref()
    }
}

/// How to launch a server.
#[derive(Debug, Clone)]
pub struct Launch {
    pub executable: String,
    pub args: Vec<String>,
    pub working_directory: Option<String>,
    pub environment: Vec<(String, String)>,
}

impl Launch {
    /// The standard invocation: `codex app-server`.
    pub fn codex(executable: impl Into<String>) -> Self {
        Self {
            executable: executable.into(),
            args: vec!["app-server".into()],
            working_directory: None,
            environment: Vec::new(),
        }
    }

    pub fn in_directory(mut self, directory: impl Into<String>) -> Self {
        self.working_directory = Some(directory.into());
        self
    }
}

/// Something the server sent that FlowX did not ask for.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerEvent {
    /// A one-way notification.
    Notification { method: String, params: Value },
    /// A request FlowX must answer.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
}

/// Why the transport stopped.
#[derive(Debug, Clone, PartialEq)]
pub struct Exit {
    pub stderr: String,
    pub code: Option<i32>,
}

impl Exit {
    /// The reason a failure is reported to the user.
    ///
    /// A killed or OOM'd server leaves stderr empty, so the fallback is not
    /// optional: without it the turn would look like it completed normally.
    pub fn reason(&self) -> String {
        let stderr = self.stderr.trim();
        if stderr.is_empty() {
            "Codex app-server exited unexpectedly.".to_string()
        } else {
            format!("Codex app-server exited: {stderr}")
        }
    }
}

/// A request that has been written and whose answer has not been awaited.
///
/// Split from [`Transport::request`] so a caller can stop waiting without
/// losing the answer. A turn stopped between writing `turn/start` and reading
/// its reply still has to learn whether Codex took the prompt, and which turn
/// to interrupt.
#[derive(Debug)]
pub struct PendingResponse {
    id: i64,
    method: String,
    receiver: oneshot::Receiver<Outcome>,
}

/// The live connection to one `codex app-server`.
#[derive(Debug)]
pub struct Transport {
    child: Mutex<Option<Child>>,
    /// The server's place in the list Quit ends, until it closes.
    running: std::sync::Mutex<Option<crate::process::Running>>,
    stdin: mpsc::UnboundedSender<Vec<u8>>,
    /// Closed when the reader task finishes, for both EOF and a hard failure.
    events: mpsc::Receiver<ServerEvent>,
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    /// The server's descendants and their groups, remembered while it runs so
    /// a failed turn still ends tools already adopted by launchd.
    tree: Option<crate::process::Watched>,
    pending: Mutex<PendingRequests>,
    stderr: Mutex<String>,
    exited: Mutex<Option<Exit>>,
}

/// Why the transport cannot be used any more.
#[derive(Debug, thiserror::Error)]
#[error("the Codex app-server is no longer running: {0}")]
pub struct Closed(pub String);

impl Transport {
    /// Start a server and complete the handshake.
    pub async fn start(
        launch: Launch,
        client_name: &str,
        client_version: &str,
    ) -> Result<Self, TransportError> {
        let mut command = Command::new(&launch.executable);
        command
            .args(&launch.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Captured rather than inherited: a server that cannot start says
            // why here, and that is the only useful diagnostic for a window
            // that failed to open.
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

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let mut stdin = child.stdin.take().expect("stdin was piped");

        let tree = group.map(crate::process::Watched::new);
        let running = group
            .zip(tree.clone())
            .map(|(group, tree)| crate::process::Running::watching(group, tree));
        let shared = Arc::new(Shared {
            tree,
            pending: Mutex::new(PendingRequests::new()),
            stderr: Mutex::new(String::new()),
            exited: Mutex::new(None),
        });
        watch_tree(&shared);
        let (events_tx, events) = mpsc::channel(EVENT_QUEUE_DEPTH);
        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        let writer_shared = shared.clone();
        tokio::spawn(async move {
            let mut rx = stdin_rx;
            while let Some(bytes) = rx.recv().await {
                if stdin.write_all(&bytes).await.is_err() {
                    break;
                }
                if stdin.flush().await.is_err() {
                    break;
                }
            }
            let _ = writer_shared;
            drop(stdin);
        });

        let reader_shared = shared.clone();
        let reader_events = events_tx.clone();
        let reader = tokio::spawn(async move {
            read_stdout(stdout, reader_shared, reader_events).await;
        });

        let stderr_shared = shared.clone();
        let mut stderr_reader = tokio::spawn(async move {
            read_stderr(stderr, stderr_shared).await;
        });

        // The exit path: when stdout closes, the failure is broadcast. Started
        // before the handshake, not after it — a server that dies answering
        // `initialize` (a bad config, an old CLI without `app-server`) would
        // otherwise leave the handshake waiting out the full control timeout,
        // and its stderr, which says why, would never be shown.
        let watcher_shared = shared.clone();
        let events_tx = events_tx.clone();
        tokio::spawn(async move {
            let _ = reader.await;
            // The reason a server died is usually the last thing it wrote to
            // stderr, which can still be in flight when stdout closes.
            let _ = tokio::time::timeout(STDERR_DRAIN, &mut stderr_reader).await;
            stderr_reader.abort();
            record_exit(&watcher_shared, String::new()).await;
            let exit = watcher_shared
                .exited
                .lock()
                .await
                .clone()
                .expect("recorded above");
            watcher_shared.pending.lock().await.fail_all(&exit.reason());
            // Awaited: the queue is bounded, so `send` is a future.
            let _ = events_tx
                .send(ServerEvent::Notification {
                    method: "flowx/server-exited".into(),
                    params: json!({ "stderr": exit.stderr, "code": exit.code }),
                })
                .await;
        });

        let transport = Self {
            child: Mutex::new(Some(child)),
            running: std::sync::Mutex::new(running),
            stdin: stdin_tx,
            events,
            shared,
        };

        // The handshake, in the only order that is safe.
        transport
            .handshake(client_name, client_version, &launch.executable)
            .await?;

        Ok(transport)
    }

    async fn handshake(
        &self,
        client_name: &str,
        client_version: &str,
        executable: &str,
    ) -> Result<(), TransportError> {
        // `initialize` is sent with the reserved id, not through the dynamic
        // allocator, so it can never collide with a real request.
        let initialized = match self
            .await_response(
                Outgoing::initialize(client_name, client_version),
                "initialize",
                INITIALIZE_ID,
            )
            .await
        {
            // A death with nothing on stderr has no reason to show; naming the
            // binary is the most useful thing left to say.
            Err(TransportError::Exited { .. })
                if self
                    .exit_reason()
                    .await
                    .is_some_and(|exit| exit.stderr.trim().is_empty()) =>
            {
                return Err(TransportError::Spawn(format!(
                    "{executable} exited before completing initialization"
                )));
            }
            other => other?,
        };

        let _ = initialized;
        // Only now. Sending `initialized` earlier is a race the server loses.
        self.notify(Outgoing::initialized())?;
        Ok(())
    }

    /// Send a request and wait for its response.
    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, TransportError> {
        let mut pending = self.send_request(method, params).await?;
        self.response(&mut pending).await
    }

    /// Write a request without waiting for its answer.
    ///
    /// Once this returns, the server has the request: whatever happens to the
    /// caller, the answer is still waiting in the returned slot.
    pub async fn send_request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<PendingResponse, TransportError> {
        // One critical section: whether the process is alive, and who is waiting,
        // cannot be two separate steps or a request can slip in after the
        // watcher drained the map and then wait out the full control timeout with
        // the composer disabled.
        let (id, receiver) = {
            let mut pending = self.shared.pending.lock().await;
            pending
                .register_or_dead(method)
                .map_err(|reason| TransportError::Exited { stderr: reason })?
        };
        let message = Outgoing::Request {
            id,
            method: method.to_string(),
            params,
        };
        if self.send(message).is_err() {
            self.shared.pending.lock().await.abandon(&json!(id));
            return Err(TransportError::Write(
                "the app-server stdin is closed".into(),
            ));
        }
        Ok(PendingResponse {
            id,
            method: method.to_string(),
            receiver,
        })
    }

    /// Wait for the answer to a request [`Self::send_request`] wrote.
    ///
    /// Cancel-safe: dropping this future leaves the answer in `pending`, so a
    /// later call still receives it.
    pub async fn response(&self, pending: &mut PendingResponse) -> Result<Value, TransportError> {
        let method = pending.method.clone();
        self.await_outcome(&mut pending.receiver, &method, Some(json!(pending.id)))
            .await
    }

    /// [`Self::response`] for a caller that is no longer reading the stream:
    /// what arrives meanwhile is read and dropped.
    ///
    /// The event queue is bounded, so with nobody draining it the reader stops
    /// at the next notification it cannot queue, and an answer behind that is
    /// never read. Stop drops the turn and then waits on `turn/interrupt`;
    /// while Codex was still reporting, that wait ran to its timeout.
    pub async fn response_discarding_events(
        &mut self,
        pending: &mut PendingResponse,
    ) -> Result<Value, TransportError> {
        let method = pending.method.clone();
        let answer = await_outcome_in(
            &self.shared,
            &mut pending.receiver,
            &method,
            Some(json!(pending.id)),
        );
        tokio::pin!(answer);
        let mut open = true;
        loop {
            tokio::select! {
                result = &mut answer => return result,
                event = self.events.recv(), if open => open = event.is_some(),
            }
        }
    }

    /// Send a request that carries a reserved id, used only by the handshake.
    async fn await_response(
        &self,
        message: Outgoing,
        method: &str,
        id: i64,
    ) -> Result<Value, TransportError> {
        let (sender, receiver) = oneshot::channel();
        {
            // One critical section, as in `request`: a server that dies at once
            // can be drained by the watcher before the handshake registers.
            let mut pending = self.shared.pending.lock().await;
            if let Some(reason) = pending.exit_reason() {
                return Err(TransportError::Exited {
                    stderr: reason.to_string(),
                });
            }
            pending.adopt(id, method.to_string(), sender);
        }
        if self.send(message).is_err() {
            return Err(TransportError::Write(
                "the app-server stdin is closed".into(),
            ));
        }
        // The id it was actually given, not the one this call happens to use
        // today: the timeout path abandons by id, and a mismatch would leave
        // the real slot in the map for the life of the transport.
        self.await_outcome(receiver, method, Some(json!(id))).await
    }

    /// Wait for one registered request, turning the three ways it can finish
    /// into three errors.
    async fn await_outcome(
        &self,
        receiver: impl std::future::Future<Output = Result<Outcome, oneshot::error::RecvError>>,
        method: &str,
        abandon: Option<Value>,
    ) -> Result<Value, TransportError> {
        await_outcome_in(&self.shared, receiver, method, abandon).await
    }

    /// Send a notification, which expects nothing back.
    pub fn notify(&self, message: Outgoing) -> Result<(), TransportError> {
        self.send(message)
            .map_err(|error| TransportError::Write(error.to_string()))
    }

    fn send(&self, message: Outgoing) -> Result<(), TransportError> {
        let bytes = message.encode()?;
        self.stdin
            .send(bytes)
            .map_err(|_| TransportError::Write("the app-server stdin is closed".into()))
    }

    /// Answer a server-initiated request.
    pub fn respond(&self, id: Value, result: Value) -> Result<(), TransportError> {
        self.send(Outgoing::Response { id, result })
            .map_err(|error| TransportError::Write(error.to_string()))
    }

    /// Refuse a server-initiated request.
    pub fn refuse(&self, id: Value, code: i64, message: &str) -> Result<(), TransportError> {
        self.send(Outgoing::ErrorResponse {
            id,
            code,
            message: message.to_string(),
        })
        .map_err(|error| TransportError::Write(error.to_string()))
    }

    /// The next thing the server said, if anything.
    pub async fn next_event(&mut self) -> Option<ServerEvent> {
        self.events.recv().await
    }

    /// Move the notification stream to a metadata connection's drain task.
    /// Normal turn sessions keep using `next_event` as their sole consumer.
    pub(crate) fn take_events(&mut self) -> mpsc::Receiver<ServerEvent> {
        std::mem::replace(&mut self.events, mpsc::channel(1).1)
    }

    /// Whether the process has exited.
    pub async fn exit_reason(&self) -> Option<Exit> {
        self.shared.exited.lock().await.clone()
    }

    /// Stop the server once its work is done.
    ///
    /// SIGTERM first, so Codex can finish writing the thread it just updated,
    /// then SIGKILL after a grace period, because a server that ignores SIGTERM
    /// would otherwise keep the app alive at quit. The process group is left
    /// alone: nothing is running at the end of a turn, and anything left
    /// deliberately in the background is not FlowX's to end.
    pub async fn close(&self) {
        if let Some(mut child) = self.child.lock().await.take() {
            crate::process::stop_leader(&mut child, TERMINATE_GRACE).await;
        }
        self.leave_running();
        self.mark_closed().await;
    }

    /// Stop the server and everything it started, for Stop or a failed turn.
    ///
    /// The whole tree gets SIGTERM, a grace period, and then SIGKILL: the
    /// server, its group, and the group each command leads, since Codex gives
    /// every command one of its own. Killing only the server, as `close` once
    /// did for every ending, left a `sleep 200` or a quiet `docker build`
    /// running after the user pressed Stop, with nothing left to report on it.
    pub async fn terminate(&self) {
        let tree = self
            .shared
            .tree
            .as_ref()
            .map(crate::process::Watched::snapshot);
        let mut slot = self.child.lock().await;
        match (tree, slot.as_mut()) {
            (Some(tree), leader) => tree.terminate(leader, TERMINATE_GRACE).await,
            (None, Some(child)) => crate::process::stop_leader(child, TERMINATE_GRACE).await,
            (None, None) => {}
        }
        slot.take();
        drop(slot);
        self.leave_running();
        self.mark_closed().await;
    }

    /// Leave the list Quit ends: what is left now is not FlowX's to stop.
    fn leave_running(&self) {
        self.running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    /// Record that this transport is finished, so a later request reports that
    /// instead of waiting out its timeout against a closed pipe.
    async fn mark_closed(&self) {
        let mut slot = self.shared.exited.lock().await;
        if slot.is_none() {
            *slot = Some(Exit {
                stderr: "the Codex app-server was closed.".into(),
                code: None,
            });
        }
        self.shared
            .pending
            .lock()
            .await
            .fail_all("the Codex app-server was closed.");
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        // `kill_on_drop` covers the common case; this is the belt to that
        // braces, for a transport dropped outside an async context where the
        // runtime cannot reap the child.
        if let Ok(mut child) = self.child.try_lock()
            && let Some(child) = child.as_mut()
        {
            let _ = child.start_kill();
        }
    }
}

/// A second way to send requests to a running server, from a task other than
/// the one reading its events.
///
/// [`Transport::next_event`] takes `&mut`, so whoever drives a turn holds the
/// transport for the turn's whole length. Steering that turn is a request made
/// *during* it, from somewhere else, and this handle is that somewhere else. It
/// shares the pending map and the stdin writer, so its answer is correlated by
/// the same reader task as every other response — the turn's driver does not
/// have to be polled for it to arrive.
#[derive(Debug, Clone)]
pub struct Requester {
    stdin: mpsc::UnboundedSender<Vec<u8>>,
    shared: Arc<Shared>,
}

impl Transport {
    /// A cloneable handle for requests made while something else reads events.
    pub fn requester(&self) -> Requester {
        Requester {
            stdin: self.stdin.clone(),
            shared: self.shared.clone(),
        }
    }
}

impl Requester {
    /// Send a request and wait for its response, under the same rules as
    /// [`Transport::request`]: a dead process fails at once, and a server
    /// that never answers fails after [`CONTROL_REQUEST_TIMEOUT`].
    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, TransportError> {
        let (id, receiver) = {
            let mut pending = self.shared.pending.lock().await;
            pending
                .register_or_dead(method)
                .map_err(|reason| TransportError::Exited { stderr: reason })?
        };
        let bytes = Outgoing::Request {
            id,
            method: method.to_string(),
            params,
        }
        .encode()?;
        if self.stdin.send(bytes).is_err() {
            self.shared.pending.lock().await.abandon(&json!(id));
            return Err(TransportError::Write(
                "the app-server stdin is closed".into(),
            ));
        }
        // The same three outcomes as `Transport::await_outcome`, which needs a
        // whole transport to call; this handle has only the shared half.
        match tokio::time::timeout(CONTROL_REQUEST_TIMEOUT, receiver).await {
            Ok(Ok(Outcome::Answered(Ok(value)))) => Ok(value),
            Ok(Ok(Outcome::Answered(Err(error)))) => Err(TransportError::Rejected {
                method: method.to_string(),
                code: error.code,
                message: error.message,
            }),
            Ok(Ok(Outcome::Dead(reason))) => Err(TransportError::Exited { stderr: reason }),
            Ok(Err(_)) => Err(TransportError::Exited {
                stderr: "the transport was dropped before Codex answered.".into(),
            }),
            Err(_) => {
                self.shared.pending.lock().await.abandon(&json!(id));
                Err(TransportError::TimedOut {
                    method: method.to_string(),
                })
            }
        }
    }
}

/// Keep the same tree Stop and Quit use current until the server exits.
fn watch_tree(shared: &Arc<Shared>) {
    let Some(tree) = shared.tree.clone() else {
        return;
    };
    let transport = Arc::downgrade(shared);
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(TREE_REFRESH);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            let Some(shared) = transport.upgrade() else {
                break;
            };
            if shared.exited.lock().await.is_some() {
                break;
            }
            drop(shared);
            tree.refresh();
        }
    });
}

async fn read_stdout(
    stdout: tokio::process::ChildStdout,
    shared: Arc<Shared>,
    events: mpsc::Sender<ServerEvent>,
) {
    let mut reader = BufReader::new(stdout);
    let mut framer = LineFramer::new();
    let mut chunk = [0u8; 16 * 1024];

    loop {
        let read = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(error) => {
                record_exit(&shared, error.to_string()).await;
                return;
            }
        };

        // The framer owns the size limit, so an oversized line is refused while
        // it is still a partial buffer rather than after it has been read whole.
        let lines = match framer.push(&chunk[..read]) {
            Ok(lines) => lines,
            Err(error) => {
                record_exit(&shared, error.to_string()).await;
                return;
            }
        };
        for line in lines {
            handle_line(&line, &shared, &events).await;
        }
    }

    // stdout closed. A partial final line is still worth parsing: a server
    // killed mid-write is otherwise completely silent. The exit itself is
    // recorded by the watcher, once stderr has had a chance to finish.
    if let Some(line) = framer.finish() {
        handle_line(&line, &shared, &events).await;
    }
}

/// Wait for an answer, giving up after the control timeout.
async fn await_outcome_in(
    shared: &Shared,
    receiver: impl std::future::Future<Output = Result<Outcome, oneshot::error::RecvError>>,
    method: &str,
    abandon: Option<Value>,
) -> Result<Value, TransportError> {
    match tokio::time::timeout(CONTROL_REQUEST_TIMEOUT, receiver).await {
        Ok(Ok(Outcome::Answered(Ok(value)))) => Ok(value),
        Ok(Ok(Outcome::Answered(Err(error)))) => Err(TransportError::Rejected {
            method: method.to_string(),
            code: error.code,
            message: error.message,
        }),
        Ok(Ok(Outcome::Dead(reason))) => Err(TransportError::Exited { stderr: reason }),
        // The sender was dropped without a value, which only happens if the
        // transport itself was torn down.
        Ok(Err(_)) => Err(TransportError::Exited {
            stderr: "the transport was dropped before Codex answered.".into(),
        }),
        Err(_) => {
            // Give the slot back, or it leaks for the life of the transport.
            if let Some(id) = abandon {
                shared.pending.lock().await.abandon(&id);
            }
            Err(TransportError::TimedOut {
                method: method.to_string(),
            })
        }
    }
}

async fn handle_line(line: &str, shared: &Arc<Shared>, events: &mpsc::Sender<ServerEvent>) {
    if line.trim().is_empty() {
        return;
    }
    match parse_line(line) {
        Ok(incoming) => dispatch(incoming, shared, events).await,
        Err(error) => {
            // Not fatal. One malformed line in a long stream should not kill a
            // turn that is otherwise going fine.
            tracing::warn!(%error, "a Codex message did not parse");
        }
    }
}

async fn dispatch(
    incoming: crate::codex::protocol::Incoming,
    shared: &Arc<Shared>,
    events: &mpsc::Sender<ServerEvent>,
) {
    match incoming.classify() {
        Classified::Response { id, payload } => {
            shared.pending.lock().await.resolve(&id, payload);
        }
        Classified::Notification { method, params } => {
            // A command's lifecycle can expose a new process before
            // the next timer tick, including one whose server then crashes.
            if (method == "item/started" || method == "item/completed")
                && params.pointer("/item/type").and_then(Value::as_str) == Some("commandExecution")
                && let Some(tree) = &shared.tree
            {
                tree.refresh();
            }
            // Awaited, not discarded: the queue is bounded, so `send` is a
            // future. Discarding it compiles and silently delivers nothing.
            let _ = events
                .send(ServerEvent::Notification { method, params })
                .await;
        }
        Classified::ServerRequest { id, method, params } => {
            let _ = events
                .send(ServerEvent::Request { id, method, params })
                .await;
        }
        Classified::Unknown(value) => {
            tracing::debug!(?value, "ignored a Codex message of unknown shape");
        }
    }
}

async fn record_exit(shared: &Arc<Shared>, stderr: String) {
    // The reader can fail before the process does; capture what it still
    // leads to before the turn tears the server down.
    if let Some(tree) = &shared.tree {
        tree.refresh();
    }
    let mut slot = shared.exited.lock().await;
    if slot.is_some() {
        return;
    }
    let captured = shared.stderr.lock().await.trim().to_string();
    *slot = Some(Exit {
        stderr: if stderr.trim().is_empty() {
            captured
        } else {
            stderr
        },
        code: None,
    });
}

async fn read_stderr(mut stderr: tokio::process::ChildStderr, shared: Arc<Shared>) {
    // Raw bytes, capped as they are read. A line reader stopped at the first
    // byte that was not UTF-8, which closed the pipe and made the server's next
    // diagnostic a write error; and `\r`-only progress output was one line that
    // grew without bound before the cap could apply.
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    while let Ok(read) = stderr.read(&mut chunk).await {
        if read == 0 {
            break;
        }
        // The *tail* is kept, not the head. The last thing a CLI writes before it
        // dies is the reason it died, so a cap that kept the head would preserve
        // the banner and discard the diagnosis — which is the only useful part.
        crate::process::keep_tail(&mut buffer, &chunk[..read], MAX_STDERR_BYTES);
        *shared.stderr.lock().await = crate::process::decode(&buffer);
    }
}

/// Tests against a real child process.
///
/// These are the only tests that prove the ordering, the correlation over an
/// actual pipe, and the exit behaviour. Everything else in this file is pure.
#[cfg(test)]
mod process {
    use super::*;
    use crate::codex::fake_server::FakeServer;

    /// Start a transport against the fake server, or return `None` to skip.
    async fn connect(server: &FakeServer) -> Option<Transport> {
        Transport::start(server.launch(), "FlowX", "test")
            .await
            .ok()
    }

    #[tokio::test]
    async fn the_handshake_happens_before_anything_else() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let Some(transport) = connect(&server).await else {
            panic!("the fake server did not start");
        };
        // Prove the server is still reading before shutting it down: a pipe is
        // FIFO, so a successful `echo` can only have been answered after the
        // server had already consumed `initialized`.
        transport
            .request("echo", Some(json!("after-handshake")))
            .await
            .expect("the server answered after the handshake");
        transport.close().await;

        let log = server.handshake_within(std::time::Duration::from_secs(5));
        let first = log.first().expect("the handshake was recorded");
        assert_eq!(first["method"], "initialize");
        // The reserved id, not a dynamic one.
        assert_eq!(first["id"], 0);
        assert_eq!(first["params"]["clientInfo"]["name"], "FlowX");
        assert_eq!(first["params"]["capabilities"]["experimentalApi"], true);

        // `initialized` follows it as a notification, with no id at all.
        let second = log.get(1).expect("the follow-up was recorded");
        assert_eq!(second["method"], "initialized");
        assert!(
            second.get("id").is_none(),
            "initialized must be a notification, not a request"
        );
    }

    #[tokio::test]
    async fn a_request_receives_its_own_response() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = connect(&server).await.expect("started");
        let value = transport
            .request("echo", Some(json!({"n": 7})))
            .await
            .expect("the echo returned");
        assert_eq!(value["echo"]["n"], 7);
        transport.close().await;
    }

    #[tokio::test]
    async fn concurrent_requests_do_not_cross() {
        // Two in flight at once is the case where a transport that matches on
        // the wrong key silently returns one answer twice.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = std::sync::Arc::new(connect(&server).await.expect("started"));

        let a = {
            let transport = transport.clone();
            tokio::spawn(async move { transport.request("echo", Some(json!("a"))).await })
        };
        let b = {
            let transport = transport.clone();
            tokio::spawn(async move { transport.request("echo", Some(json!("b"))).await })
        };

        assert_eq!(a.await.unwrap().unwrap()["echo"], json!("a"));
        assert_eq!(b.await.unwrap().unwrap()["echo"], json!("b"));
        transport.close().await;
    }

    #[tokio::test]
    async fn many_requests_in_sequence_all_arrive() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = connect(&server).await.expect("started");
        for n in 0..25 {
            let value = transport
                .request("echo", Some(json!(n)))
                .await
                .unwrap_or_else(|error| panic!("request {n} failed: {error}"));
            assert_eq!(value["echo"], json!(n));
        }
        transport.close().await;
    }

    #[tokio::test]
    async fn a_rejected_method_reports_its_code_and_message() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = connect(&server).await.expect("started");
        let error = transport
            .request("refuse", None)
            .await
            .expect_err("the method was refused");
        match error {
            TransportError::Rejected { code, message, .. } => {
                assert_eq!(code, -32601);
                assert!(message.contains("Method not found"));
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
        transport.close().await;
    }

    #[tokio::test]
    async fn a_notification_arrives_as_an_event() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let mut transport = connect(&server).await.expect("started");
        transport
            .notify(Outgoing::Notification {
                method: "notify-me".into(),
                params: None,
            })
            .expect("written");

        // A notification has no id, so it must not be mistaken for a response.
        let event = tokio::time::timeout(Duration::from_secs(5), transport.next_event())
            .await
            .expect("an event arrived in time")
            .expect("the event channel was open");
        match event {
            ServerEvent::Notification { method, params } => {
                assert_eq!(method, "codex/event");
                assert_eq!(params["msg"], "hello from the server");
            }
            other => panic!("expected a notification, got {other:?}"),
        }
        transport.close().await;
    }

    #[tokio::test]
    async fn a_server_request_is_reported_so_the_caller_can_answer() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let mut transport = connect(&server).await.expect("started");
        transport
            .notify(Outgoing::Notification {
                method: "ask-me".into(),
                params: None,
            })
            .expect("written");

        let event = tokio::time::timeout(Duration::from_secs(5), transport.next_event())
            .await
            .expect("an event arrived in time")
            .expect("the event channel was open");
        match event {
            ServerEvent::Request { id, method, .. } => {
                assert_eq!(id, json!("server-1"));
                assert_eq!(method, "applyPatchApproval");
            }
            other => panic!("expected a request, got {other:?}"),
        }
        transport.close().await;
    }

    #[tokio::test]
    async fn a_server_that_dies_during_initialize_fails_at_once_with_its_stderr() {
        // A bad config or an old CLI without `app-server` exits before
        // answering. The handshake used to wait out the 30-second control
        // timeout and then report a timeout, never the reason.
        let launch = |script: &str| Launch {
            executable: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            working_directory: None,
            environment: vec![("PATH".into(), "/usr/bin:/bin".into())],
        };
        let started = tokio::time::timeout(
            Duration::from_secs(5),
            Transport::start(
                launch("echo \"error: unrecognized subcommand 'app-server'\" >&2; exit 2"),
                "FlowX",
                "test",
            ),
        )
        .await
        .expect("failed promptly, not after the control timeout");
        match started {
            Err(TransportError::Exited { stderr }) => {
                assert!(stderr.contains("unrecognized subcommand"), "{stderr}")
            }
            other => panic!("expected an exit with its stderr, got {other:?}"),
        }

        // Nothing on stderr: the binary is named instead.
        let quiet = tokio::time::timeout(
            Duration::from_secs(5),
            Transport::start(launch("exit 1"), "FlowX", "test"),
        )
        .await
        .expect("failed promptly");
        assert!(
            matches!(&quiet, Err(TransportError::Spawn(message)) if message.contains("/bin/sh")),
            "{quiet:?}"
        );
    }

    #[tokio::test]
    async fn a_dying_server_fails_the_pending_request_with_its_stderr() {
        // The case that decides whether a turn reports why or vanishes.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = connect(&server).await.expect("started");
        transport
            .notify(Outgoing::Notification {
                method: "die".into(),
                params: None,
            })
            .expect("written");

        let error = transport
            .request("echo", None)
            .await
            .expect_err("the server had gone");
        match error {
            TransportError::Exited { stderr } => {
                assert!(
                    stderr.contains("told to die"),
                    "the stderr was not surfaced: {stderr}"
                );
            }
            other => panic!("expected an exit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_quiet_exit_still_reports_something() {
        // An OOM kill leaves stderr empty; reporting nothing would look like a
        // turn that completed.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = connect(&server).await.expect("started");
        transport
            .notify(Outgoing::Notification {
                method: "exit-quietly".into(),
                params: None,
            })
            .expect("written");

        let error = transport
            .request("echo", None)
            .await
            .expect_err("the server had gone");
        assert!(
            matches!(&error, TransportError::Exited { stderr } if stderr.contains("unexpectedly")),
            "expected an explained exit, got {error:?}"
        );
    }

    #[tokio::test]
    async fn a_server_that_never_answers_times_out() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = connect(&server).await.expect("started");
        transport
            .notify(Outgoing::Notification {
                method: "never".into(),
                params: None,
            })
            .expect("written");

        // The production timeout is 30s, which is too long for a test, so this
        // asserts the shape with the request running under its own shorter
        // budget rather than weakening the constant.
        let timed_out =
            tokio::time::timeout(Duration::from_millis(500), transport.request("echo", None))
                .await
                .is_err();
        assert!(timed_out, "the request should still be waiting");
        transport.close().await;
    }

    #[tokio::test]
    async fn an_endless_line_is_refused_rather_than_buffered() {
        // The 64 MB guard, exercised for real. Without it this test would exhaust
        // memory instead of failing.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let mut transport = connect(&server).await.expect("started");
        transport
            .notify(Outgoing::Notification {
                method: "ramble".into(),
                params: None,
            })
            .expect("written");

        let event = tokio::time::timeout(Duration::from_secs(30), transport.next_event())
            .await
            .expect("the reader gave up rather than buffering")
            .expect("the event channel was open");
        match event {
            ServerEvent::Notification { method, params } => {
                assert_eq!(method, "flowx/server-exited");
                assert!(
                    params["stderr"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("safety limit"),
                    "the overflow was not reported: {params}"
                );
            }
            other => panic!("expected an exit notification, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_second_request_after_a_die_still_reports_the_exit() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = connect(&server).await.expect("started");
        transport
            .notify(Outgoing::Notification {
                method: "die".into(),
                params: None,
            })
            .expect("written");
        let _ = transport.request("echo", None).await;

        // The second failure must also be a clear exit, not a hang or a panic.
        let error = transport
            .request("echo", None)
            .await
            .expect_err("still dead");
        assert!(
            matches!(error, TransportError::Exited { .. }),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn a_missing_executable_reports_where_it_looked() {
        let error = Transport::start(
            Launch {
                executable: "/nonexistent/codex".into(),
                args: vec!["app-server".into()],
                working_directory: None,
                environment: Vec::new(),
            },
            "FlowX",
            "test",
        )
        .await
        .expect_err("nothing to launch");
        match error {
            TransportError::Spawn(message) => {
                assert!(message.contains("/nonexistent/codex"), "{message}");
            }
            other => panic!("expected a spawn failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn terminating_ends_the_tools_the_server_started() {
        // Stop used to SIGKILL the server alone, at once. The server was not in
        // a group of its own, so the `sleep 200` it was running kept going.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = connect(&server).await.expect("started");
        let started = transport
            .request("spawn-child", None)
            .await
            .expect("the tool started");
        let pid = libc::pid_t::try_from(started["pid"].as_i64().expect("a pid")).unwrap();
        let _reap = crate::process::testing::KillOnDrop(pid);

        transport.terminate().await;
        assert!(
            crate::process::testing::gone_within(pid, Duration::from_secs(5)).await,
            "the tool outlived the server it belonged to"
        );
        assert!(matches!(
            transport.request("echo", None).await,
            Err(TransportError::Exited { .. })
        ));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn terminating_ends_tools_after_the_server_crashes() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = connect(&server).await.expect("started");
        let started = transport
            .request("spawn-child", None)
            .await
            .expect("tool started");
        let pid = libc::pid_t::try_from(started["pid"].as_i64().expect("a pid")).unwrap();
        let _reap = crate::process::testing::KillOnDrop(pid);

        // This command reports no lifecycle notifications: the periodic watch
        // must remember its group while the server is still its parent.
        tokio::time::sleep(TREE_REFRESH * 2).await;
        assert!(matches!(
            transport.request("die", None).await,
            Err(TransportError::Exited { .. })
        ));
        assert!(
            crate::process::process_table()
                .iter()
                .any(|entry| { entry.pid == pid && entry.ppid == 1 && entry.pgid == pid }),
            "the tool must be orphaned in its own group before cleanup"
        );

        transport.terminate().await;
        assert!(
            crate::process::testing::gone_within(pid, Duration::from_secs(5)).await,
            "the crashed server's orphaned tool outlived failure cleanup"
        );
    }

    #[tokio::test]
    async fn closing_a_successful_turn_preserves_its_background_tools() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = connect(&server).await.expect("started");
        let started = transport
            .request("spawn-child", None)
            .await
            .expect("tool started");
        let pid = libc::pid_t::try_from(started["pid"].as_i64().expect("a pid")).unwrap();
        let _reap = crate::process::testing::KillOnDrop(pid);
        tokio::time::sleep(TREE_REFRESH * 2).await;

        transport.close().await;
        // The tree is retained for failures and Stop, but a successful turn's
        // dev server is deliberately left running and unregistered from Quit.
        assert!(transport.running.lock().unwrap().is_none());
        // SAFETY: signal 0 only checks whether this test's child still exists.
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            0,
            "background tool was stopped"
        );
    }

    #[tokio::test]
    async fn stderr_that_is_not_text_does_not_stop_its_capture() {
        // A line reader stopped at the first invalid byte and dropped the pipe,
        // so the server's next write to stderr failed — and the reason it gave
        // when it died was never captured.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = connect(&server).await.expect("started");
        transport
            .request("stderr-noise", None)
            .await
            .expect("answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
        transport
            .request("stderr-noise", None)
            .await
            .expect("the server could still write to stderr");
        transport
            .notify(Outgoing::Notification {
                method: "die".into(),
                params: None,
            })
            .expect("written");
        match transport.request("echo", None).await {
            Err(TransportError::Exited { stderr }) => {
                assert!(stderr.contains("told to die"), "{stderr}")
            }
            other => panic!("expected an exit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn closing_releases_the_process() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = connect(&server).await.expect("started");
        transport.close().await;
        // After close, requests fail rather than hanging on a dead pipe.
        let error = transport
            .request("echo", None)
            .await
            .expect_err("the transport is closed");
        assert!(
            matches!(
                error,
                TransportError::Write(_) | TransportError::Exited { .. }
            ),
            "got {error:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_start_above_the_handshake_range() {
        // The server treats low ids as handshake traffic; a dynamic id must not
        // land there.
        let mut pending = PendingRequests::new();
        let (first, _) = pending.register("thread/start");
        let (second, _) = pending.register("turn/start");
        assert!(first > FIRST_DYNAMIC_ID);
        assert_eq!(second, first + 1);
    }

    #[test]
    fn a_response_reaches_the_caller_that_asked_for_it() {
        let mut pending = PendingRequests::new();
        let (id, receiver) = pending.register("turn/start");
        assert!(pending.resolve(&json!(id), Ok(json!({"ok": true}))));
        match receiver.blocking_recv().unwrap() {
            Outcome::Answered(value) => assert_eq!(value.unwrap()["ok"], json!(true)),
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    #[test]
    fn two_waiters_do_not_cross() {
        let mut pending = PendingRequests::new();
        let (first, first_rx) = pending.register("turn/start");
        let (second, second_rx) = pending.register("thread/list");

        pending.resolve(&json!(second), Ok(json!("second")));
        pending.resolve(&json!(first), Ok(json!("first")));

        assert!(matches!(
            second_rx.blocking_recv().unwrap(),
            Outcome::Answered(Ok(value)) if value == json!("second")
        ));
        assert!(matches!(
            first_rx.blocking_recv().unwrap(),
            Outcome::Answered(Ok(value)) if value == json!("first")
        ));
    }

    #[test]
    fn a_response_for_an_unknown_id_is_dropped_not_fatal() {
        // A late response after a timeout is normal, not an error.
        let mut pending = PendingRequests::new();
        assert!(!pending.resolve(&json!(9999), Ok(Value::Null)));
        assert!(pending.is_empty());
    }

    #[test]
    fn a_non_numeric_id_is_dropped() {
        let mut pending = PendingRequests::new();
        let (_id, _rx) = pending.register("turn/start");
        assert!(!pending.resolve(&json!("not-a-number"), Ok(Value::Null)));
        assert_eq!(pending.len(), 1, "the real waiter was disturbed");
    }

    #[test]
    fn an_error_response_is_delivered_to_the_caller() {
        let mut pending = PendingRequests::new();
        let (id, receiver) = pending.register("thread/start");
        pending.resolve(
            &json!(id),
            Err(IncomingError {
                code: -32601,
                message: "Method not found".into(),
            }),
        );
        match receiver.blocking_recv().unwrap() {
            Outcome::Answered(Err(error)) => assert_eq!(error.code, -32601),
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn abandoning_a_timed_out_request_leaves_no_slot_behind() {
        let mut pending = PendingRequests::new();
        let (id, _rx) = pending.register("turn/start");
        assert!(pending.abandon(&json!(id)));
        assert!(pending.is_empty());
        // And a late response for it is simply dropped.
        assert!(!pending.resolve(&json!(id), Ok(Value::Null)));
    }

    #[test]
    fn failing_everything_reports_which_calls_were_waiting() {
        let mut pending = PendingRequests::new();
        let (_a, a_rx) = pending.register("turn/start");
        let (_b, b_rx) = pending.register("thread/list");
        let methods = pending.fail_all("exited");
        assert_eq!(methods.len(), 2);
        assert!(pending.is_empty());

        // Both callers learn that the server died, rather than seeing a bare
        // channel close that would read as a cancellation.
        for receiver in [a_rx, b_rx] {
            match receiver.blocking_recv().unwrap() {
                Outcome::Dead(reason) => assert_eq!(reason, "exited"),
                other => panic!("expected a dead outcome, got {other:?}"),
            }
        }
    }

    #[test]
    fn failing_everything_on_an_idle_transport_does_nothing() {
        let mut pending = PendingRequests::new();
        assert!(pending.fail_all("x").is_empty());
    }

    #[test]
    fn a_waiter_can_be_identified_by_id() {
        let mut pending = PendingRequests::new();
        let (id, _rx) = pending.register("turn/start");
        assert_eq!(pending.waiting_for(&json!(id)), Some("turn/start"));
        assert_eq!(pending.waiting_for(&json!(1234)), None);
    }

    #[test]
    fn an_exit_with_no_stderr_still_explains_itself() {
        // A killed or OOM'd server leaves stderr empty. Reporting nothing here
        // would make the turn look like it completed normally.
        let exit = Exit {
            stderr: "   ".into(),
            code: Some(9),
        };
        assert!(exit.reason().contains("unexpectedly"));
    }

    #[test]
    fn an_exit_with_stderr_quotes_it() {
        let exit = Exit {
            stderr: "  panic: boom  ".into(),
            code: None,
        };
        assert_eq!(exit.reason(), "Codex app-server exited: panic: boom");
    }

    #[test]
    fn the_standard_launch_asks_for_the_app_server_subcommand() {
        let launch = Launch::codex("/usr/local/bin/codex");
        assert_eq!(launch.args, vec!["app-server".to_string()]);
        assert_eq!(launch.working_directory, None);
    }
}
