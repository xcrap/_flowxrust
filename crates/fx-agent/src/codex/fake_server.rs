//! A fake `codex app-server`, used to test the transport against a real process.
//!
//! Spawning a real child is the point: the parts most likely to break are the
//! parts a mocked stream cannot reach — the handshake order, response
//! correlation over an actual pipe, and what a caller sees when the process dies
//! mid-request.
//!
//! Written as a script rather than a second binary so the test has no build step
//! of its own. It speaks just enough of the protocol to drive the paths above.

use std::process::Command;

/// The server, as source text.
///
/// It is deliberately dumb: it answers whatever it is asked with an id echoed
/// back, which is exactly what a correlation test needs, and it exits on
/// `die` with a message on stderr so the exit path is observable.
const SOURCE: &str = r#"
import json, os, sys, time

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

HERE = os.path.dirname(os.path.abspath(__file__))
with open(os.path.join(HERE, "server.pid"), "w") as pid:
    pid.write(str(os.getpid()))

def flood():
    # More notifications than the transport queues, ahead of a reply: a caller
    # that never drains them never sees the reply either.
    if os.path.exists(os.path.join(HERE, "flood")):
        for n in range(2000):
            send({"jsonrpc": "2.0", "method": "codex/event", "params": {"n": n}})

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    mid = msg.get("id")

    if method == "initialize":
        # Record the handshake so the test can assert its shape and order.
        with open("HANDSHAKE_LOG", "a") as log:
            log.write(json.dumps({"method": method, "id": mid,
                                 "params": msg.get("params")}) + "\n")
        send({"jsonrpc": "2.0", "id": mid, "result": {"userAgent": "fake"}})
    elif method == "initialized":
        # The id key is omitted rather than written as null, so the test can
        # assert that a notification really carried no id.
        entry = {"method": method}
        if mid is not None:
            entry["id"] = mid
        with open("HANDSHAKE_LOG", "a") as log:
            log.write(json.dumps(entry) + "\n")
    elif method is None and mid == "interactive-1":
        send({"jsonrpc": "2.0", "method": "item/agentMessage/delta", "params": {"delta": json.dumps(msg)}})
        send({"jsonrpc": "2.0", "method": "turn/completed", "params": {"turn": {"id": "turn-1", "status": "completed"}}})
    elif method == "model/list":
        flood()
        if (msg.get("params") or {}).get("cursor"):
            data = [{"id": "model-second", "displayName": "Second", "inputModalities": ["text"]}]
            cursor = None
        else:
            data = [{"id": "model-first", "displayName": "First", "isDefault": True,
                     "defaultReasoningEffort": "high", "supportedReasoningEfforts": [{"reasoningEffort": "high"}]},
                    {"id": "model-hidden", "hidden": True}]
            cursor = "second-page"
        send({"jsonrpc":"2.0", "id":mid, "result":{"data":data, "nextCursor":cursor}})
    elif method == "env":
        # The whole environment, so a test can assert on whatever it set.
        send({"jsonrpc": "2.0", "id": mid, "result": dict(os.environ)})
    elif method == "echo":
        send({"jsonrpc": "2.0", "id": mid, "result": {"echo": msg.get("params")}})
    elif method == "slow":
        time.sleep(0.2)
        send({"jsonrpc": "2.0", "id": mid, "result": {"slow": True}})
    elif method == "spawn-child":
        # A tool the server started, in the server's own process group.
        import subprocess
        child = subprocess.Popen(["sleep", "60"])
        send({"jsonrpc": "2.0", "id": mid, "result": {"pid": child.pid}})
    elif method == "stderr-noise":
        # Not UTF-8, and progress that only ever returns the carriage.
        sys.stderr.buffer.write(b"\xff\xfe not text\r50%\r60%\r\n")
        sys.stderr.buffer.flush()
        send({"jsonrpc": "2.0", "id": mid, "result": {"noisy": True}})
    elif method == "refuse":
        send({"jsonrpc": "2.0", "id": mid,
              "error": {"code": -32601, "message": "Method not found"}})
    elif method in ("thread/start", "thread/resume"):
        requested = (msg.get("params") or {}).get("threadId")
        send({"jsonrpc": "2.0", "id": mid, "result": {"thread": {
            "id": requested or "thread-new"}}})
    elif method == "turn/start":
        prompt = ""
        for item in (msg.get("params") or {}).get("input") or []:
            if item.get("type") == "text":
                prompt = item.get("text") or ""
        if prompt == "slow-ack":
            # Long enough for a Stop to arrive between the request and its reply.
            time.sleep(0.3)
        send({"jsonrpc": "2.0", "id": mid, "result": {"turn": {"id": "turn-1"}}})
        thread = (msg.get("params") or {}).get("threadId")

        if prompt == "fail-now":
            send({"jsonrpc": "2.0", "method": "turn/completed",
                  "params": {"turn": {"id": "turn-1", "status": "failed",
                                      "error": {"message": "the model refused"}}}})
        elif prompt in ("hang", "slow-ack"):
            # Starts, says nothing, and never completes.
            send({"jsonrpc": "2.0", "method": "turn/started",
                  "params": {"turn": {"id": "turn-1"}}})
        elif prompt == "quiet":
            # Silent for as long as the test likes — a `sleep 200`, a quiet
            # build — and then finishes normally.
            send({"jsonrpc": "2.0", "method": "turn/started",
                  "params": {"turn": {"id": "turn-1"}}})
            while not os.path.exists(os.path.join(HERE, "release-quiet")):
                time.sleep(0.005)
            send({"jsonrpc": "2.0", "method": "item/agentMessage/delta",
                  "params": {"delta": "finished after quiet work"}})
            send({"jsonrpc": "2.0", "method": "turn/completed",
                  "params": {"turn": {"id": "turn-1", "status": "completed"}}})
        elif prompt == "die-mid-turn":
            send({"jsonrpc": "2.0", "method": "turn/started",
                  "params": {"turn": {"id": "turn-1"}}})
            sys.stderr.write("server died mid turn\n")
            sys.stderr.flush()
            sys.exit(4)
        elif prompt in ("interactive-approval", "interactive-question", "foreign-request"):
            send({"jsonrpc": "2.0", "method": "turn/started",
                  "params": {"threadId": thread, "turn": {"id": "turn-1"}}})
            if prompt == "interactive-question":
                method = "item/tool/requestUserInput"
                params = {"threadId": thread, "turnId": "turn-1", "questions": [
                    {"id": "database", "question": "Which database?", "options": [{"label": "SQLite"}]},
                    {"id": "region", "question": "Which region?", "options": []}]}
            else:
                method = "item/commandExecution/requestApproval"
                params = {"threadId": "other-thread" if prompt == "foreign-request" else thread,
                          "turnId": "turn-1", "itemId": "command-1", "command": "cargo test", "cwd": "/tmp"}
            send({"jsonrpc": "2.0", "id": "interactive-1", "method": method, "params": params})
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
        # Recorded, so a test can see which turn it was asked to stop.
        with open("HANDSHAKE_LOG", "a") as log:
            log.write(json.dumps({"method": method, "params": msg.get("params")}) + "\n")
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
    elif method == "thread/list":
        flood()
        if os.environ.get("FAKE_NO_LIST"):
            send({"jsonrpc": "2.0", "id": mid,
                  "error": {"code": -32601, "message": "Method not found"}})
            continue
        cursor = (msg.get("params") or {}).get("cursor")
        if cursor == "page-1":
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "data": [{"id": "thread-c", "title": "Third", "cwd": "/tmp/project"}],
                "nextCursor": "page-2"}})
        elif cursor == "page-2":
            # The last page: no cursor, so the caller stops.
            send({"jsonrpc": "2.0", "id": mid, "result": {"data": [
                {"id": "thread-b", "title": "Second", "cwd": "/tmp/project"}]}})
        elif cursor == "page-repeats":
            # A cursor that never advances would page forever.
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "data": [{"id": "loop", "cwd": "/tmp/project"}],
                "nextCursor": "page-repeats"}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {"data": [
                {"id": "thread-a", "title": "A thread", "cwd": "/tmp/project",
                 "updatedAt": 1735689600000}],
                **({"nextCursor": os.environ["FAKE_PAGES"]} if os.environ.get("FAKE_PAGES") else {})}})
    elif method == "thread/read":
        flood()
        send({"jsonrpc": "2.0", "id": mid, "result": {"thread": {"turns": [
            {"items": [{"type": "userMessage", "text": "hello"},
                       {"type": "agentMessage", "text": "hi"}]}]}}})
    elif method == "notify-me":
        send({"jsonrpc": "2.0", "method": "codex/event",
              "params": {"msg": "hello from the server"}})
    elif method == "ask-me":
        send({"jsonrpc": "2.0", "id": "server-1", "method": "applyPatchApproval",
              "params": {"callId": "c1"}})
    elif method == "die":
        sys.stderr.write("fake server was told to die\n")
        sys.stderr.flush()
        sys.exit(3)
    elif method == "exit-quietly":
        sys.exit(0)
    elif method == "ramble":
        # A line with no terminator ever: the reader must give up rather than
        # buffer without bound.
        sys.stdout.write("x" * (90 * 1024 * 1024))
        sys.stdout.flush()
    elif method == "thread/list-unknown":
        send({"jsonrpc": "2.0", "id": mid,
              "error": {"code": -32601, "message": "Method not found"}})
    elif method == "never":
        time.sleep(3600)
    else:
        send({"jsonrpc": "2.0", "id": mid,
              "error": {"code": -32601, "message": "unknown method"}})
