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

async fn query_catalog(mut command: Command, timeout: Duration) -> Result<ModelCatalog, String> {
    // Safe mode disables custom hooks/plugins; strict MCP and empty tools keep
    // discovery metadata-only. No `user` frame is ever written to this process.
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("Claude model discovery could not start: {error}"))?;
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
        let mut request = json!({
            "type": "control_request",
            "request_id": REQUEST_ID,
            "request": {"subtype": "initialize", "hooks": {}}
        })
        .to_string();
        request.push('\n');
        stdin
            .write_all(request.as_bytes())
            .await
            .map_err(|error| error.to_string())?;
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
                return Err("Claude exited before returning its model catalog.".to_string());
            }
            received += count;
            if received > RESPONSE_LIMIT {
                return Err("Claude model discovery exceeded its response limit.".to_string());
            }
            for line in framer
                .push(&chunk[..count])
                .map_err(|error| error.to_string())?
            {
                let frame: Value =
                    serde_json::from_str(&line).map_err(|error| error.to_string())?;
                if frame["type"] != "control_response"
                    || frame["response"]["request_id"] != REQUEST_ID
                {
                    continue;
                }
                if frame["response"]["subtype"] != "success" {
                    return Err("Claude rejected its model discovery request.".to_string());
                }
                return parse_models(&frame["response"]["response"]);
            }
        }
    })
    .await
    .unwrap_or_else(|_| Err("Claude model discovery timed out.".to_string()));

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
