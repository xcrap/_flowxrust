//! Translating FlowX's vocabulary into Codex's.
//!
//! Everything here is pure: given a request, produce the JSON Codex expects,
//! and given a notification, produce a [`ProviderEvent`]. That split is why the
//! protocol can be tested against real captured payloads without spawning a
//! process.

use fx_core::AgentAccess;
use serde_json::{Value, json};

use crate::conversation::state::TRIVIAL_TOOL_RESULT;
use crate::provider::{
    ApprovalRequest, Lifecycle, Model, ProviderEvent, Question, RiskLevel, Usage,
};

/// Codex's `approvalPolicy` and `sandbox` for a FlowX access level.
///
/// These three pairings are the whole point of the access control. They are
/// worth pinning: a change here silently alters what the agent may do.
pub fn access_params(access: AgentAccess) -> (&'static str, &'static str) {
    match access {
        AgentAccess::Supervised => ("untrusted", "workspace-write"),
        AgentAccess::AcceptEdits => ("on-request", "workspace-write"),
        AgentAccess::FullAccess => ("never", "danger-full-access"),
    }
}

/// The structured `sandboxPolicy` `turn/start` expects.
///
/// Unlike the string form on `thread/start`, this is a nested object that also
/// names the writable roots.
pub fn sandbox_policy(access: AgentAccess, working_directory: &str) -> Value {
    match access {
        AgentAccess::FullAccess => json!({ "type": "dangerFullAccess" }),
        _ => json!({
            "type": "workspaceWrite",
            "networkAccess": false,
            "writableRoots": [working_directory],
        }),
    }
}

/// One image attachment, as Codex's `localImage` input.
pub fn image_input(path: &str) -> Value {
    json!({ "type": "localImage", "path": path })
}

/// The `input` array for a turn.
///
/// **Order matters and is asserted by tests:** images come first and the text
/// block comes last. Codex treats the final text item as the prompt, so a
/// trailing image would be read as the user's words.
pub fn turn_input(prompt: &str, image_paths: &[String]) -> Vec<Value> {
    let mut input: Vec<Value> = image_paths.iter().map(|p| image_input(p)).collect();
    input.push(json!({ "type": "text", "text": prompt }));
    input
}

/// Parameters for `thread/start` or `thread/resume`.
pub fn thread_params(
    thread_id: Option<&str>,
    access: AgentAccess,
    system_prompt: Option<&str>,
    working_directory: &str,
) -> Value {
    let (approval, sandbox) = access_params(access);
    let mut params = json!({
        "approvalPolicy": approval,
        "sandbox": sandbox,
        "cwd": working_directory,
    });
    if let Some(id) = thread_id {
        params["threadId"] = json!(id);
    }
    if let Some(prompt) = system_prompt.filter(|p| !p.is_empty()) {
        params["developerInstructions"] = json!(prompt);
    }
    params
}

/// Everything one turn needs.
#[derive(Debug, Clone)]
pub struct TurnRequest<'a> {
    pub thread_id: &'a str,
    pub prompt: &'a str,
    /// Prepared image paths, in attachment order.
    pub image_paths: &'a [String],
    pub model: &'a str,
    pub effort: &'a str,
    pub access: AgentAccess,
    pub working_directory: &'a str,
}

impl<'a> TurnRequest<'a> {
    pub fn new(thread_id: &'a str, prompt: &'a str, working_directory: &'a str) -> Self {
        Self {
            thread_id,
            prompt,
            image_paths: &[],
            model: "gpt-5.6-sol",
            effort: "medium",
            access: AgentAccess::FullAccess,
            working_directory,
        }
    }

    pub fn images(mut self, paths: &'a &[String]) -> Self {
        self.image_paths = paths;
        self
    }

    pub fn model(mut self, model: &'a str) -> Self {
        self.model = model;
        self
    }

    pub fn effort(mut self, effort: &'a str) -> Self {
        self.effort = effort;
        self
    }

    pub fn access(mut self, access: AgentAccess) -> Self {
        self.access = access;
        self
    }
}

/// Parameters for `turn/start`.
pub fn turn_params(request: &TurnRequest<'_>) -> Value {
    let TurnRequest {
        thread_id,
        prompt,
        image_paths,
        model,
        effort,
        access,
        working_directory,
    } = *request;
    let (approval, _) = access_params(access);
    let mut params = json!({
        "threadId": thread_id,
        "input": turn_input(prompt, image_paths),
        "approvalPolicy": approval,
        "sandboxPolicy": sandbox_policy(access, working_directory),
        "model": model,
        "cwd": working_directory,
        // Codex restores a saved collaboration mode when resuming a thread.
        // Always select its normal workflow so older FlowX planning sessions
        // cannot retain their previous restrictions after the UI mode is gone.
        "collaborationMode": {
            "mode": "default",
            "settings": {
                "model": model,
                "reasoning_effort": if effort.is_empty() { Value::Null } else { json!(effort) },
                "developer_instructions": Value::Null,
            },
        },
    });

    if !effort.is_empty() {
        params["effort"] = json!(effort);
    }
    params
}

/// Parameters for `turn/steer`.
pub fn steer_params(
    thread_id: &str,
    expected_turn_id: &str,
    prompt: &str,
    image_paths: &[String],
) -> Value {
    json!({
        "threadId": thread_id,
        "expectedTurnId": expected_turn_id,
        "input": turn_input(prompt, image_paths),
    })
}

/// Parameters for `thread/goal/set`.
///
/// Either half may be absent: `/goal pause` changes only the status, and an
/// objective set from the composer is always made active.
pub fn goal_set_params(
    thread_id: &str,
    objective: Option<&str>,
    status: Option<fx_core::models::goal::GoalStatus>,
) -> Value {
    let mut params = json!({ "threadId": thread_id });
    if let Some(objective) = objective {
        params["objective"] = json!(objective);
    }
    if let Some(status) = status {
        params["status"] = json!(status.as_str());
    }
    params
}

/// Parameters for `thread/list`.
pub fn list_threads_params(
    working_directories: &[String],
    limit: u64,
    archived: bool,
    indexed_only: bool,
    cursor: Option<&str>,
) -> Value {
    let mut params = json!({
        "cwd": if working_directories.len() == 1 {
            Value::String(working_directories[0].clone())
        } else {
            json!(working_directories)
        },
        "limit": limit.clamp(1, 100),
        "sortKey": "recency_at",
        "sortDirection": "desc",
        "useStateDbOnly": indexed_only,
        "archived": archived,
    });
    if let Some(cursor) = cursor {
        params["cursor"] = json!(cursor);
    }
    params
}

