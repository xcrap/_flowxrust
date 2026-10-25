//! A fake `claude`, used to test the transport against a real process.
//!
//! Spawning a real child is the point: the parts most likely to break are the
//! parts a mocked stream cannot reach — what actually lands on stdin, what the
//! process does when its input ends, and what a caller sees when it dies mid-turn.
//!
//! Written as a script rather than a second binary so the test has no build step
//! of its own. Its behaviour is chosen by the prompt, in the same way the real
//! CLI's is chosen by what it is asked: a prompt of `!ask-tool` opens a control
//! request and waits, `!die` exits mid-turn, and anything else is a plain turn.

use std::process::Command;

/// The server, as source text.
///
/// It records every message it receives — including the command line it was
/// launched with — because the two things worth pinning are the shape of what
/// FlowX writes and the flags it writes it under.
const SOURCE: &str = r#"
import json, os, sys, threading, time

RECEIVED = "RECEIVED_LOG"
EXITED = "EXITED_LOG"

def gate(name):
    return os.path.join(os.path.dirname(RECEIVED), name)

def wait_for(name):
    while not os.path.exists(gate(name)):
        time.sleep(0.01)

# Background tasks write from their own threads, so a line is never split.
LOCK = threading.Lock()
workers = []

def send(obj):
    with LOCK:
        sys.stdout.write(json.dumps(obj) + "\n")
        sys.stdout.flush()

def record(obj):
    with open(RECEIVED, "a") as handle:
        handle.write(json.dumps(obj) + "\n")

# The launch itself, so a test can assert the flags it asked for.
record({"type": "__started__", "argv": sys.argv, "cwd": os.getcwd(),
        "env": dict(os.environ)})

waiting = []

def prompt_of(message):
    for block in (message.get("message") or {}).get("content") or []:
        if block.get("type") == "text":
            return block.get("text") or ""
    return ""

def emit_init():
    send({"type": "system", "subtype": "init",
          "session_id": "fake-session", "model": "fake-model"})
    # Every turn's first request, which is where Claude reads what was queued
    # for it, steers included.
    send({"type": "system", "subtype": "status", "status": "requesting"})

