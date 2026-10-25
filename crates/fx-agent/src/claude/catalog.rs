//! Read Claude Code's authenticated model menu without sending a user turn.
//!
//! The Agent SDK's `initialize` control response owns the model names and
//! capabilities. CLI help only contains examples and is not a model catalog.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use fx_core::services::runtime::{RuntimeDiscovery, claude_spec};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::Model;
use crate::codex::framing::LineFramer;

const REQUEST_ID: &str = "flowx-model-discovery";
const CONTEXT_REQUEST_ID: &str = "flowx-context-usage";
const RESPONSE_LIMIT: usize = 1024 * 1024;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);
const CLOSE_GRACE: Duration = Duration::from_millis(750);

#[derive(Debug, Default)]
pub struct ModelCatalog {
    pub models: Vec<Model>,
    /// Concrete IDs can be saved from earlier runs while the native menu uses
    /// aliases such as `opus`. Both refer to the same advertised capabilities.
    pub aliases: HashMap<String, String>,
}

pub async fn discover_models(discovery: &RuntimeDiscovery) -> Result<ModelCatalog, String> {
    let launch = discovery
        .launch(&claude_spec())
        .await
        .map_err(|error| error.to_string())?;
    let mut command = Command::new(launch.executable);
    command
        .args(discovery_arguments())
        .env_clear()
        .envs(launch.environment)
        .current_dir(std::env::temp_dir());
    query_catalog(command, DISCOVERY_TIMEOUT).await
}

/// How full a saved Claude session's context is, as `/context` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionContext {
    pub used: u64,
    pub window: u64,
    /// The model the window belongs to, such as `claude-opus-5-5[1m]`.
    pub model: Option<String>,
}

/// Ask Claude how full a saved session's context is, without a turn.
///
/// The session is resumed with persistence off and no tools, and only control
/// requests are written: `get_context_usage` in its `summary` form answers from
/// the session's last usage and local estimates, with no model call. Neither
/// the transcript nor the window size can be read any other way before the
/// next turn's `result` — the window is not in the transcript, and it depends
/// on the model and on whether the account has the 1M-token context.
pub async fn session_context(
    discovery: &RuntimeDiscovery,
    working_directory: &str,
    session_id: &str,
) -> Result<SessionContext, String> {
    let session = uuid::Uuid::parse_str(session_id.trim())
        .map_err(|_| "The saved Claude session ID is invalid.".to_string())?;
    let launch = discovery
        .launch(&claude_spec())
        .await
        .map_err(|error| error.to_string())?;
    let mut command = Command::new(launch.executable);
    command
        .args(discovery_arguments())
        .args(["--resume", &session.to_string()])
        .env_clear()
        .envs(launch.environment)
        // A session is found through the project it ran in.
        .current_dir(working_directory);
    query_context(command, DISCOVERY_TIMEOUT).await
}

async fn query_context(command: Command, timeout: Duration) -> Result<SessionContext, String> {
    let requests = [
        json!({
            "type": "control_request",
            "request_id": REQUEST_ID,
            "request": {"subtype": "initialize", "hooks": {}}
        }),
        json!({
            "type": "control_request",
            "request_id": CONTEXT_REQUEST_ID,
            "request": {"subtype": "get_context_usage", "detail": "summary"}
        }),
    ];
    let response = control_query(command, &requests, CONTEXT_REQUEST_ID, timeout, &CONTEXT).await?;
    parse_context(&response)
}

fn parse_context(response: &Value) -> Result<SessionContext, String> {
    let used = response["totalTokens"]
        .as_u64()
        .ok_or("Claude's context report had no token count.")?;
    let window = response["maxTokens"]
        .as_u64()
        .filter(|window| *window > 0)
        .ok_or("Claude's context report had no window size.")?;
    Ok(SessionContext {
        used,
        window,
        model: response["model"]
            .as_str()
            .filter(|model| !model.is_empty())
            .map(str::to_owned),
    })
}

fn discovery_arguments() -> [&'static str; 13] {
    [
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--no-session-persistence",
        "--safe-mode",
        "--strict-mcp-config",
        "--mcp-config",
        "{\"mcpServers\":{}}",
        "--tools",
        "",
    ]
}