/// Parse one `codex debug models` payload.
///
/// Missing fields fall back to `fallback`, because an older CLI emits less.
pub fn parse_model_catalog(payload: &Value, fallback: &[Model]) -> Vec<Model> {
    let Some(models) = payload.get("models").and_then(Value::as_array) else {
        return fallback.to_vec();
    };

    let mut out: Vec<Model> = models
        .iter()
        .filter(|m| {
            // Codex tags visibility; only listed models are offered.
            m.get("visibility").and_then(Value::as_str) == Some("list")
        })
        .map(|m| {
            let id = m
                .get("slug")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let known = fallback.iter().find(|f| f.id == id);
            let supported_efforts = m
                .get("supported_reasoning_levels")
                .and_then(Value::as_array)
                .map(|levels| {
                    levels
                        .iter()
                        .filter_map(|l| l.get("effort").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| {
                    known
                        .map(|f| f.supported_efforts.clone())
                        .unwrap_or_default()
                });
            let supports_vision = m
                .get("input_modalities")
                .and_then(Value::as_array)
                .is_some_and(|v| v.iter().any(|s| s.as_str() == Some("image")));

            Model {
                id: id.clone(),
                display_name: m
                    .get("display_name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| known.map(|f| f.display_name.clone()))
                    .unwrap_or_else(|| id.clone()),
                context_window: m
                    .get("context_window")
                    .and_then(Value::as_u64)
                    .or_else(|| known.map(|f| f.context_window))
                    .unwrap_or(0),
                max_context_window: m
                    .get("max_context_window")
                    .and_then(Value::as_u64)
                    .or_else(|| known.map(|f| f.max_context_window))
                    .unwrap_or(0),
                supported_efforts,
                default_effort: m
                    .get("default_reasoning_level")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| known.and_then(|f| f.default_effort.clone())),
                supports_vision,
                is_default: false,
            }
        })
        .collect();

    if let Some(first) = out.first_mut() {
        first.is_default = true;
    }
    out
}

/// The app-server `model/list` response (distinct from the older CLI catalog).
/// Account-provided effort choices and defaults are authoritative.
pub fn parse_model_list(payload: &Value, fallback: &[Model]) -> Vec<Model> {
    payload
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|model| model.get("hidden").and_then(Value::as_bool) != Some(true))
        .filter_map(|model| {
            let id = model
                .get("model")
                .or_else(|| model.get("id"))
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())?;
            let known = fallback.iter().find(|known| known.id == id);
            Some(Model {
                id: id.into(),
                display_name: model
                    .get("displayName")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .into(),
                context_window: model
                    .get("contextWindow")
                    .and_then(Value::as_u64)
                    .or_else(|| known.map(|m| m.context_window))
                    .unwrap_or_default(),
                max_context_window: model
                    .get("maxContextWindow")
                    .and_then(Value::as_u64)
                    .or_else(|| known.map(|m| m.max_context_window))
                    .unwrap_or_default(),
                supported_efforts: model
                    .get("supportedReasoningEfforts")
                    .and_then(Value::as_array)
                    .map(|efforts| {
                        efforts
                            .iter()
                            .filter_map(|effort| {
                                effort.get("reasoningEffort").and_then(Value::as_str)
                            })
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_else(|| {
                        known
                            .map(|m| m.supported_efforts.clone())
                            .unwrap_or_default()
                    }),
                default_effort: model
                    .get("defaultReasoningEffort")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| known.and_then(|m| m.default_effort.clone())),
                // Older app-server catalogs omit modalities; its documented
                // compatibility default is text + image.
                supports_vision: model
                    .get("inputModalities")
                    .and_then(Value::as_array)
                    .is_none_or(|modes| modes.iter().any(|mode| mode.as_str() == Some("image"))),
                is_default: model
                    .get("isDefault")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

/// The models Codex offers when discovery fails.
///
/// Kept as a fallback rather than as the catalog itself: the running CLI is
/// authoritative, and its list changes with the installed build.
pub fn fallback_models() -> Vec<Model> {
    fn model(
        id: &str,
        name: &str,
        context: u64,
        max_context: u64,
        default_effort: Option<&str>,
        is_default: bool,
    ) -> Model {
        Model {
            id: id.into(),
            display_name: name.into(),
            context_window: context,
            max_context_window: max_context,
            supported_efforts: ["low", "medium", "high", "xhigh", "max", "ultra"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            default_effort: default_effort.map(str::to_string),
            supports_vision: true,
            is_default,
        }
    }
    vec![
        model(
            "gpt-6-astra",
            "GPT-6 Astra",
            272_000,
            872_000,
            Some("medium"),
            false,
        ),
        model(
            "gpt-5.6-sol",
            "GPT-5.6 Sol",
            272_000,
            272_000,
            Some("low"),
            true,
        ),
        model(
            "gpt-5.6-terra",
            "GPT-5.6 Terra",
            272_000,
            272_000,
            Some("medium"),
            false,
        ),
        model(
            "gpt-5.6-luna",
            "GPT-5.6 Luna",
            272_000,
            272_000,
            Some("medium"),
            false,
        ),
    ]
}

/// Item types that carry no user-visible content and are skipped.
fn is_ignored_item(item_type: &str) -> bool {
    matches!(
        item_type,
        "reasoning"
            | "plan"
            | "contextCompaction"
            | "hookPrompt"
            | "subAgentActivity"
            | "enteredReviewMode"
            | "exitedReviewMode"
    )
}

/// Whether an item type is a tool invocation.
fn is_tool_item(item_type: &str) -> bool {
    matches!(
        item_type,
        "toolCall"
            | "commandExecution"
            | "fileChange"
            | "mcpToolCall"
            | "dynamicToolCall"
            | "collabAgentToolCall"
            | "webSearch"
            | "imageView"
            | "imageGeneration"
            | "sleep"
    )
}

/// Risk classification for a tool call, used to decide how an approval reads.
///
/// `command` is the call's command in whichever form Codex sent it: a string
/// for a shell line, or an argv array.
pub fn risk_for(item_type: &str, command: Option<&Value>) -> RiskLevel {
    match item_type {
        "webSearch" | "imageView" | "sleep" => RiskLevel::Safe,
        "commandExecution" => {
            // A shell command can do anything; whether this one does is a
            // judgement the user makes, so it is surfaced as risky.
            match command {
                Some(command) if is_read_only_command_value(command) => RiskLevel::Moderate,
                _ => RiskLevel::Dangerous,
            }
        }
        "fileChange" | "collabAgentToolCall" | "imageGeneration" => RiskLevel::Moderate,
        _ => RiskLevel::Moderate,
    }
}

/// The read-only check, shared with Claude so the two cannot disagree.
pub use crate::shell::is_read_only_command;

/// Whether a command only reads, in either of the two forms Codex sends.
///
/// An argv array is classified as argv, never joined into a line and split
/// again: the join moves a word's boundaries wherever one element holds a
/// quote, which is a way to hide `-delete` inside what reads as one word.
pub fn is_read_only_command_value(command: &Value) -> bool {
    match command {
        Value::String(text) => is_read_only_command(text),
        Value::Array(words) => words
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()
            .is_some_and(|argv| crate::shell::is_read_only_argv(&argv)),
        _ => false,
    }
}

/// What a failed tool that said nothing is recorded as.
pub(super) const TOOL_FAILED: &str = "Tool failed";

/// What a completed tool item produced, and whether it failed.
///
/// Shared by live items and native history, so a turn reads the same while it
/// streams as after a restart. The key order is the server's, not a guess:
/// `aggregatedOutput` is what Codex actually sends, and reading only
/// `output`/`result` yields an empty result for every real tool call — which
/// then looks trivial and is dropped, so the transcript silently loses every
/// tool output.
///
/// An explicit `null` is not output. Codex sends `"error": null` on every
/// successful MCP call; reading the key's presence marked each one failed, and
/// taking the first present key showed the literal text `null`.
pub(super) fn tool_outcome(item: &Value) -> (Option<String>, bool) {
    let output = [
        "aggregatedOutput",
        "output",
        "result",
        "error",
        "contentItems",
        "changes",
        "results",
        "agentsStates",
    ]
    .iter()
    .filter_map(|key| item.get(*key))
    .filter(|value| !value.is_null())
    .map(|value| {
        value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string())
    })
    .find(|text| !text.trim().is_empty());
    let is_error = item["status"] == "failed"
        || item["status"] == "declined"
        || item["success"] == false
        || item["is_error"] == true
        || item["exitCode"].as_i64().is_some_and(|code| code != 0)
        || !item["error"].is_null();
    (output, is_error)
}

/// Preserve the tool arguments shared by live items and native history.
///
/// Command items carry their command at the top level, whereas MCP and dynamic
/// calls carry `arguments`. The transcript reads a JSON `command` string; argv
/// arrays additionally retain their exact boundaries in `command_argv`.
pub(super) fn tool_input(item: &Value) -> Value {
    let mut input = item
        .get("input")
        .filter(|value| !value.is_null())
        .or_else(|| item.get("arguments").filter(|value| !value.is_null()))
        .cloned()
        .unwrap_or_else(|| {
            let mut input = item.clone();
            if let Some(map) = input.as_object_mut() {
                for key in [
                    "aggregatedOutput",
                    "output",
                    "result",
                    "error",
                    "contentItems",
                ] {
                    map.remove(key);
                }
            }
            input
        });
    // Some protocol versions serialize the input object as a JSON string.
    if let Some(raw) = input.as_str()
        && let Ok(parsed @ Value::Object(_)) = serde_json::from_str::<Value>(raw)
    {
        input = parsed;
    }
    let command = item
        .get("command")
        .filter(|value| !value.is_null())
        .or_else(|| input.get("command"))
        .cloned();
    let display = match command.as_ref() {
        Some(Value::String(command)) => Some(command.clone()),
        Some(Value::Array(words)) => words
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()
            .map(|words| words.join(" ")),
        _ => None,
    };
    if let Some(display) = display {
        if !input.is_object() {
            input = json!({ "input": input });
        }
        input["command"] = Value::String(display);
        if matches!(command, Some(Value::Array(_))) {
            input["command_argv"] = command.unwrap();
        }
        if let Some(cwd) = item.get("cwd") {
            input["cwd"] = cwd.clone();
        }
    }
    input
}

/// Translate one notification into zero or more events.
///
/// Returns a list because several Codex notifications map to more than one
/// event — `item/completed` can be a tool result *and* a phase change.
pub fn parse_notification(method: &str, params: &Value) -> Vec<ProviderEvent> {
    match method {
        "turn/started" => vec![ProviderEvent::Lifecycle(Lifecycle::TurnStarted)],

        "item/agentMessage/delta" => params
            .get("delta")
            .and_then(Value::as_str)
            .map(|d| vec![ProviderEvent::TextDelta(d.to_string())])
            .unwrap_or_default(),

        "item/started" | "item/completed" => {
            let Some(item) = params.get("item") else {
                return Vec::new();
            };
            let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
            if is_ignored_item(item_type) {
                // A compaction item is the one exception worth surfacing, and
                // its completion has to end the phase its start began: mapping
                // both to `Compacting` left a finished compaction on screen for
                // the rest of the turn.
                return match (item_type, method) {
                    ("contextCompaction", "item/started") => {
                        vec![ProviderEvent::Lifecycle(Lifecycle::Compacting)]
                    }
                    ("contextCompaction", _) => {
                        vec![ProviderEvent::Lifecycle(Lifecycle::Compacted)]
                    }
                    _ => Vec::new(),
                };
            }
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            // A tool item usually names itself in `name`, but a bare
            // `commandExecution` does not. Falling back to the type keeps the
            // row identifiable instead of blank.
            let name = item
                .get("name")
                .or_else(|| item.get("tool"))
                .and_then(Value::as_str)
                .unwrap_or(item_type)
                .to_string();

            if is_tool_item(item_type) {
                if method == "item/started" {
                    let input = tool_input(item);
                    return vec![ProviderEvent::ToolUse {
                        id,
                        name,
                        input: input
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| input.to_string()),
                    }];
                }
                // Completed: the output is the result.
                let (output, is_error) = tool_outcome(item);
                let content = output.unwrap_or_else(|| {
                    if is_error {
                        TOOL_FAILED.to_string()
                    } else {
                        // Defaulting to `Completed` is what lets the conversation's
                        // trivial-result rule recognise a tool that said nothing.
                        TRIVIAL_TOOL_RESULT.to_string()
                    }
                });
                return vec![ProviderEvent::ToolResult {
                    id,
                    content,
                    is_error,
                }];
            }

            if method == "item/completed"
                && let Some(text) = item
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
            {
                return vec![ProviderEvent::Text(text.to_string())];
            }
            Vec::new()
        }

        "item/tool/call"
        | "item/commandExecution/requestApproval"
        | "item/fileChange/requestApproval" => {
            vec![ProviderEvent::Approval(parse_approval(method, params))]
        }

        "item/permissions/requestApproval" => {
            vec![ProviderEvent::Approval(parse_approval(method, params))]
        }

        "item/tool/requestUserInput" => {
            vec![ProviderEvent::UserInput(parse_user_input(params))]
        }

        "model/rerouted" => {
            let model = params
                .get("toModel")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let reason = params
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default();
            vec![ProviderEvent::ModelChanged {
                model: model.to_string(),
                reason: reason.to_string(),
            }]
        }

        "error" => {
            let message = params
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .or_else(|| params.get("message").and_then(Value::as_str))
                .unwrap_or("Codex reported an error.");
            if params.get("willRetry").and_then(Value::as_bool) == Some(true) {
                vec![ProviderEvent::Lifecycle(Lifecycle::Preparing)]
            } else {
                vec![ProviderEvent::Error(message.to_string())]
            }
        }

        "thread/tokenUsage/updated" => {
            let usage = params.get("tokenUsage");
            let Some(usage) = usage else {
                return Vec::new();
            };
            // `total` is the thread's running count and `last` the most recent
            // request's. The conversation's totals come from `total`, as they
            // did in Swift: adding `last` on every update counted each request
            // again for as long as the thread was open. `last` is what fills
            // the context window. Fields carry a `Tokens` suffix.
            let read = |block: &Value| {
                let count = |key: &str| block.get(key).and_then(Value::as_u64).unwrap_or(0);
                Usage {
                    input_tokens: count("inputTokens"),
                    output_tokens: count("outputTokens"),
                    cached_input_tokens: count("cachedInputTokens"),
                    reasoning_output_tokens: count("reasoningOutputTokens"),
                    total_tokens: count("totalTokens"),
                }
            };
            let mut events: Vec<ProviderEvent> = usage
                .get("total")
                .filter(|total| total.is_object())
                .map(|total| ProviderEvent::CumulativeUsage(read(total)))
                .into_iter()
                .collect();
            let window = usage.get("modelContextWindow").and_then(Value::as_u64);
            if let Some(window) = window.filter(|w| *w > 0) {
                events.push(ProviderEvent::ContextUsage {
                    used: read(usage.get("last").unwrap_or(&Value::Null)).total_tokens,
                    window,
                });
            }
            events
        }

        "thread/compacted" => vec![ProviderEvent::Lifecycle(Lifecycle::Compacted)],

        "thread/status/changed" => {
            let status = params.get("status").unwrap_or(params);
            let kind = status
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if kind == "systemError" {
                vec![ProviderEvent::Error(
                    status
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("Codex reported a system error.")
                        .to_string(),
                )]
            } else {
                compaction_status(params)
                    .map(ProviderEvent::Lifecycle)
                    .into_iter()
                    .collect()
            }
        }

        "turn/completed" => {
            let turn = params.get("turn").unwrap_or(params);
            let status = turn
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("completed");
            if status == "failed" {
                let message = turn
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("The turn failed.");
                vec![ProviderEvent::Error(message.to_string())]
            } else {
                vec![ProviderEvent::Done {
                    stop_reason: status.to_string(),
                }]
            }
        }

        _ => Vec::new(),
    }
}

/// Whether a thread status reports compaction starting or finishing.
///
/// The status has arrived as a bare string, as `{type|state|status}`, and on
/// the thread itself, depending on the build. Swift read all of them, and so
/// does this.
fn compaction_status(params: &Value) -> Option<Lifecycle> {
    let normalize = |value: &str| {
        let value = value.trim().to_ascii_lowercase();
        if value.contains("compacting") {
            Some(Lifecycle::Compacting)
        } else if value.contains("compacted") || value == "compact_boundary" {
            Some(Lifecycle::Compacted)
        } else {
            None
        }
    };
    let read = |value: &Value, keys: &[&str]| {
        keys.iter()
            .filter_map(|key| value.get(*key).and_then(Value::as_str))
            .find_map(normalize)
    };
    match params.get("status") {
        Some(Value::String(status)) => normalize(status),
        Some(status @ Value::Object(_)) => read(status, &["type", "state", "status"]),
        _ => None,
    }
    .or_else(|| {
        params
            .get("thread")
            .and_then(|thread| read(thread, &["state", "status"]))
    })
    .or_else(|| read(params, &["state", "type"]))
}

/// The stateful half of reading one turn's notifications.
///
/// [`parse_notification`] reads each notification on its own, and on its own a
/// completed `agentMessage` cannot say whether its text already streamed. Swift
/// remembered which item ids had streamed, and so does this. A completed item
/// that streamed ends its message, so the next one starts a new row; one that
/// did not stream arrives whole as [`ProviderEvent::Text`].
#[derive(Debug, Default)]
pub struct TurnNotifications {
    streamed: std::collections::HashSet<String>,
    /// A delta arrived with no item id, so the next completed message is the
    /// one it belonged to.
    streamed_without_id: bool,
}

impl TurnNotifications {
    pub fn new() -> Self {
        Self::default()
    }

    /// Translate one notification, given what this turn has already seen.
    pub fn parse(&mut self, method: &str, params: &Value) -> Vec<ProviderEvent> {
        let events = parse_notification(method, params);
        match method {
            "item/agentMessage/delta" if !events.is_empty() => {
                match params.get("itemId").and_then(Value::as_str) {
                    Some(id) => {
                        self.streamed.insert(id.to_owned());
                    }
                    None => self.streamed_without_id = true,
                }
            }
            "item/completed"
                if params.pointer("/item/type").and_then(Value::as_str) == Some("agentMessage") =>
            {
                let id = params
                    .pointer("/item/id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let streamed = self.streamed.remove(id);
                if std::mem::take(&mut self.streamed_without_id) || streamed {
                    return vec![ProviderEvent::TextEnd];
                }
            }
            _ => {}
        }
        events
    }
}

fn parse_approval(method: &str, params: &Value) -> ApprovalRequest {
    let item = params.get("item").unwrap_or(params);
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
    let command = item
        .get("command")
        .filter(|command| !command.is_null())
        .or_else(|| item.get("input").and_then(|i| i.get("command")));
    ApprovalRequest {
        id: item
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        tool_name: if method.contains("commandExecution") {
            "shell".into()
        } else if method.contains("fileChange") {
            "edit".into()
        } else {
            item.get("name")
                .and_then(Value::as_str)
                .unwrap_or(item_type)
                .to_string()
        },
        input: item.get("input").cloned().unwrap_or(Value::Null),
        risk: risk_for(item_type, command),
    }
}

pub(crate) fn parse_user_input(params: &Value) -> crate::provider::UserInputRequest {
    let questions = params
        .get("questions")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .take(crate::provider::MAX_QUESTIONS)
                .map(|q| Question {
                    id: q
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    header: q
                        .get("header")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    question: q
                        .get("question")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    options: q
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|o| {
                            o.iter()
                                .take(crate::provider::MAX_OPTIONS)
                                .map(|opt| crate::provider::Option_ {
                                    label: opt
                                        .get("label")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_string(),
                                    description: opt
                                        .get("description")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_string(),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    is_other: q.get("isOther").and_then(Value::as_bool).unwrap_or(false),
                    is_secret: q.get("isSecret").and_then(Value::as_bool).unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default();

    crate::provider::UserInputRequest {
        id: params
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        questions,
    }
}

/// Extract the thread id from a `thread/start` or `thread/resume` result.
pub fn thread_id_from_result(result: &Value) -> Option<String> {
    result
        .get("thread")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Read a list of threads out of a `thread/list` result.
pub fn threads_from_result(result: &Value) -> Vec<Value> {
    result
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// The next page cursor, if the list was truncated.
pub fn next_cursor(result: &Value) -> Option<String> {
    result
        .get("nextCursor")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Mark a sentence as one Codex signals as a writer conflict.
///
/// Codex permits one writer per task. When another Codex session owns a task,
/// FlowX can still display the transcript but must not send to it, so the
/// refusal is detected and reported as its own condition rather than as a
/// generic failure.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "This task is open in another Codex session. FlowX can show its updates, but your message was not sent. \
     Finish any running work and quit the other Codex app or session, then retry your message here."
)]
pub struct CodexWriterConflict;

impl CodexWriterConflict {
    pub const MESSAGE: &'static str = "This task is open in another Codex session. FlowX can show its updates, but your message was not sent. \
         Finish any running work and quit the other Codex app or session, then retry your message here.";

    /// Whether a server error message describes a writer conflict.
    pub fn matches(message: &str) -> bool {
        message == Self::MESSAGE
            || message
                .to_ascii_lowercase()
                .contains("already has an active writer")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_access_level_maps_to_a_distinct_policy_pair() {
        let supervised = access_params(AgentAccess::Supervised);
        let accept = access_params(AgentAccess::AcceptEdits);
        let full = access_params(AgentAccess::FullAccess);

        assert_eq!(supervised, ("untrusted", "workspace-write"));
        assert_eq!(accept, ("on-request", "workspace-write"));
        assert_eq!(full, ("never", "danger-full-access"));
    }

    #[test]
    fn the_two_lesser_access_levels_share_a_sandbox() {
        // Only the approval policy differs between them, which is the intent:
        // one asks every time, the other asks about edits.
        assert_eq!(
            access_params(AgentAccess::Supervised).1,
            access_params(AgentAccess::AcceptEdits).1
        );
        assert_ne!(
            access_params(AgentAccess::Supervised).0,
            access_params(AgentAccess::AcceptEdits).0
        );
    }

    #[test]
    fn the_structured_sandbox_names_the_writable_root() {
        let policy = sandbox_policy(AgentAccess::Supervised, "/Users/x/code");
        assert_eq!(policy["type"], "workspaceWrite");
        assert_eq!(policy["networkAccess"], false);
        assert_eq!(policy["writableRoots"][0], "/Users/x/code");
    }

    #[test]
    fn full_access_has_no_writable_roots() {
        let policy = sandbox_policy(AgentAccess::FullAccess, "/Users/x/code");
        assert_eq!(policy["type"], "dangerFullAccess");
        assert!(policy.get("writableRoots").is_none(), "{policy}");
    }

    #[test]
    fn images_come_before_the_text_so_the_prompt_is_read_correctly() {
        // Codex treats the last text item as the prompt. A trailing image would
        // become the user's words.
        let images = vec!["/tmp/a.png".to_string(), "/tmp/b.png".to_string()];
        let input = turn_input("fix the parser", &images);
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["type"], "localImage");
        assert_eq!(input[0]["path"], "/tmp/a.png");
        assert_eq!(input[1]["type"], "localImage");
        assert_eq!(
            input.last().unwrap()["type"],
            "text",
            "the prompt must be the final input item"
        );
        assert_eq!(input.last().unwrap()["text"], "fix the parser");
    }

    #[test]
    fn a_prompt_with_no_images_is_just_the_text() {
        let input = turn_input("hello", &[]);
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "text");
    }

    #[test]
    fn a_goal_update_names_only_what_changed() {
        use fx_core::models::goal::GoalStatus;
        assert_eq!(
            goal_set_params("t1", None, Some(GoalStatus::Paused)),
            json!({ "threadId": "t1", "status": "paused" })
        );
        assert_eq!(
            goal_set_params("t1", Some("Ship it"), Some(GoalStatus::Active)),
            json!({ "threadId": "t1", "objective": "Ship it", "status": "active" })
        );
    }

    #[test]
    fn steering_carries_the_turn_it_expects() {
        // Codex rejects a steer aimed at a turn that has already finished.
        let params = steer_params("t1", "turn-9", "actually, use zsh", &[]);
        assert_eq!(params["expectedTurnId"], "turn-9");
        assert_eq!(params["threadId"], "t1");
        assert_eq!(params["input"][0]["text"], "actually, use zsh");
    }

    #[test]
    fn starting_a_thread_omits_the_id_and_resuming_includes_it() {
        let start = thread_params(None, AgentAccess::FullAccess, None, "/w");
        assert!(start.get("threadId").is_none(), "{start}");
        let resume = thread_params(Some("t1"), AgentAccess::Supervised, None, "/w");
        assert_eq!(resume["threadId"], "t1");
        assert_eq!(resume["approvalPolicy"], "untrusted");
    }

    #[test]
    fn a_system_prompt_becomes_a_developer_instruction() {
        let params = thread_params(None, AgentAccess::FullAccess, Some("be terse"), "/w");
        assert_eq!(params["developerInstructions"], "be terse");
        // An empty prompt must not send an empty instruction.
        let blank = thread_params(None, AgentAccess::FullAccess, Some(""), "/w");
        assert!(blank.get("developerInstructions").is_none());
    }

    #[test]
    fn turns_preserve_access_without_injecting_workflow_instructions() {
        for (access, approval, sandbox) in [
            (AgentAccess::Supervised, "untrusted", "workspaceWrite"),
            (AgentAccess::AcceptEdits, "on-request", "workspaceWrite"),
            (AgentAccess::FullAccess, "never", "dangerFullAccess"),
        ] {
            let params = turn_params(&TurnRequest::new("t1", "do it", "/w").access(access));
            assert_eq!(params["collaborationMode"]["mode"], "default");
            assert!(params["collaborationMode"]["settings"]["developer_instructions"].is_null());
            assert!(params.get("additionalContext").is_none(), "{params}");
            assert_eq!(params["input"], json!([{"type":"text", "text":"do it"}]));
            assert_eq!(params["approvalPolicy"], approval);
            assert_eq!(params["sandboxPolicy"]["type"], sandbox);
        }
    }

    #[test]
    fn listing_clamps_the_limit_to_the_documented_range() {
        assert_eq!(
            list_threads_params(&["/w".into()], 0, false, false, None)["limit"],
            1
        );
        assert_eq!(
            list_threads_params(&["/w".into()], 5000, false, false, None)["limit"],
            100
        );
        assert_eq!(
            list_threads_params(&["/w".into()], 25, false, false, None)["limit"],
            25
        );
    }

    #[test]
    fn listing_one_directory_sends_a_string_and_several_sends_an_array() {
        let one = list_threads_params(&["/a".into()], 10, false, false, None);
        assert!(one["cwd"].is_string(), "{one}");
        let many = list_threads_params(&["/a".into(), "/b".into()], 10, false, false, None);
        assert!(many["cwd"].is_array(), "{many}");
        assert_eq!(many["cwd"][1], "/b");
    }

    // ── Notifications ───────────────────────────────────────────────────────

    #[test]
    fn a_text_delta_becomes_a_delta_event() {
        let events = parse_notification(
            "item/agentMessage/delta",
            &json!({ "delta": "chunk", "itemId": "i1" }),
        );
        assert_eq!(events, vec![ProviderEvent::TextDelta("chunk".into())]);
    }

    #[test]
    fn a_delta_without_text_produces_nothing() {
        assert!(parse_notification("item/agentMessage/delta", &json!({})).is_empty());
    }

    #[test]
    fn an_unknown_notification_is_silently_ignored() {
        // Providers add methods over time; an unknown one must not break a turn.
        assert!(parse_notification("thread/something/new", &json!({"a": 1})).is_empty());
    }

    #[test]
    fn a_completed_text_item_becomes_text() {
        let events = parse_notification(
            "item/completed",
            &json!({ "item": { "type": "agentMessage", "text": "done" } }),
        );
        assert_eq!(events, vec![ProviderEvent::Text("done".into())]);
    }

    #[test]
    fn internal_items_are_not_surfaced() {
        for item_type in ["reasoning", "plan", "hookPrompt", "subAgentActivity"] {
            let events = parse_notification(
                "item/completed",
                &json!({ "item": { "type": item_type, "text": "internal" } }),
            );
            assert!(events.is_empty(), "{item_type} was surfaced");
        }
    }

    #[test]
    fn a_context_compaction_item_becomes_a_phase_change() {
        let events = parse_notification(
            "item/started",
            &json!({ "item": { "type": "contextCompaction" } }),
        );
        assert_eq!(
            events,
            vec![ProviderEvent::Lifecycle(Lifecycle::Compacting)]
        );
    }

    #[test]
    fn a_completed_compaction_item_ends_the_phase() {
        // Both ends used to map to `Compacting`, so the phase never left it.
        let events = parse_notification(
            "item/completed",
            &json!({ "item": { "type": "contextCompaction", "id": "c1" } }),
        );
        assert_eq!(events, vec![ProviderEvent::Lifecycle(Lifecycle::Compacted)]);
    }

    #[test]
    fn a_thread_status_reports_compaction_in_each_shape_codex_has_used() {
        for (params, expected) in [
            (json!({ "status": "compacting" }), Lifecycle::Compacting),
            (
                json!({ "status": { "type": "Compacted" } }),
                Lifecycle::Compacted,
            ),
            (
                json!({ "status": { "state": "compacting" } }),
                Lifecycle::Compacting,
            ),
            (
                json!({ "thread": { "status": "compacted" } }),
                Lifecycle::Compacted,
            ),
            (json!({ "state": "compact_boundary" }), Lifecycle::Compacted),
        ] {
            assert_eq!(
                parse_notification("thread/status/changed", &params),
                vec![ProviderEvent::Lifecycle(expected)],
                "{params}"
            );
        }
        assert!(
            parse_notification(
                "thread/status/changed",
                &json!({ "status": { "type": "idle" } })
            )
            .is_empty()
        );
    }

    #[test]
    fn consecutive_agent_messages_become_separate_rows() {
        // Two messages and no tool call between them: both streamed used to
        // merge into one row, and a second that did not stream was dropped.
        let mut turn = TurnNotifications::new();
        let mut conversation = crate::conversation::Conversation::new(uuid::Uuid::nil());
        for (method, params) in [
            (
                "item/agentMessage/delta",
                json!({ "itemId": "m1", "delta": "First." }),
            ),
            (
                "item/completed",
                json!({ "item": { "type": "agentMessage", "id": "m1", "text": "First." } }),
            ),
            (
                "item/agentMessage/delta",
                json!({ "itemId": "m2", "delta": "Second." }),
            ),
            (
                "item/completed",
                json!({ "item": { "type": "agentMessage", "id": "m2", "text": "Second." } }),
            ),
            (
                "item/completed",
                json!({ "item": { "type": "agentMessage", "id": "m3", "text": "Third." } }),
            ),
            ("item/agentMessage/delta", json!({ "delta": "Fourth." })),
            (
                "item/completed",
                json!({ "item": { "type": "agentMessage", "id": "m4", "text": "Fourth." } }),
            ),
        ] {
            for event in turn.parse(method, &params) {
                conversation.apply(event);
            }
        }
        let rows: Vec<String> = conversation
            .messages()
            .map(fx_core::ConversationMessage::text_content)
            .collect();
        assert_eq!(rows, ["First.", "Second.", "Third.", "Fourth."]);
        assert!(!conversation.is_streaming());
    }

    #[test]
    fn a_started_tool_item_becomes_a_tool_use() {
        let events = parse_notification(
            "item/started",
            &json!({
                "item": {
                    "type": "commandExecution",
                    "id": "c1",
                    "command": "ls",
                    "input": { "command": "ls" }
                }
            }),
        );
        match &events[0] {
            ProviderEvent::ToolUse { id, name, .. } => {
                assert_eq!(id, "c1");
                assert_eq!(
                    name, "commandExecution",
                    "a tool item with no name field must fall back to its type"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_real_shaped_started_command_keeps_its_command_and_working_directory() {
        // Same top-level fields as the captured 0.155.1 native command item;
        // values are synthetic. In particular, there is no invented `input`.
        let events = parse_notification(
            "item/started",
            &json!({
                "threadId": "thread", "turnId": "turn", "item": {
                    "type": "commandExecution", "id": "command", "command": "cargo test",
                    "cwd": "/project", "status": "inProgress", "commandActions": [],
                    "aggregatedOutput": null, "exitCode": null, "durationMs": null
                }
            }),
        );
        let ProviderEvent::ToolUse { input, .. } = &events[0] else {
            panic!("missing tool use");
        };
        let input: Value = serde_json::from_str(input).unwrap();
        assert_eq!(input["command"], "cargo test");
        assert_eq!(input["cwd"], "/project");
        assert!(input.get("aggregatedOutput").is_none());
    }

    #[test]
    fn argv_commands_preserve_argument_boundaries_and_other_input_fields() {
        // The argv form is also exercised by the Codex approval fixtures.
        let argv = json!(["printf", "%s", "a path with spaces"]);
        for raw in [
            json!({"type":"commandExecution", "command":argv, "cwd":"/project", "input":{"timeout_ms":1000}}),
            json!({"type":"commandExecution", "input":{"command":argv, "timeout_ms":1000}}),
        ] {
            let input = tool_input(&raw);
            assert_eq!(input["command"], "printf %s a path with spaces");
            assert_eq!(input["command_argv"], argv);
            assert_eq!(input["timeout_ms"], 1000);
        }
        let input = tool_input(
            &json!({"command":"cargo test", "input":{"command":"stale", "timeout_ms":1000}}),
        );
        assert_eq!(input["command"], "cargo test");
        assert_eq!(input["timeout_ms"], 1000);
    }

    #[test]
    fn tool_arguments_are_preserved_without_copying_result_payloads_into_input() {
        assert_eq!(
            tool_input(
                &json!({"type":"mcpToolCall", "input":null, "arguments":{"file":"readme"}, "result":"contents"})
            ),
            json!({"file":"readme"})
        );
        assert_eq!(
            tool_input(&json!({"type":"toolCall", "input":"plain text input"})),
            json!("plain text input")
        );
        assert_eq!(
            tool_input(&json!({"type":"toolCall", "input":"{\"command\":\"ls\"}"}))["command"],
            "ls"
        );
    }

    #[test]
    fn a_completed_tool_item_becomes_a_result() {
        let events = parse_notification(
            "item/completed",
            &json!({
                "item": {
                    "type": "commandExecution",
                    "id": "c1",
                    "output": "file.txt"
                }
            }),
        );
        match &events[0] {
            ProviderEvent::ToolResult {
                id,
                content,
                is_error,
            } => {
                assert_eq!(id, "c1");
                assert!(content.contains("file.txt"));
                assert!(!is_error);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_failed_tool_item_is_marked_as_an_error() {
        let events = parse_notification(
            "item/completed",
            &json!({
                "item": { "type": "commandExecution", "id": "c1", "error": { "message": "boom" } }
            }),
        );
        match &events[0] {
            ProviderEvent::ToolResult { is_error, .. } => assert!(is_error),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn explicit_nulls_in_a_completed_tool_item_are_neither_output_nor_failure() {
        let result =
            |item: Value| match parse_notification("item/completed", &json!({ "item": item }))
                .remove(0)
            {
                ProviderEvent::ToolResult {
                    content, is_error, ..
                } => (content, is_error),
                other => panic!("{other:?}"),
            };

        // A successful MCP call: `error` is present and null.
        let (content, is_error) = result(json!({
            "type": "mcpToolCall", "id": "m1", "status": "completed",
            "result": {"content": [{"type": "text", "text": "42"}]}, "error": null
        }));
        assert!(!is_error, "a null error is not a failure");
        assert!(content.contains("42"), "{content}");

        // A failed MCP call: the leading null `result` must not hide the error.
        let (content, is_error) = result(json!({
            "type": "mcpToolCall", "id": "m2", "status": "failed",
            "result": null, "error": {"message": "server unavailable"}
        }));
        assert!(is_error);
        assert!(content.contains("server unavailable"), "{content}");

        // A command that failed by exit code, with nulls ahead of nothing.
        let (content, is_error) = result(json!({
            "type": "commandExecution", "id": "c2", "status": "failed",
            "aggregatedOutput": null, "exitCode": 2
        }));
        assert!(is_error);
        assert_ne!(content, "null");

        // A quiet success stays trivial, so the conversation drops it.
        let (content, is_error) = result(json!({
            "type": "commandExecution", "id": "c3", "status": "completed",
            "aggregatedOutput": null, "exitCode": 0, "error": null
        }));
        assert!(!is_error);
        assert_eq!(content, TRIVIAL_TOOL_RESULT);
    }

    #[test]
    fn usage_reports_the_running_total_and_the_last_request_fills_the_window() {
        let events = parse_notification(
            "thread/tokenUsage/updated",
            &json!({
                "tokenUsage": {
                    "total": {
                        "inputTokens": 900,
                        "outputTokens": 120,
                        "cachedInputTokens": 400,
                        "reasoningOutputTokens": 50,
                        "totalTokens": 1020
                    },
                    "last": {
                        "inputTokens": 100,
                        "outputTokens": 20,
                        "cachedInputTokens": 40,
                        "reasoningOutputTokens": 5,
                        "totalTokens": 125
                    },
                    "modelContextWindow": 272000
                }
            }),
        );
        assert_eq!(
            events,
            vec![
                ProviderEvent::CumulativeUsage(Usage {
                    input_tokens: 900,
                    output_tokens: 120,
                    cached_input_tokens: 400,
                    reasoning_output_tokens: 50,
                    total_tokens: 1020,
                }),
                ProviderEvent::ContextUsage {
                    used: 125,
                    window: 272_000,
                },
            ]
        );
    }

    #[test]
    fn repeated_usage_updates_do_not_inflate_the_totals() {
        // Every update carries the thread's running total. Adding `last` from
        // each one counted the same requests again and again.
        let mut conversation = crate::conversation::Conversation::new(uuid::Uuid::nil());
        for (total, last) in [(100, 100), (150, 50), (150, 0)] {
            for event in parse_notification(
                "thread/tokenUsage/updated",
                &json!({ "tokenUsage": {
                    "total": { "inputTokens": total, "totalTokens": total },
                    "last": { "inputTokens": last, "totalTokens": last },
                    "modelContextWindow": 1000
                } }),
            ) {
                conversation.apply(event);
            }
        }
        assert_eq!(conversation.usage().total_tokens, 150);
        assert_eq!(conversation.usage().input_tokens, 150);
    }

    #[test]
    fn usage_without_a_total_counts_nothing_and_does_not_panic() {
        // A notification shape change must not panic the turn, nor invent a
        // total the server did not send.
        for usage in [json!({}), json!({ "last": { "totalTokens": 10 } })] {
            let events =
                parse_notification("thread/tokenUsage/updated", &json!({ "tokenUsage": usage }));
            assert!(events.is_empty(), "{events:?}");
        }
    }

    #[test]
    fn a_zero_context_window_is_omitted() {
        let events = parse_notification(
            "thread/tokenUsage/updated",
            &json!({
                "tokenUsage": {
                    "total": { "totalTokens": 10 },
                    "last": { "totalTokens": 10 },
                    "modelContextWindow": 0
                }
            }),
        );
        assert_eq!(events.len(), 1, "{events:?}");
    }

    #[test]
    fn a_retryable_error_is_a_preparation_phase_rather_than_a_failure() {
        let events = parse_notification("error", &json!({ "willRetry": true }));
        assert_eq!(events, vec![ProviderEvent::Lifecycle(Lifecycle::Preparing)]);
    }

    #[test]
    fn a_terminal_error_is_surfaced() {
        let events =
            parse_notification("error", &json!({ "error": { "message": "rate limited" } }));
        assert_eq!(events, vec![ProviderEvent::Error("rate limited".into())]);
    }

    #[test]
    fn a_failed_turn_reports_the_error_rather_than_completing() {
        let events = parse_notification(
            "turn/completed",
            &json!({ "turn": { "status": "failed", "error": { "message": "no credits" } } }),
        );
        assert_eq!(events, vec![ProviderEvent::Error("no credits".into())]);
    }

    #[test]
    fn a_successful_turn_completes_with_its_status() {
        let events = parse_notification(
            "turn/completed",
            &json!({ "turn": { "status": "completed" } }),
        );
        assert_eq!(
            events,
            vec![ProviderEvent::Done {
                stop_reason: "completed".into()
            }]
        );
    }

    #[test]
    fn user_input_is_capped_at_the_documented_limit() {
        let questions: Vec<Value> = (0..10).map(|i| json!({ "id": format!("q{i}") })).collect();
        let events = parse_notification(
            "item/tool/requestUserInput",
            &json!({ "id": "r1", "questions": questions }),
        );
        match &events[0] {
            ProviderEvent::UserInput(request) => {
                assert_eq!(request.questions.len(), crate::provider::MAX_QUESTIONS);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn options_are_capped_per_question() {
        let options: Vec<Value> = (0..30)
            .map(|i| json!({ "label": format!("o{i}") }))
            .collect();
        let events = parse_notification(
            "item/tool/requestUserInput",
            &json!({
                "id": "r1",
                "questions": [{ "id": "q", "options": options }]
            }),
        );
        match &events[0] {
            ProviderEvent::UserInput(request) => {
                assert_eq!(
                    request.questions[0].options.len(),
                    crate::provider::MAX_OPTIONS
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_command_approval_is_named_and_rated() {
        let events = parse_notification(
            "item/commandExecution/requestApproval",
            &json!({ "item": { "type": "commandExecution", "id": "c1", "command": "rm -rf /" } }),
        );
        match &events[0] {
            ProviderEvent::Approval(a) => {
                assert_eq!(a.tool_name, "shell");
                assert_eq!(a.risk, RiskLevel::Dangerous);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_read_only_command_is_rated_moderate() {
        let events = parse_notification(
            "item/commandExecution/requestApproval",
            &json!({ "item": { "type": "commandExecution", "command": "ls -la" } }),
        );
        match &events[0] {
            ProviderEvent::Approval(a) => assert_eq!(a.risk, RiskLevel::Moderate),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_argv_command_is_rated_as_argv() {
        // Joined into one line, the quotes would turn `-delete` into part of a
        // harmless-looking word.
        let hidden = json!(["find", ".", "'", "-delete", "'"]);
        assert_eq!(
            risk_for("commandExecution", Some(&hidden)),
            RiskLevel::Dangerous
        );
        assert_eq!(
            risk_for(
                "commandExecution",
                Some(&json!(["/bin/zsh", "-lc", "rg -n foo"]))
            ),
            RiskLevel::Moderate
        );
        assert!(!is_read_only_command_value(&json!(["cat", 7])));
    }

    #[test]
    fn the_read_only_check_is_conservative() {
        assert!(is_read_only_command("ls"));
        assert!(
            is_read_only_command("/bin/cat file"),
            "a path prefix is fine"
        );
        assert!(
            is_read_only_command("  grep foo  "),
            "surrounding space is fine"
        );

        // Anything it is unsure about is treated as dangerous.
        for cmd in [
            "ls | sh",
            "cat > out.txt",
            "ls; rm -rf /",
            "echo hi && rm file",
            "rm -rf /",
            "",
            "  ",
        ] {
            assert!(
                !is_read_only_command(cmd),
                "{cmd:?} was treated as read-only"
            );
        }
    }

    #[test]
    fn a_web_search_is_never_risky() {
        assert_eq!(risk_for("webSearch", None), RiskLevel::Safe);
        assert_eq!(risk_for("imageView", None), RiskLevel::Safe);
    }

    #[test]
    fn a_reroute_is_surfaced_with_its_reason() {
        let events = parse_notification(
            "model/rerouted",
            &json!({ "toModel": "gpt-5.6-terra", "reason": "overloaded" }),
        );
        assert_eq!(
            events,
            vec![ProviderEvent::ModelChanged {
                model: "gpt-5.6-terra".into(),
                reason: "overloaded".into()
            }]
        );
    }

    #[test]
    fn a_system_error_status_is_surfaced() {
        let events = parse_notification(
            "thread/status/changed",
            &json!({ "status": { "type": "systemError", "message": "disk full" } }),
        );
        assert_eq!(events, vec![ProviderEvent::Error("disk full".into())]);
    }

    // ── Results and catalogs ───────────────────────────────────────────────

    #[test]
    fn a_thread_id_is_read_from_a_start_or_resume_result() {
        assert_eq!(
            thread_id_from_result(&json!({ "thread": { "id": "t-abc" } })).as_deref(),
            Some("t-abc")
        );
        assert_eq!(thread_id_from_result(&json!({})), None);
    }

    #[test]
    fn a_page_is_read_with_its_cursor() {
        let result = json!({
            "data": [{ "id": "a" }, { "id": "b" }],
            "nextCursor": "c2"
        });
        assert_eq!(threads_from_result(&result).len(), 2);
        assert_eq!(next_cursor(&result).as_deref(), Some("c2"));
        assert_eq!(next_cursor(&json!({"data": []})), None);
    }

    #[test]
    fn an_empty_catalog_falls_back_rather_than_offering_nothing() {
        let fallback = fallback_models();
        assert_eq!(parse_model_catalog(&json!({}), &fallback), fallback);
        assert!(!fallback.is_empty());
    }

    #[test]
    fn exactly_one_fallback_model_is_the_default() {
        let fallback = fallback_models();
        assert_eq!(fallback.iter().filter(|m| m.is_default).count(), 1);
    }

    #[test]
    fn the_catalog_marks_its_first_entry_as_the_default() {
        let payload = json!({
            "models": [
                { "slug": "a", "display_name": "A", "visibility": "list" },
                { "slug": "b", "display_name": "B", "visibility": "list" }
            ]
        });
        let models = parse_model_catalog(&payload, &[]);
        assert!(models[0].is_default);
        assert!(!models[1].is_default);
    }

    #[test]
    fn hidden_models_are_not_offered() {
        let payload = json!({
            "models": [
                { "slug": "listed", "visibility": "list" },
                { "slug": "hidden", "visibility": "internal" },
                { "slug": "untagged" }
            ]
        });
        let models = parse_model_catalog(&payload, &[]);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "listed");
    }

    #[test]
    fn a_known_model_supplies_fields_the_payload_omits() {
        let fallback = fallback_models();
        let payload = json!({
            "models": [{ "slug": "gpt-5.6-sol", "visibility": "list" }]
        });
        let models = parse_model_catalog(&payload, &fallback);
        assert_eq!(models[0].display_name, "GPT-5.6 Sol");
        assert_eq!(models[0].context_window, 272_000);
        assert!(!models[0].supported_efforts.is_empty());
    }

    #[test]
    fn vision_support_is_read_from_the_input_modalities() {
        let with_image = json!({
            "models": [{
                "slug": "a",
                "visibility": "list",
                "input_modalities": ["text", "image"]
            }]
        });
        let text_only = json!({
            "models": [{ "slug": "a", "visibility": "list", "input_modalities": ["text"] }]
        });
        assert!(parse_model_catalog(&with_image, &[])[0].supports_vision);
        assert!(!parse_model_catalog(&text_only, &[])[0].supports_vision);
    }

    #[test]
    fn reasoning_levels_become_effort_names() {
        let payload = json!({
            "models": [{
                "slug": "a",
                "visibility": "list",
                "supported_reasoning_levels": [{ "effort": "low" }, { "effort": "high" }],
                "default_reasoning_level": "low"
            }]
        });
        let models = parse_model_catalog(&payload, &[]);
        assert_eq!(models[0].supported_efforts, vec!["low", "high"]);
        assert_eq!(models[0].default_effort.as_deref(), Some("low"));
    }

    #[test]
    fn an_unspecified_effort_is_omitted_from_turn_requests() {
        let request = TurnRequest::new("thread", "prompt", "/tmp").effort("");
        let params = turn_params(&request);
        assert!(params.get("effort").is_none());
        assert!(params["collaborationMode"]["settings"]["reasoning_effort"].is_null());
    }

    #[test]
    fn rpc_model_catalog_preserves_account_defaults_and_filters_hidden_entries() {
        let models = parse_model_list(
            &json!({"data": [
                {"id":"picker-id", "model":"account-model", "displayName":"Account model", "isDefault":true,
                 "supportedReasoningEfforts":[{"reasoningEffort":"medium"},{"reasoningEffort":"high"}],
                 "defaultReasoningEffort":"high", "inputModalities":["text"]},
                {"id":"old-model"}, {"id":"hidden", "hidden":true}, {"displayName":"invalid"}
            ]}),
            &[],
        );
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "account-model");
        assert_eq!(models[0].supported_efforts, ["medium", "high"]);
        assert_eq!(models[0].default_effort.as_deref(), Some("high"));
        assert!(models[0].is_default);
        assert!(!models[0].supports_vision);
        assert!(models[1].supports_vision);
    }

    // ── Writer conflict ─────────────────────────────────────────────────────

    #[test]
    fn the_real_codex_conflict_phrase_is_recognized() {
        assert!(CodexWriterConflict::matches(
            "thread existing-task already has an active writer"
        ));
        assert!(CodexWriterConflict::matches(
            "THREAD EXISTING-TASK ALREADY HAS AN ACTIVE WRITER"
        ));
        assert!(CodexWriterConflict::matches(CodexWriterConflict::MESSAGE));
    }

    #[test]
    fn an_ordinary_error_is_not_mistaken_for_a_conflict() {
        for message in [
            "rate limited",
            "model not found",
            "connection reset",
            "the writer is unavailable for another reason",
        ] {
            assert!(
                !CodexWriterConflict::matches(message),
                "{message:?} was misread as a writer conflict"
            );
        }
    }
}