def emit_result(text):
    # Streamed first and repeated in the assembled message, because that is what
    # Claude does and the parser's echo suppression has to survive it.
    send({"type": "stream_event", "event": {"type": "content_block_delta",
         "delta": {"type": "text_delta", "text": text}}})
    send({"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}})
    send({"type": "result", "subtype": "end_turn",
          "usage": {"input_tokens": 11, "output_tokens": 3}})

def request_control(request_id, tool_name, tool_use_id, tool_input):
    send({"type": "control_request", "request_id": request_id,
          "request": {"subtype": "can_use_tool", "tool_name": tool_name,
                      "tool_use_id": tool_use_id, "input": tool_input}})

def die(code, message):
    if message:
        sys.stderr.write(message + "\n")
        sys.stderr.flush()
    sys.exit(code)

def background_task():
    # The shape Claude Code 2.1.283 writes: the task's end, then a turn of its
    # own that answers the notification, closed by a `result` whose origin says
    # so. Held until the test releases it, and again between the patch and the
    # notification, so what happens in between is observable rather than a race.
    wait_for("release-background")
    send({"type": "system", "subtype": "task_progress", "task_id": "task-1",
          "tool_use_id": "tool-bg", "description": "sleep 6; echo done",
          "usage": {"total_tokens": 0, "tool_uses": 1, "duration_ms": 50},
          "last_tool_name": "Bash"})
    send({"type": "system", "subtype": "task_updated", "task_id": "task-1",
          "patch": {"status": "completed", "end_time": 1}})
    wait_for("release-notification")
    send({"type": "system", "subtype": "task_notification", "task_id": "task-1",
          "tool_use_id": "tool-bg", "status": "completed", "output_file": "/tmp/task-1",
          "summary": "Background command \"sleep 6; echo done\" completed (exit code 0)"})
    emit_init()
    send({"type": "stream_event", "event": {"type": "content_block_delta",
         "delta": {"type": "text_delta", "text": "finished"}}})
    send({"type": "assistant", "message": {"content": [{"type": "text", "text": "finished"}]}})
    send({"type": "result", "subtype": "success", "num_turns": 1,
          "origin": {"kind": "task-notification"},
          "usage": {"input_tokens": 7, "output_tokens": 1}})

def start_background():
    emit_init()
    send({"type": "assistant", "message": {"content": [{"type": "tool_use",
          "id": "tool-bg", "name": "Bash",
          "input": {"command": "sleep 6; echo done", "run_in_background": True}}]}})
    send({"type": "system", "subtype": "task_started", "task_id": "task-1",
          "tool_use_id": "tool-bg", "description": "sleep 6; echo done",
          "is_backgrounded": True, "task_type": "local_bash"})
    send({"type": "user", "message": {"content": [{"type": "tool_result",
          "tool_use_id": "tool-bg", "content": "Command running in background with ID: task-1."}]}})
    worker = threading.Thread(target=background_task)
    worker.start()
    workers.append(worker)
    emit_result("started")

def settled(how):
    # Background work that ends inside the turn and that nothing answers after
    # the turn's `result`: a task the agent stopped itself (Claude marks it
    # told as it kills it), one a subagent owns (its notification goes to the
    # subagent), and one whose notification the turn's next request carried.
    emit_init()
    send({"type": "assistant", "message": {"content": [{"type": "tool_use",
          "id": "tool-s", "name": "Bash",
          "input": {"command": "npm run watch", "run_in_background": True}}]}})
    send({"type": "system", "subtype": "task_started", "task_id": "task-s",
          "tool_use_id": "tool-s", "description": "npm run watch",
          "is_backgrounded": True, "task_type": "local_bash",
          "owned_by_subagent": how == "subagent"})
    send({"type": "user", "message": {"content": [{"type": "tool_result",
          "tool_use_id": "tool-s", "content": "Command running in background with ID: task-s."}]}})
    if how == "stopped":
        send({"type": "system", "subtype": "task_updated", "task_id": "task-s",
              "patch": {"status": "killed", "end_time": 1}})
        send({"type": "system", "subtype": "task_notification", "task_id": "task-s",
              "tool_use_id": "tool-s", "status": "stopped", "summary": "npm run watch"})
    else:
        send({"type": "system", "subtype": "task_updated", "task_id": "task-s",
              "patch": {"status": "completed", "end_time": 1}})
        send({"type": "system", "subtype": "task_notification", "task_id": "task-s",
              "tool_use_id": "tool-s", "status": "completed",
              "summary": "Background command \"npm run watch\" completed (exit code 0)"})
        if how == "carried":
            send({"type": "system", "subtype": "status", "status": "requesting"})
    emit_result("settled")

# A turn whose `result` waits for the next user message, which then races it.
held = []

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    message = json.loads(line)
    record(message)

    if message.get("type") == "user" and held:
        # The steer arrived before the turn ended, so Claude ends the turn
        # with it still queued and answers it next as a turn of its own, which
        # asks for a permission.
        held.pop()
        send({"type": "result", "subtype": "success", "num_turns": 1,
              "queued_turn_count": 1, "usage": {"input_tokens": 4, "output_tokens": 1}})
        emit_init()
        request_control("req-q", "Bash", "tool-q", {"command": "cargo test"})
        waiting.append("req-q")
    elif message.get("type") == "user":
        prompt = prompt_of(message)
        if prompt.startswith("!"):
            # A prompt may be a bare "!die" with nothing after it, so the verb
            # and the rest are split on a space that is allowed not to exist.
            behaviour, _, rest = prompt[1:].partition(" ")
        else:
            behaviour, rest = "answer", prompt

        if behaviour == "answer":
            emit_init()
            emit_result(rest or "hello from the fake server")
        elif behaviour == "background":
            start_background()
        elif behaviour == "settled":
            settled(rest)
        elif behaviour == "queued":
            emit_init()
            send({"type": "stream_event", "event": {"type": "content_block_delta",
                 "delta": {"type": "text_delta", "text": "working"}}})
            held.append(True)
        elif behaviour == "spawn-and-die":
            # A background tool, in a session of its own and writing to its own
            # output as the real CLI starts them, and then Claude dies before
            # anybody presses Stop.
            import subprocess
            child = subprocess.Popen(["sleep", "60"], start_new_session=True,
                                     stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                     stderr=subprocess.DEVNULL)
            with open(gate("child.pid"), "w") as handle:
                handle.write(str(child.pid))
            emit_init()
            send({"type": "system", "subtype": "task_started", "task_id": "task-d",
                  "tool_use_id": "tool-d", "description": "sleep 60",
                  "is_backgrounded": True, "task_type": "local_bash"})
            wait_for("release-death")
            die(9, "fake claude died with a tool running")
        elif behaviour == "ask-tool":
            emit_init()
            request_control("req-1", "Bash", "tool-1", {"command": "cargo test"})
            waiting.append("req-1")
        elif behaviour == "ask-question":
            emit_init()
            request_control("req-2", "AskUserQuestion", "tool-2", {"questions": [
                {"question": "Which database?", "header": "Database",
                 "options": [{"label": "Postgres", "description": "primary store"},
                             {"label": "SQLite"}]}]})
            waiting.append("req-2")
        elif behaviour == "ask-two":
            emit_init()
            request_control("req-a", "Bash", "tool-a", {"command": "ls"})
            request_control("req-b", "Bash", "tool-b", {"command": "ls"})
            waiting.extend(["req-a", "req-b"])
        elif behaviour == "hang":
            emit_init()
        elif behaviour == "stall":
            # Stops reading for a moment, so FlowX's next writes back up
            # behind a full pipe.
            import time
            time.sleep(1.0)
        elif behaviour == "spawn-and-hang":
            # A tool Claude started, still running when the user presses
            # Stop. The real CLI starts each `Bash` call in a session of its
            # own, so this one is outside Claude's process group.
            import subprocess
            child = subprocess.Popen(["sleep", "60"], start_new_session=True)
            with open(os.path.join(os.path.dirname(RECEIVED), "child.pid"), "w") as handle:
                handle.write(str(child.pid))
            emit_init()
        elif behaviour == "noisy-death":
            # Not UTF-8 and `\r`-only progress, then the reason, then the exit.
            sys.stderr.buffer.write(b"\xff\xfe not text\r50%\r60%\r\n")
            sys.stderr.buffer.flush()
            import time
            time.sleep(0.2)
            die(7, "the real reason")
        elif behaviour == "error":
            emit_init()
            send({"type": "result", "is_error": True, "result": "hit a limit"})
        elif behaviour == "die":
            # Starts, says something, and dies without ever finishing. The case
            # that decides whether a turn reports why or vanishes.
            emit_init()
            send({"type": "stream_event", "event": {"type": "content_block_delta",
                 "delta": {"type": "text_delta", "text": "working"}}})
            die(7, "fake claude died mid-turn")
        elif behaviour == "die-quietly":
            # An OOM kill: no stderr, no status anybody can read.
            die(7, "")
        elif behaviour == "ramble":
            # More than FlowX's line limit and never terminated, so the reader
            # has to give up rather than buffer without bound.
            sys.stdout.write("x" * (40 * 1024 * 1024))
            sys.stdout.flush()
            die(0, "")
    elif message.get("type") == "control_response":
        request_id = (message.get("response") or {}).get("request_id")
        decision = (message.get("response") or {}).get("response") or {}
        if request_id == "req-2" and decision.get("behavior") == "allow":
            # Claude 2.1.283 validates updatedInput before calling the tool.
            # AskUserQuestion accepts a question-text -> answer-string record,
            # not an array of pairs (which our old fake silently accepted).
            answers = (decision.get("updatedInput") or {}).get("answers")
            if not isinstance(answers, dict) or not all(isinstance(v, str) for v in answers.values()):
                send({"type": "result", "is_error": True,
                      "result": "AskUserQuestion updatedInput.answers must be an object"})
                continue
        if request_id in waiting:
            waiting.remove(request_id)
        if not waiting:
            emit_result("answered")

# Reached only on end of input: the exit is what FlowX's closing stdin caused.
# Like Claude in print mode, a closed input still waits for running tasks and
# answers their notifications before the process exits.
for worker in workers:
    worker.join()
with open(EXITED, "w") as handle:
    handle.write(json.dumps({"code": 0}) + "\n")
"#;

/// A fake `claude` on disk, cleaned up when dropped.
pub struct FakeClaude {
    _dir: tempfile::TempDir,
    script: std::path::PathBuf,
    received: std::path::PathBuf,
    exited: std::path::PathBuf,
}

impl FakeClaude {
    /// Write the script and make it executable.
    ///
    /// `python3` is used because it is present on every macOS install and needs
    /// no build step; if it is missing the test skips rather than fails.
    pub fn new() -> Option<Self> {
        if !python_available() {
            eprintln!("skipping: python3 is not on PATH");
            return None;
        }
        let dir = tempfile::tempdir().ok()?;
        let script = dir.path().join("fake_claude.py");
        std::fs::write(&script, SOURCE).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&script).ok()?.permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(&script, permissions).ok()?;
        }
        Some(Self {
            received: dir.path().join("RECEIVED_LOG"),
            exited: dir.path().join("EXITED_LOG"),
            _dir: dir,
            script,
        })
    }

    /// A launch spec for this server, with the logs substituted in.
    pub fn launch(&self) -> crate::claude::transport::Launch {
        self.launch_with_env(&[])
    }

    /// A launch spec with behaviour pinned by the environment.
    ///
    /// `PATH` is added when the caller has not set it, because the transport
    /// clears the environment and `python3` has to stay findable.
    pub fn launch_with_env(&self, pairs: &[(&str, &str)]) -> crate::claude::transport::Launch {
        let source = SOURCE
            .replace("RECEIVED_LOG", &self.received.to_string_lossy())
            .replace("EXITED_LOG", &self.exited.to_string_lossy());
        let runnable = self.script.with_extension("run.py");
        std::fs::write(&runnable, source).expect("the runnable script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(mut permissions) = std::fs::metadata(&runnable).map(|meta| meta.permissions())
            {
                permissions.set_mode(0o700);
                let _ = std::fs::set_permissions(&runnable, permissions);
            }
        }

        let mut environment: Vec<(String, String)> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        if !environment.iter().any(|(key, _)| key == "PATH") {
            environment.push(("PATH".into(), "/usr/bin:/bin".into()));
        }
        crate::claude::transport::Launch {
            executable: "python3".into(),
            args: vec![runnable.to_string_lossy().into_owned()],
            working_directory: None,
            environment,
        }
    }

    /// The command line the process actually saw, script path included.
    ///
    /// Polled rather than read once: the process may not have started by the
    /// time the caller asks.
    pub async fn argv(&self) -> Vec<String> {
        let received = self.received_within(1, STARTUP_TIMEOUT).await;
        let Some(started) = received.first() else {
            return Vec::new();
        };
        started["argv"]
            .as_array()
            .map(|argv| {
                argv.iter()
                    .filter_map(|argument| argument.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Let the task `!background` started finish. It stops again after the
    /// patch that says so, until [`Self::release_notification`].
    pub fn release_background(&self) {
        self.release("release-background");
    }

    /// Let the finished task's notification, and the follow-up turn that
    /// answers it, through.
    pub fn release_notification(&self) {
        self.release("release-notification");
    }

    /// Open a gate the script is waiting at.
    pub fn release(&self, gate: &str) {
        std::fs::write(self.received.with_file_name(gate), "go").expect("the release file");
    }

    /// The pid of the tool `!spawn-and-hang` started, once it has started.
    pub async fn child_pid(&self) -> Option<i32> {
        let path = self.received.with_file_name("child.pid");
        tokio::time::timeout(STARTUP_TIMEOUT, async {
            loop {
                if let Some(pid) = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| text.trim().parse().ok())
                {
                    return pid;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .ok()
    }

    /// Every message the server received, oldest first.
    ///
    /// A half-written line does not parse and is skipped rather than failing the
    /// read, which is why the polling helper below exists.
    pub fn received(&self) -> Vec<serde_json::Value> {
        read_json_lines(&self.received)
    }

    /// The received messages, polled until there are `count` of them.
    ///
    /// Asynchronous on purpose: these helpers are called from inside a turn, and
    /// a blocking sleep there would stop the writer task from ever handing the
    /// prompt to the child.
    pub async fn received_within(
        &self,
        count: usize,
        timeout: std::time::Duration,
    ) -> Vec<serde_json::Value> {
        tokio::time::timeout(timeout, async {
            loop {
                let received = self.received();
                if received.len() >= count {
                    return received;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| self.received())
    }

    /// The received messages matching `wanted`, polled until `count` have arrived.
    ///
    /// Counted after filtering, because the tests that use this are waiting for
    /// one kind of message and the ones before it are not evidence of anything.
    pub async fn received_matching(
        &self,
        count: usize,
        timeout: std::time::Duration,
        wanted: impl Fn(&serde_json::Value) -> bool,
    ) -> Vec<serde_json::Value> {
        let matching = |received: Vec<serde_json::Value>| -> Vec<serde_json::Value> {
            received.into_iter().filter(|m| wanted(m)).collect()
        };
        tokio::time::timeout(timeout, async {
            loop {
                let found = matching(self.received());
                if found.len() >= count {
                    return found;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_default()
    }

    /// Whether the process ended on its own, within the timeout.
    ///
    /// Recorded when the server's input reached end of file, which only happens
    /// because FlowX closed it — so this is how a test proves the turn's stdin
    /// was closed rather than assuming it.
    pub async fn exited_within(&self, timeout: std::time::Duration) -> Option<serde_json::Value> {
        tokio::time::timeout(timeout, async {
            loop {
                if let Some(exited) = read_json_lines(&self.exited).into_iter().next() {
                    return exited;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .ok()
    }
}

/// How long a helper waits for a process that was only just started.
const STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn read_json_lines(path: &std::path::Path) -> Vec<serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn python_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}