async fn query_catalog(command: Command, timeout: Duration) -> Result<ModelCatalog, String> {
    let request = json!({
        "type": "control_request",
        "request_id": REQUEST_ID,
        "request": {"subtype": "initialize", "hooks": {}}
    });
    let response = control_query(command, &[request], REQUEST_ID, timeout, &CATALOG).await?;
    parse_models(&response)
}

/// What a control query is for, in the words its errors use.
struct Purpose {
    start: &'static str,
    exited: &'static str,
    too_large: &'static str,
    rejected: &'static str,
    timed_out: &'static str,
}

const CATALOG: Purpose = Purpose {
    start: "Claude model discovery could not start",
    exited: "Claude exited before returning its model catalog.",
    too_large: "Claude model discovery exceeded its response limit.",
    rejected: "Claude rejected its model discovery request.",
    timed_out: "Claude model discovery timed out.",
};

const CONTEXT: Purpose = Purpose {
    start: "Claude's context check could not start",
    exited: "Claude exited before reporting the session's context.",
    too_large: "Claude's context check exceeded its response limit.",
    rejected: "Claude could not report this session's context.",
    timed_out: "Claude's context check timed out.",
};

/// Write control requests to a metadata-only Claude process and return the
/// payload of the one answering `wanted`.
async fn control_query(
    mut command: Command,
    requests: &[Value],
    wanted: &str,
    timeout: Duration,
    purpose: &Purpose,
) -> Result<Value, String> {
    // Safe mode disables custom hooks/plugins; strict MCP and empty tools keep
    // the process metadata-only. No `user` frame is ever written to it.
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("{}: {error}", purpose.start))?;
    let mut stdin = child.stdin.take().ok_or("Claude discovery has no stdin.")?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or("Claude discovery has no stdout.")?;
    let stderr = child
        .stderr
        .take()
        .ok_or("Claude discovery has no stderr.")?;
    let mut diagnostics = tokio::spawn(async move {
        let mut stderr = stderr;
        let mut saved = Vec::new();
        let mut chunk = [0; 4096];
        while let Ok(count) = stderr.read(&mut chunk).await {
            if count == 0 {
                break;
            }
            let keep = count.min(4096_usize.saturating_sub(saved.len()));
            saved.extend_from_slice(&chunk[..keep]);
        }
        String::from_utf8_lossy(&saved).trim().to_string()
    });
    let result = tokio::time::timeout(timeout, async {
        for request in requests {
            let mut line = request.to_string();
            line.push('\n');
            stdin
                .write_all(line.as_bytes())
                .await
                .map_err(|error| error.to_string())?;
        }
        stdin.flush().await.map_err(|error| error.to_string())?;
        let mut framer = LineFramer::with_limit(RESPONSE_LIMIT);
        let mut chunk = [0; 4096];
        let mut received = 0;
        loop {
            let count = stdout
                .read(&mut chunk)
                .await
                .map_err(|error| error.to_string())?;
            if count == 0 {
                return Err(purpose.exited.to_string());
            }
            received += count;
            if received > RESPONSE_LIMIT {
                return Err(purpose.too_large.to_string());
            }
            for line in framer
                .push(&chunk[..count])
                .map_err(|error| error.to_string())?
            {
                // A line that is not JSON — a warning, an update notice — is
                // not the answer, and is passed over like any other frame that
                // is not. Failing on it failed the whole probe.
                let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if frame["type"] != "control_response" || frame["response"]["request_id"] != wanted
                {
                    continue;
                }
                if frame["response"]["subtype"] != "success" {
                    return Err(purpose.rejected.to_string());
                }
                return Ok(frame["response"]["response"].clone());
            }
        }
    })
    .await
    .unwrap_or_else(|_| Err(purpose.timed_out.to_string()));
    drop(stdin);
    if tokio::time::timeout(CLOSE_GRACE, child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    // The process is reaped on every normal/error/timeout path. Dropping the
    // outer discovery future also kills it through kill_on_drop.
    let stderr = match tokio::time::timeout(CLOSE_GRACE, &mut diagnostics).await {
        Ok(Ok(stderr)) => stderr,
        _ => {
            diagnostics.abort();
            String::new()
        }
    };
    result.map_err(|reason| {
        if stderr.is_empty() {
            reason
        } else {
            format!("{reason} {stderr}")
        }
    })
}