"#;

/// A fake server on disk, cleaned up when dropped.
pub struct FakeServer {
    _dir: tempfile::TempDir,
    script: std::path::PathBuf,
    log: std::path::PathBuf,
}

impl FakeServer {
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
        let script = dir.path().join("fake_codex.py");
        std::fs::write(&script, SOURCE).ok()?;
        let mut permissions = std::fs::metadata(&script).ok()?.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o700);
        }
        std::fs::set_permissions(&script, permissions).ok()?;
        Some(Self {
            log: dir.path().join("HANDSHAKE_LOG"),
            _dir: dir,
            script,
        })
    }

    /// Check that the embedded script is valid Python.
    ///
    /// This is not paranoia. A missing brace in the script makes every process
    /// test fail by timing out after the control timeout — minutes of red for a
    /// one-character mistake, and easy to misread as a transport bug.
    pub fn is_valid_python() -> Result<(), String> {
        let temp = std::env::temp_dir().join(format!("fx-fake-codex-{}.py", std::process::id()));
        std::fs::write(&temp, SOURCE).map_err(|error| error.to_string())?;
        let output = std::process::Command::new("python3")
            .args(["-m", "py_compile", &temp.to_string_lossy()])
            .output()
            .map_err(|error| format!("python3 could not be run: {error}"))?;
        let _ = std::fs::remove_file(&temp);
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).into_owned())
        }
    }

    /// A file beside the script: where the server writes its pid and looks for
    /// the markers that change its behaviour (`flood`, `release-quiet`).
    pub fn file(&self, name: &str) -> std::path::PathBuf {
        self.script.with_file_name(name)
    }

    /// The absolute path of the runnable script.
    pub fn script_path(&self) -> std::path::PathBuf {
        self.launch()
            .args
            .into_iter()
            .next()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| self.script.with_extension("run.py"))
    }

    /// A launch spec that sets the server's behaviour.
    pub fn launch_with_env(&self, pairs: &[(&str, &str)]) -> crate::codex::transport::Launch {
        let mut launch = self.launch();
        launch.environment.extend(
            pairs
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string())),
        );
        // The interpreter must stay findable alongside the added variables.
        if !launch.environment.iter().any(|(key, _)| key == "PATH") {
            launch
                .environment
                .push(("PATH".into(), "/usr/bin:/bin".into()));
        }
        launch
    }

    /// A launch spec for this server.
    ///
    /// The log path is substituted into the script's source, so the server
    /// writes where the test can read.
    pub fn launch(&self) -> crate::codex::transport::Launch {
        let source = SOURCE.replace("HANDSHAKE_LOG", &self.log.to_string_lossy());
        let script = self.script.with_extension("run.py");
        std::fs::write(&script, source).expect("the runnable script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&script).unwrap().permissions();
            permissions.set_mode(0o700);
            let _ = std::fs::set_permissions(&script, permissions);
        }
        crate::codex::transport::Launch {
            executable: "python3".into(),
            args: vec![script.to_string_lossy().into_owned()],
            working_directory: None,
            environment: Vec::new(),
        }
    }

    /// The handshake, polled until it has two entries or the deadline passes.
    ///
    /// `Transport::start` returns once `initialize` is answered, which can be
    /// before the `initialized` notification has been written to the pipe. The
    /// child is the only place that ordering is observable, so this waits for it
    /// rather than reading once and hoping.
    pub fn handshake_within(&self, timeout: std::time::Duration) -> Vec<serde_json::Value> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let log = self.handshake();
            if log.len() >= 2 || std::time::Instant::now() >= deadline {
                return log;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// The handshake the server actually observed.
    pub fn handshake(&self) -> Vec<serde_json::Value> {
        let Ok(text) = std::fs::read_to_string(&self.log) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
}

fn python_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod guard {
    use super::FakeServer;

    #[test]
    fn the_embedded_script_is_valid_python() {
        if let Err(error) = FakeServer::is_valid_python() {
            panic!("the Codex fake server does not compile: {error}");
        }
    }
}