fn parse_models(response: &Value) -> Result<ModelCatalog, String> {
    let rows = response["models"]
        .as_array()
        .ok_or("Claude's initialization response did not contain a model catalog.")?;
    let mut catalog = ModelCatalog::default();
    for row in rows {
        let Some(id) = row["value"].as_str().filter(|id| !id.trim().is_empty()) else {
            continue;
        };
        if catalog.models.iter().any(|model| model.id == id) {
            continue;
        }
        let supported_efforts: Vec<String> = if row["supportsEffort"] == false {
            Vec::new()
        } else {
            row["supportedEffortLevels"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|effort| !effort.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let default_effort = row["defaultEffort"]
            .as_str()
            .filter(|effort| supported_efforts.iter().any(|known| known == effort))
            .map(str::to_owned);
        let model = Model {
            id: id.into(),
            display_name: row["displayName"].as_str().unwrap_or(id).into(),
            // The native response currently omits these fields. Unknown is
            // preferable to fabricating a context window from the model name.
            context_window: row["contextWindow"].as_u64().unwrap_or(0),
            max_context_window: row["maxContextWindow"].as_u64().unwrap_or(0),
            supported_efforts,
            default_effort,
            supports_vision: row["supportsVision"].as_bool().unwrap_or(false),
            is_default: id == "default",
        };
        // Prefer the named choice over the default alias for concrete IDs.
        if id != "default"
            && let Some(resolved) = row["resolvedModel"].as_str()
            && resolved != id
        {
            catalog.aliases.insert(resolved.into(), id.into());
        }
        catalog.models.push(model);
    }
    if catalog.models.is_empty() {
        return Err("Claude returned no available model choices.".into());
    }
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = include_str!("fixtures/initialize-models-2.1.283.json");

    #[test]
    fn current_live_initialize_shape_preserves_models_aliases_and_effort_capabilities() {
        let frame: Value = serde_json::from_str(LIVE).unwrap();
        let catalog = parse_models(&frame["response"]["response"]).unwrap();
        assert_eq!(catalog.models.len(), 11);
        assert_eq!(catalog.models[0].display_name, "Default (recommended)");
        assert_eq!(catalog.models[1].id, "opus");
        assert_eq!(catalog.models[1].display_name, "Opus 5.5");
        assert!(
            catalog
                .models
                .iter()
                .any(|model| model.id == "claude-fable-5-1")
        );
        assert_eq!(catalog.aliases["claude-opus-5-5"], "opus");
        let haiku = catalog
            .models
            .iter()
            .find(|model| model.id == "haiku")
            .unwrap();
        assert!(haiku.supported_efforts.is_empty());
        let old = catalog
            .models
            .iter()
            .find(|model| model.id == "claude-opus-4-6")
            .unwrap();
        assert_eq!(old.supported_efforts, ["low", "medium", "high", "max"]);
        assert_eq!(
            catalog.models[1].supported_efforts,
            ["low", "medium", "high", "xhigh", "max"]
        );
        assert_eq!(catalog.models[1].context_window, 0);
    }

    #[tokio::test]
    async fn discovery_only_initializes_then_closes_without_a_turn() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = temp.path().join("models.json");
        let seen = temp.path().join("received.json");
        std::fs::write(&fixture, LIVE).unwrap();
        let mut command = Command::new("python3");
        command.args(["-u", "-c", r#"
import json, sys
request = json.loads(sys.stdin.readline())
assert request == {"type":"control_request", "request_id":"flowx-model-discovery", "request":{"subtype":"initialize", "hooks":{}}}
print('{"type":"control_response","response":{"request_id":"unrelated","subtype":"error"}}', flush=True)
print(json.dumps(json.load(open(sys.argv[1]))), flush=True)
rest = sys.stdin.read()
assert not rest, 'Discovery must never send user messages'
open(sys.argv[2], 'w').write(json.dumps(request))
"#]).arg(fixture).arg(&seen);
        let catalog = query_catalog(command, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(catalog.models.len(), 11);
        assert!(
            seen.exists(),
            "discovery must close stdin and reap the child"
        );
        assert!(discovery_arguments().contains(&"--no-session-persistence"));
        assert!(discovery_arguments().contains(&"--safe-mode"));
    }

    #[tokio::test]
    async fn a_context_check_asks_only_for_context_and_reads_the_real_shape() {
        // The shape and figures are a live 2.1.283 answer for a resumed
        // session. The check must never send a user message: a prompt would be
        // a turn, and a turn costs.
        let temp = tempfile::tempdir().unwrap();
        let seen = temp.path().join("received.json");
        let mut command = Command::new("python3");
        command.args(["-u", "-c", r#"
import json, sys
init = json.loads(sys.stdin.readline())
assert init["request"] == {"subtype":"initialize", "hooks":{}}, init
ask = json.loads(sys.stdin.readline())
assert ask == {"type":"control_request", "request_id":"flowx-context-usage", "request":{"subtype":"get_context_usage", "detail":"summary"}}, ask
# A resumed session first replays tasks a killed process left behind.
print(json.dumps({"type":"system","subtype":"task_notification","status":"stopped"}), flush=True)
# And a CLI may print a line that is not JSON at all, which is not the answer.
print("A newer version of Claude Code is available.", flush=True)
print(json.dumps({"type":"result","subtype":"success","num_turns":0}), flush=True)
print(json.dumps({"type":"control_response","response":{"subtype":"success","request_id":"flowx-model-discovery","response":{"models":[]}}}), flush=True)
print(json.dumps({"type":"control_response","response":{"subtype":"success","request_id":"flowx-context-usage","response":{"totalTokens":88443,"maxTokens":1000000,"rawMaxTokens":1000000,"percentage":9,"model":"claude-opus-5-5[1m]"}}}), flush=True)
rest = sys.stdin.read()
assert not rest, 'A context check must never send user messages'
open(sys.argv[1], 'w').write('closed')
"#]).arg(&seen);
        let context = query_context(command, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(
            context,
            SessionContext {
                used: 88_443,
                window: 1_000_000,
                model: Some("claude-opus-5-5[1m]".into()),
            }
        );
        assert!(
            seen.exists(),
            "the check must close stdin and reap the child"
        );
        assert!(parse_context(&json!({"totalTokens": 10})).is_err());
        assert!(parse_context(&json!({"totalTokens": 10, "maxTokens": 0})).is_err());
    }

    #[tokio::test]
    async fn an_unresponsive_catalog_process_is_killed_on_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let pid = temp.path().join("pid");
        let mut command = Command::new("python3");
        command.args(["-u", "-c", "import os,sys,time; open(sys.argv[1], 'w').write(str(os.getpid())); sys.stdin.readline(); time.sleep(30)"]).arg(&pid);
        let error = query_catalog(command, Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(error.contains("timed out"));
        let status = std::process::Command::new("kill")
            .args(["-0", std::fs::read_to_string(pid).unwrap().trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(
            !status.success(),
            "timed-out discovery must not leave a child process"
        );
    }

    #[test]
    fn absent_or_explicitly_unsupported_effort_is_not_invented() {
        let catalog = parse_models(&json!({"models": [
            {"value":"new-model", "supportsEffort":true},
            {"value":"no-effort", "supportsEffort":false, "supportedEffortLevels":["high"]},
            {"value":"new-model", "displayName":"duplicate"},
            {"displayName":"missing id"}
        ]}))
        .unwrap();
        assert_eq!(catalog.models.len(), 2);
        assert!(
            catalog
                .models
                .iter()
                .all(|model| model.supported_efforts.is_empty())
        );
        assert!(parse_models(&json!({"models":[]})).is_err());
        assert!(parse_models(&json!({})).is_err());
    }

    #[tokio::test]
    #[ignore = "reads the installed authenticated CLI's metadata; sends no user turn"]
    async fn installed_claude_returns_a_live_catalog_without_a_turn() {
        let catalog = discover_models(&RuntimeDiscovery::default()).await.unwrap();
        assert!(!catalog.models.is_empty());
        for model in catalog.models {
            println!(
                "{}: {} — {:?}",
                model.id, model.display_name, model.supported_efforts
            );
        }
    }
}
