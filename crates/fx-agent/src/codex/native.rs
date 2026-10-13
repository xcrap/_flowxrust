//! Read-only Codex thread discovery and complete, paginated native transcripts.
//!
//! No thread is resumed or subscribed: reading history must not claim its writer.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, UNIX_EPOCH};

use base64::Engine;
use fx_core::services::runtime::RuntimeDiscovery;
use fx_core::{AgentAccess, ConversationMessage, MessageContent, MessageRole, NativeDefaults};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{CodexProvider, ServerEvent, Transport, TransportError};
use crate::native::{NativeThread, NativeThreadIndex, NativeThreadSummary};
use crate::{ProviderError, ThreadIdentity, ThreadSummary};

const LIST_TIMEOUT: Duration = Duration::from_secs(45);
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_INDEX_BYTES: usize = 32 * 1024 * 1024;
const MAX_HISTORY_BYTES: usize = 128 * 1024 * 1024;
const MAX_CACHED_HISTORIES: usize = 4;
const MAX_CONTEXT_SEARCH: u64 = 32 * 1024 * 1024;
const CONTEXT_CHUNK: u64 = 256 * 1024;
const MAIN_SOURCES: &[&str] = &["cli", "vscode", "exec", "appServer", "unknown"];

struct Connection {
    transport: Arc<Transport>,
    drain: tokio::task::JoinHandle<()>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.drain.abort();
    }
}

/// Reuse one instance on a provider worker's Tokio runtime. Calls are serialized;
/// the read-only app-server is restarted after exit or a failed timed-out request.
pub struct CodexNativeStore {
    provider: CodexProvider,
    connection: Mutex<Option<Connection>>,
    operation: Mutex<()>,
    archived: StdMutex<HashSet<String>>,
    metadata: StdMutex<HashMap<String, RolloutMetadata>>,
    histories: StdMutex<VecDeque<CachedHistory>>,
}

struct CachedHistory {
    thread: NativeThread,
    bytes: usize,
}

impl CodexNativeStore {
    pub fn new(discovery: RuntimeDiscovery) -> Self {
        Self::from_provider(CodexProvider::new(discovery))
    }

    fn from_provider(provider: CodexProvider) -> Self {
        Self {
            provider,
            connection: Mutex::new(None),
            operation: Mutex::new(()),
            archived: StdMutex::new(HashSet::new()),
            metadata: StdMutex::new(HashMap::new()),
            histories: StdMutex::new(VecDeque::new()),
        }
    }

    async fn connection(&self) -> Result<Arc<Transport>, ProviderError> {
        let mut connection = self.connection.lock().await;
        if let Some(existing) = connection.as_ref()
            && existing.transport.exit_reason().await.is_none()
        {
            return Ok(existing.transport.clone());
        }
        if let Some(old) = connection.take() {
            old.transport.close().await;
        }
        let mut transport = self.provider.start_session().await?;
        let mut events = transport.take_events();
        let transport = Arc::new(transport);
        let weak = Arc::downgrade(&transport);
        let drain = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if let ServerEvent::Request { id, .. } = event
                    && let Some(transport) = weak.upgrade()
                {
                    let _ = transport.refuse(
                        id,
                        -32601,
                        "History discovery does not run tools or answer account prompts.",
                    );
                }
            }
        });
        *connection = Some(Connection {
            transport: transport.clone(),
            drain,
        });
        Ok(transport)
    }

    async fn reset_connection(&self) {
        if let Some(connection) = self.connection.lock().await.take() {
            connection.transport.close().await;
        }
    }

    pub async fn list(&self, working_directory: &str) -> Result<NativeThreadIndex, ProviderError> {
        let workspace = canonical_workspace(working_directory)?;
        let _operation = self.operation.lock().await;
        let result =
            tokio::time::timeout(LIST_TIMEOUT, self.list_inner(&workspace, working_directory))
                .await;
        match result {
            Ok(Ok(index)) => Ok(index),
            Ok(Err(error)) => {
                self.reset_connection().await;
                Err(error)
            }
            Err(_) => {
                self.reset_connection().await;
                Err(unavailable(
                    "Codex thread discovery timed out; the previous index was kept.",
                ))
            }
        }
    }

    async fn list_inner(
        &self,
        workspace: &Path,
        configured: &str,
    ) -> Result<NativeThreadIndex, ProviderError> {
        let transport = self.connection().await?;
        let mut index = NativeThreadIndex::default();
        let mut ids = HashSet::new();
        let mut bytes = 0;
        let mut archived_ids = HashSet::new();
        let canonical = workspace.to_string_lossy();
        let cwd = if configured == canonical {
            json!(canonical)
        } else {
            json!([configured, canonical])
        };
        for archived in [false, true] {
            let mut cursor: Option<String> = None;
            let mut cursors = HashSet::new();
            loop {
                let response = transport
                    .request(
                        "thread/list",
                        Some(json!({
                            "cwd": cwd, "limit": 100, "cursor": cursor, "archived": archived,
                            "sortKey":"recency_at", "sortDirection":"desc", "useStateDbOnly":true,
                            "sourceKinds": MAIN_SOURCES, "modelProviders":[]
                        })),
                    )
                    .await
                    .map_err(protocol_error)?;
                let page = data_array(&response, "thread/list")?;
                for raw in page {
                    bytes += raw.to_string().len();
                    if bytes > MAX_INDEX_BYTES {
                        return Err(unavailable(
                            "Codex's project thread index exceeds the 32 MiB discovery limit; the previous index was kept.",
                        ));
                    }
                    let Some(mut entry) = summary(raw, workspace, archived)? else {
                        continue;
                    };
                    self.enrich(raw, workspace, &mut entry, false);
                    let id = entry.summary.identity.session_id.clone();
                    if archived {
                        archived_ids.insert(id.clone());
                    }
                    if ids.insert(id) {
                        index.threads.push(entry);
                    }
                }
                let next = page_cursor(&response)?;
                if !advance_cursor(&mut cursors, &next)? {
                    break;
                }
                cursor = next;
            }
        }
        let workspace_ids: HashSet<_> = index
            .threads
            .iter()
            .map(|entry| entry.summary.identity.session_id.clone())
            .collect();
        let mut known_archived = self.archived.lock().unwrap_or_else(|e| e.into_inner());
        known_archived.retain(|id| !workspace_ids.contains(id));
        known_archived.extend(archived_ids);
        index.threads.sort_by(|a, b| {
            b.summary
                .updated_at
                .cmp(&a.summary.updated_at)
                .then_with(|| {
                    a.summary
                        .identity
                        .session_id
                        .cmp(&b.summary.identity.session_id)
                })
        });
        Ok(index)
    }

    pub async fn read(
        &self,
        working_directory: &str,
        session_id: &str,
    ) -> Result<NativeThread, ProviderError> {
        self.read_if_changed(working_directory, session_id, None)
            .await?
            .ok_or_else(|| protocol("Codex did not return the requested history."))
    }

    /// Return `None` only when both provider metadata and a validated rollout's
    /// fingerprint match the revision the caller has already applied. Without a
    /// readable native file, second-resolution timestamps never suppress reads.
    pub async fn read_if_changed(
        &self,
        working_directory: &str,
        session_id: &str,
        known_revision: Option<&str>,
    ) -> Result<Option<NativeThread>, ProviderError> {
        let workspace = canonical_workspace(working_directory)?;
        if session_id.trim().is_empty() {
            return Err(protocol("A Codex thread id is required."));
        }
        let _operation = self.operation.lock().await;
        let result = tokio::time::timeout(
            READ_TIMEOUT,
            self.read_inner(&workspace, session_id, known_revision),
        )
        .await;
        match result {
            Ok(Ok(thread)) => Ok(thread),
            Ok(Err(error)) => {
                self.reset_connection().await;
                Err(error)
            }
            Err(_) => {
                self.reset_connection().await;
                Err(unavailable(
                    "Codex history loading timed out; the saved transcript was kept.",
                ))
            }
        }
    }

    async fn read_inner(
        &self,
        workspace: &Path,
        session_id: &str,
        known_revision: Option<&str>,
    ) -> Result<Option<NativeThread>, ProviderError> {
        let transport = self.connection().await?;
        let response = transport
            .request(
                "thread/read",
                Some(json!({"threadId":session_id,"includeTurns":false})),
            )
            .await
            .map_err(protocol_error)?;
        let raw = response
            .get("thread")
            .ok_or_else(|| protocol("Codex returned no thread metadata."))?;
        let archived = self
            .archived
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(session_id)
            || raw["path"].as_str().is_some_and(|p| {
                Path::new(p)
                    .components()
                    .any(|c| c.as_os_str() == "archived_sessions")
            });
        let mut entry = summary(raw, workspace, archived)?.ok_or_else(|| {
            protocol("This Codex thread does not belong to the selected project.")
        })?;
        if entry.summary.identity.session_id != session_id {
            return Err(protocol(
                "Codex returned a different thread than requested.",
            ));
        }
        let current_context_tokens = self.enrich(raw, workspace, &mut entry, true);
        let reliable_fingerprint = self.validated_fingerprint(raw, session_id);
        if reliable_fingerprint.is_some() {
            if known_revision == Some(entry.revision.as_str()) {
                return Ok(None);
            }
            let mut histories = self.histories.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(index) = histories.iter().position(|cached| {
                cached.thread.entry.summary.identity == entry.summary.identity
                    && cached.thread.entry.revision == entry.revision
            }) {
                let cached = histories.remove(index).unwrap();
                let thread = cached.thread.clone();
                histories.push_back(cached);
                return Ok(Some(thread));
            }
        }
        let mut cursor: Option<String> = None;
        let mut cursors = HashSet::new();
        let mut turns = Vec::new();
        let mut turn_ids = HashSet::new();
        let mut bytes = 0;
        loop {
            let response = transport.request("thread/turns/list", Some(json!({
                "threadId":session_id,"cursor":cursor,"limit":8,"sortDirection":"desc","itemsView":"full"
            }))).await;
            let response = match response {
                Ok(response) => response,
                Err(TransportError::Rejected { code: -32601, .. }) if cursor.is_none() => {
                    let legacy = transport
                        .request(
                            "thread/read",
                            Some(json!({"threadId":session_id,"includeTurns":true})),
                        )
                        .await
                        .map_err(protocol_error)?;
                    let raw = legacy
                        .get("thread")
                        .ok_or_else(|| protocol("Codex returned no thread history."))?;
                    let checked = summary(raw, workspace, archived)?
                        .ok_or_else(|| protocol("Codex returned history from another project."))?;
                    if checked.summary.identity.session_id != session_id {
                        return Err(protocol("Codex returned another thread's history."));
                    }
                    let turns = raw["turns"]
                        .as_array()
                        .ok_or_else(|| protocol("Codex returned invalid thread history."))?;
                    if serialized_size(turns)? > MAX_HISTORY_BYTES {
                        return Err(history_size_error());
                    }
                    let messages = convert_turns(session_id, turns, raw)?;
                    return self.finish_read(
                        NativeThread {
                            entry,
                            messages,
                            current_context_tokens,
                            history_truncated: false,
                        },
                        raw,
                        reliable_fingerprint,
                    );
                }
                Err(error) => return Err(protocol_error(error)),
            };
            for turn in data_array(&response, "thread/turns/list")? {
                let id = required_string(turn, "id")?;
                if !turn["items"].is_array() {
                    return Err(protocol("Codex returned a turn without its history items."));
                }
                if !turn_ids.insert(id.to_string()) {
                    continue;
                }
                if turn["itemsView"]
                    .as_str()
                    .is_some_and(|view| view != "full")
                {
                    return Err(protocol(
                        "Codex returned a summarized turn when full history was requested.",
                    ));
                }
                bytes += turn.to_string().len();
                if bytes > MAX_HISTORY_BYTES {
                    return Err(history_size_error());
                }
                turns.push(turn.clone());
            }
            let next = page_cursor(&response)?;
            if !advance_cursor(&mut cursors, &next)? {
                break;
            }
            cursor = next;
        }
        turns.reverse();
        let messages = convert_turns(session_id, &turns, raw)?;
        self.finish_read(
            NativeThread {
                entry,
                messages,
                current_context_tokens,
                history_truncated: false,
            },
            raw,
            reliable_fingerprint,
        )
    }

    fn validated_fingerprint(&self, raw: &Value, id: &str) -> Option<String> {
        let path = Path::new(raw["path"].as_str()?);
        let fingerprint = file_fingerprint(path)?;
        let key = format!("{id}|{}", path.display());
        let metadata = self.metadata.lock().unwrap_or_else(|e| e.into_inner());
        metadata
            .get(&key)
            .filter(|cached| cached.validated && cached.fingerprint == fingerprint)
            .map(|_| fingerprint)
    }

    fn finish_read(
        &self,
        thread: NativeThread,
        raw: &Value,
        fingerprint: Option<String>,
    ) -> Result<Option<NativeThread>, ProviderError> {
        let bytes = serialized_size(&thread.messages)?;
        if bytes > MAX_HISTORY_BYTES {
            return Err(history_size_error());
        }
        // An append while paging invalidates this snapshot for caching. The
        // caller receives its original revision and will refresh on the next poll.
        if fingerprint.is_some()
            && fingerprint
                == raw["path"]
                    .as_str()
                    .and_then(|path| file_fingerprint(Path::new(path)))
        {
            let mut histories = self.histories.lock().unwrap_or_else(|e| e.into_inner());
            histories.retain(|cached| {
                cached.thread.entry.summary.identity != thread.entry.summary.identity
            });
            while histories.len() >= MAX_CACHED_HISTORIES
                || histories.iter().map(|cached| cached.bytes).sum::<usize>() + bytes
                    > MAX_HISTORY_BYTES
            {
                histories.pop_front();
            }
            histories.push_back(CachedHistory {
                thread: thread.clone(),
                bytes,
            });
        }
        Ok(Some(thread))
    }

    fn enrich(
        &self,
        raw: &Value,
        workspace: &Path,
        entry: &mut NativeThreadSummary,
        include_usage: bool,
    ) -> Option<u64> {
        let path = raw["path"].as_str().map(Path::new)?;
        let fingerprint = file_fingerprint(path)?;
        entry.revision.push_str(&format!("|{fingerprint}"));
        let key = format!("{}|{}", entry.summary.identity.session_id, path.display());
        let mut cache = self.metadata.lock().unwrap_or_else(|e| e.into_inner());
        if cache
            .get(&key)
            .is_some_and(|value| value.fingerprint != fingerprint)
        {
            cache.remove(&key);
        }
        if !include_usage {
            if let Some(metadata) = cache.get(&key) {
                merge_defaults(&mut entry.defaults, &metadata.defaults);
            }
            return None;
        }
        if cache.len() > 4096 {
            cache.clear();
        }
        let metadata = cache.entry(key).or_insert_with(|| {
            read_rollout_metadata(
                path,
                &entry.summary.identity.session_id,
                workspace,
                fingerprint,
            )
        });
        merge_defaults(&mut entry.defaults, &metadata.defaults);
        metadata.context_tokens
    }
}

fn canonical_workspace(path: &str) -> Result<PathBuf, ProviderError> {
    let path = std::fs::canonicalize(path)
        .map_err(|error| unavailable(&format!("Could not open the project directory: {error}")))?;
    if !path.is_dir() {
        return Err(protocol("The project working directory is not a folder."));
    }
    Ok(path)
}
fn protocol(message: &str) -> ProviderError {
    ProviderError::Protocol(message.into())
}
fn unavailable(message: &str) -> ProviderError {
    ProviderError::Unavailable(message.into())
}
fn history_size_error() -> ProviderError {
    unavailable("Codex history exceeds the 128 MiB import limit; the saved transcript was kept.")
}
fn serialized_size<T: serde::Serialize + ?Sized>(value: &T) -> Result<usize, ProviderError> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).map_err(|error| protocol(&error.to_string()))?;
    Ok(counter.0)
}
fn protocol_error(error: TransportError) -> ProviderError {
    ProviderError::Protocol(error.to_string())
}
fn required_string<'a>(raw: &'a Value, key: &str) -> Result<&'a str, ProviderError> {
    raw[key]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            protocol(&format!(
                "Codex returned a missing or invalid `{key}` field."
            ))
        })
}
fn data_array<'a>(raw: &'a Value, method: &str) -> Result<&'a Vec<Value>, ProviderError> {
    raw["data"]
        .as_array()
        .ok_or_else(|| protocol(&format!("Codex returned an invalid {method} page.")))
}
fn page_cursor(raw: &Value) -> Result<Option<String>, ProviderError> {
    match raw.get("nextCursor") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(cursor)) if cursor.is_empty() => Ok(None),
        Some(Value::String(cursor)) => Ok(Some(cursor.clone())),
        _ => Err(protocol("Codex returned an invalid pagination cursor.")),
    }
}
fn advance_cursor(
    seen: &mut HashSet<String>,
    cursor: &Option<String>,
) -> Result<bool, ProviderError> {
    let Some(cursor) = cursor else {
        return Ok(false);
    };
    if !seen.insert(cursor.clone()) {
        return Err(protocol(
            "Codex repeated a history pagination cursor; the incomplete result was discarded.",
        ));
    }
    Ok(true)
}
fn timestamp(value: Option<i64>) -> fx_core::Timestamp {
    value
        .and_then(|v| {
            if v.unsigned_abs() < 100_000_000_000 {
                chrono::DateTime::from_timestamp(v, 0)
            } else {
                chrono::DateTime::from_timestamp_millis(v)
            }
        })
        .unwrap_or_default()
}
fn summary(
    raw: &Value,
    workspace: &Path,
    archived: bool,
) -> Result<Option<NativeThreadSummary>, ProviderError> {
    let id = required_string(raw, "id")?;
    let cwd = required_string(raw, "cwd")?;
    if !Path::new(cwd).is_absolute()
        || std::fs::canonicalize(cwd).ok().as_deref() != Some(workspace)
    {
        return Ok(None);
    }
    let title = ["name", "title", "preview"]
        .into_iter()
        .filter_map(|key| raw[key].as_str())
        .map(str::trim)
        .find(|text| !text.is_empty())
        .unwrap_or("Codex thread")
        .lines()
        .next()
        .unwrap_or("Codex thread")
        .chars()
        .take(100)
        .collect();
    let mut revision_raw = raw.clone();
    if let Some(object) = revision_raw.as_object_mut() {
        object.remove("turns");
    }
    let revision =
        Uuid::new_v5(&Uuid::NAMESPACE_URL, revision_raw.to_string().as_bytes()).to_string();
    Ok(Some(NativeThreadSummary {
        summary: ThreadSummary {
            identity: ThreadIdentity {
                provider_id: "codex".into(),
                session_id: id.into(),
            },
            title,
            working_directory: workspace.to_string_lossy().into_owned(),
            updated_at: timestamp(raw["recencyAt"].as_i64().or(raw["updatedAt"].as_i64())),
            is_archived: archived,
        },
        defaults: native_defaults(raw),
        revision: format!("{revision}|archive:{archived}"),
    }))
}
fn native_defaults(raw: &Value) -> NativeDefaults {
    let collaboration = raw
        .get("collaborationMode")
        .or(raw.get("collaboration_mode"))
        .unwrap_or(&Value::Null);
    let settings = &collaboration["settings"];
    let string = |keys: &[&str]| {
        keys.iter().find_map(|key| {
            raw[*key]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
    };
    let approval = string(&["approvalPolicy", "approval_policy"]);
    let sandbox = raw
        .get("sandboxPolicy")
        .or(raw.get("sandbox_policy"))
        .or(raw.get("sandbox"))
        .unwrap_or(&Value::Null);
    let sandbox = sandbox
        .as_str()
        .or(sandbox["type"].as_str())
        .unwrap_or("")
        .replace('-', "")
        .to_ascii_lowercase();
    let access = match (approval.as_deref(), sandbox.as_str()) {
        (Some("never"), "dangerfullaccess") => Some(AgentAccess::FullAccess),
        (Some("on-request"), "workspacewrite") => Some(AgentAccess::AcceptEdits),
        (Some("untrusted"), "workspacewrite") | (_, "readonly") => Some(AgentAccess::Supervised),
        _ => None,
    };
    NativeDefaults {
        provider_id: Some("codex".into()),
        model_id: string(&["model"]).or_else(|| settings["model"].as_str().map(str::to_owned)),
        effort: string(&["effort", "reasoningEffort", "reasoning_effort"])
            .or_else(|| settings["reasoning_effort"].as_str().map(str::to_owned)),
        agent_access: access,
        context_window_size: raw["model_context_window"]
            .as_u64()
            .or(raw["context_window"].as_u64()),
    }
}
fn merge_defaults(target: &mut NativeDefaults, other: &NativeDefaults) {
    if other.model_id.is_some() {
        target.model_id.clone_from(&other.model_id);
    }
    if other.effort.is_some() {
        target.effort.clone_from(&other.effort);
    }
    if other.agent_access.is_some() {
        target.agent_access = other.agent_access;
    }
    if other.context_window_size.is_some() {
        target.context_window_size = other.context_window_size;
    }
}

struct RolloutMetadata {
    validated: bool,
    fingerprint: String,
    defaults: NativeDefaults,
    context_tokens: Option<u64>,
}
fn file_fingerprint(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(format!(
        "{}:{modified}:{}:{}:{}:{}",
        metadata.len(),
        metadata.dev(),
        metadata.ino(),
        metadata.ctime(),
        metadata.ctime_nsec()
    ))
}
fn read_rollout_metadata(
    path: &Path,
    id: &str,
    workspace: &Path,
    fingerprint: String,
) -> RolloutMetadata {
    let mut result = RolloutMetadata {
        validated: false,
        fingerprint,
        defaults: NativeDefaults::default(),
        context_tokens: None,
    };
    let Ok(mut file) = std::fs::File::open(path) else {
        return result;
    };
    let Ok(metadata) = file.metadata() else {
        return result;
    };
    if !metadata.is_file() {
        return result;
    }
    let mut header = String::new();
    if BufReader::new((&mut file).take(128 * 1024))
        .read_line(&mut header)
        .is_err()
    {
        return result;
    }
    let Ok(header) = serde_json::from_str::<Value>(&header) else {
        return result;
    };
    if header["type"] != "session_meta"
        || header["payload"]["id"] != id
        || header["payload"]["cwd"]
            .as_str()
            .and_then(|p| std::fs::canonicalize(p).ok())
            .as_deref()
            != Some(workspace)
    {
        return result;
    }
    result.validated = true;
    result.defaults = native_defaults(&header["payload"]);
    let mut context_found = false;
    let mut end = metadata.len();
    let lower = end.saturating_sub(MAX_CONTEXT_SEARCH);
    let mut remainder = Vec::new();
    while end > lower && (!context_found || result.context_tokens.is_none()) {
        let start = end.saturating_sub(CONTEXT_CHUNK).max(lower);
        if file.seek(SeekFrom::Start(start)).is_err() {
            break;
        }
        let mut chunk = vec![0; (end - start) as usize];
        if file.read_exact(&mut chunk).is_err() {
            break;
        }
        chunk.extend_from_slice(&remainder);
        let mut lines = chunk.rsplit(|byte| *byte == b'\n');
        let mut incomplete = None;
        while let Some(line) = lines.next() {
            if lines.clone().next().is_none() && start > 0 {
                incomplete = Some(line.to_vec());
                break;
            }
            if line.is_empty() {
                continue;
            }
            let Ok(record) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            let payload = &record["payload"];
            if !context_found
                && matches!(
                    record["type"].as_str(),
                    Some("turn_context" | "thread_settings_applied")
                )
            {
                merge_defaults(&mut result.defaults, &native_defaults(payload));
                context_found = true;
            }
            if result.context_tokens.is_none() && payload["type"] == "token_count" {
                result.context_tokens =
                    payload["info"]["last_token_usage"]["total_tokens"].as_u64();
                if let Some(window) = payload["info"]["model_context_window"].as_u64() {
                    result.defaults.context_window_size = Some(window);
                }
            }
        }
        remainder = incomplete.unwrap_or_default();
        if remainder.len() > MAX_CONTEXT_SEARCH as usize {
            break;
        }
        end = start;
    }
    result
}

fn convert_turns(
    session: &str,
    turns: &[Value],
    raw: &Value,
) -> Result<Vec<ConversationMessage>, ProviderError> {
    let mut messages = Vec::new();
    let mut image_budget = MAX_HISTORY_BYTES;
    let mut total_bytes = 0;
    for (turn_index, turn) in turns.iter().enumerate() {
        let turn_id = turn["id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("turn-{turn_index}"));
        let started = timestamp(turn["startedAt"].as_i64().or(raw["createdAt"].as_i64()));
        let completed = turn["completedAt"]
            .as_i64()
            .map(|v| timestamp(Some(v)))
            .unwrap_or(started);
        let Some(items) = turn["items"].as_array() else {
            return Err(protocol("Codex returned a turn without its history items."));
        };
        for (item_index, item) in items.iter().enumerate() {
            let kind = item["type"].as_str().unwrap_or("unknown");
            let item_id = item["id"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("item-{item_index}"));
            let key = format!("flowx:codex:{session}:{turn_id}:{item_id}:{kind}");
            let message_id = Uuid::new_v5(&Uuid::NAMESPACE_URL, key.as_bytes());
            let mut push = |role, content: Vec<MessageContent>, result: bool| {
                let message = ConversationMessage {
                    id: if result {
                        Uuid::new_v5(&Uuid::NAMESPACE_URL, format!("{key}:result").as_bytes())
                    } else {
                        message_id
                    },
                    role,
                    content,
                    timestamp: if result || role == MessageRole::Assistant {
                        completed
                    } else {
                        started
                    },
                };
                total_bytes += serialized_size(&message)?;
                if total_bytes > MAX_HISTORY_BYTES {
                    return Err(history_size_error());
                }
                messages.push(message);
                Ok::<(), ProviderError>(())
            };
            match kind {
                "userMessage" => {
                    let mut content = Vec::new();
                    for block in item["content"].as_array().into_iter().flatten() {
                        match block["type"].as_str() {
                            Some("text") => {
                                if let Some(text) = block["text"].as_str() {
                                    content.push(MessageContent::Text {
                                        text: text.to_string(),
                                    });
                                }
                            }
                            Some("image" | "localImage") => {
                                content.push(image_content(block, &mut image_budget)?)
                            }
                            _ => content.push(MessageContent::Text {
                                text: block.to_string(),
                            }),
                        }
                    }
                    if !content.is_empty() {
                        push(MessageRole::User, content, false)?;
                    }
                }
                "agentMessage" | "plan" => {
                    if let Some(text) = item["text"].as_str().filter(|text| !text.is_empty()) {
                        push(
                            MessageRole::Assistant,
                            vec![MessageContent::Text {
                                text: text.to_string(),
                            }],
                            false,
                        )?;
                    }
                }
                "reasoning" => {} // Native internal reasoning is not a visible conversation message.
                "contextCompaction" => push(
                    MessageRole::System,
                    vec![MessageContent::Text {
                        text: "[Context compacted]".into(),
                    }],
                    false,
                )?,
                "hookPrompt" | "subAgentActivity" | "enteredReviewMode" | "exitedReviewMode" => {
                    push(
                        MessageRole::System,
                        vec![MessageContent::Text {
                            text: item.to_string(),
                        }],
                        false,
                    )?
                }
                "functionCallOutput" => push(
                    MessageRole::Tool,
                    vec![MessageContent::ToolResult {
                        id: item_id.clone(),
                        content: value_text(&item["output"]),
                        is_error: false,
                    }],
                    true,
                )?,
                _ => {
                    let is_tool = matches!(
                        kind,
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
                    ) || ["tool", "command", "input", "arguments"]
                        .iter()
                        .any(|key| !item[*key].is_null());
                    if !is_tool {
                        push(
                            MessageRole::System,
                            vec![MessageContent::Text {
                                text: item.to_string(),
                            }],
                            false,
                        )?;
                        continue;
                    }
                    let name = item["name"]
                        .as_str()
                        .or(item["tool"].as_str())
                        .unwrap_or(kind)
                        .to_string();
                    let input = super::session::tool_input(item);
                    push(
                        MessageRole::Assistant,
                        vec![MessageContent::ToolUse {
                            id: item_id.clone(),
                            name,
                            input: value_text(&input),
                        }],
                        false,
                    )?;
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
                    .map(value_text)
                    .find(|text| !text.trim().is_empty());
                    let is_error = item["status"] == "failed"
                        || item["status"] == "declined"
                        || item["success"] == false
                        || item["exitCode"].as_i64().is_some_and(|code| code != 0)
                        || !item["error"].is_null();
                    if is_error
                        || output
                            .as_deref()
                            .is_some_and(|text| text.trim() != "Completed")
                    {
                        push(
                            MessageRole::Tool,
                            vec![MessageContent::ToolResult {
                                id: item_id,
                                content: output.unwrap_or_else(|| "Tool failed".into()),
                                is_error,
                            }],
                            true,
                        )?;
                    }
                }
            }
        }
    }
    Ok(messages)
}
fn value_text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}
fn image_content(block: &Value, budget: &mut usize) -> Result<MessageContent, ProviderError> {
    let source = block["url"]
        .as_str()
        .or(block["path"].as_str())
        .unwrap_or("");
    let decoded = if let Some(data) = source.strip_prefix("data:") {
        data.split_once(";base64,").and_then(|(mime, encoded)| {
            if !matches!(
                mime,
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            ) || encoded.len() > budget.saturating_mul(4) / 3 + 4
            {
                return None;
            }
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()
                .map(|bytes| (bytes, mime.to_string()))
        })
    } else if block["type"] == "localImage" && Path::new(source).is_absolute() {
        let mime = match Path::new(source)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("png") => Some("image/png"),
            Some("jpg" | "jpeg") => Some("image/jpeg"),
            Some("gif") => Some("image/gif"),
            Some("webp") => Some("image/webp"),
            _ => None,
        };
        mime.and_then(|mime| {
            let file = std::fs::File::open(source).ok()?;
            let meta = file.metadata().ok()?;
            if !meta.is_file() || meta.len() > *budget as u64 {
                return None;
            }
            let mut data = Vec::new();
            file.take(*budget as u64 + 1).read_to_end(&mut data).ok()?;
            Some((data, mime.to_string()))
        })
    } else {
        None
    };
    if let Some((data, mime_type)) = decoded
        && !data.is_empty()
        && data.len() <= *budget
    {
        *budget -= data.len();
        return Ok(MessageContent::Image { data, mime_type });
    }
    if source.starts_with("data:") {
        return Err(protocol(
            "Codex history contains an invalid or oversized embedded image; the saved transcript was kept.",
        ));
    }
    Ok(MessageContent::Text {
        text: if block["type"] == "localImage" {
            format!("Attached image: {source}")
        } else {
            format!("[Image]\nPreview unavailable: {source}")
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fx_core::services::runtime::BinarySpec;

    const SERVER: &str = r#"
import json, os, sys
root, log = sys.argv[1:3]
def record(obj):
 with open(log,'a') as f: f.write(json.dumps(obj)+'\n')
def row(id, foreign=False):
 return {'id':id,'cwd':'/' if foreign else root,'name':id,'preview':'Preview','createdAt':1791485000,'updatedAt':1791486055,'recencyAt':1791486054,'model':'gpt-native','reasoningEffort':'high','status':{'type':'notLoaded'},'historyMode':'paginated','path':os.path.join(root,id+'.jsonl') if id.startswith('cached') else None,'turns':[]}
def turns(page):
 return [{'id':'turn-'+str(n),'startedAt':1791485000+n,'completedAt':1791485001+n,'itemsView':'full','items':[{'id':'message-'+str(n),'type':'agentMessage','text':'answer '+str(n)}]} for n in reversed(range(max(0,280-(page+1)*8),280-page*8))]
def reply(id,result): print(json.dumps({'id':id,'result':result}),flush=True)
record({'pid':os.getpid()})
for line in sys.stdin:
 q=json.loads(line); record(q); method=q.get('method'); id=q.get('id'); p=q.get('params') or {}
 if method=='initialize': reply(id,{'userAgent':'history-fixture'})
 elif method=='initialized': pass
 elif method=='thread/list':
  assert p['useStateDbOnly'] and p['modelProviders']==[]
  assert p['sourceKinds']==['cli','vscode','exec','appServer','unknown']
  page=int(p.get('cursor') or 0)
  if page==0:
   for n in range(1100): print(json.dumps({'method':'fixture/notification','params':{}}))
  archived=p['archived']; maximum=2 if archived else 22
  reply(id,{'data':[row(('archived-' if archived else 'thread-')+str(page)),row('foreign',True)],'nextCursor':str(page+1) if page+1<maximum else None})
 elif method=='thread/read':
  thread=p['threadId']; r=row(thread,thread=='foreign')
  if p.get('includeTurns'): r['turns']=list(reversed(turns(34)))
  reply(id,{'thread':r})
 elif method=='thread/turns/list':
  thread=p['threadId']; page=int(p.get('cursor') or 0)
  assert p['sortDirection']=='desc' and p['itemsView']=='full'
  if thread=='legacy': print(json.dumps({'id':id,'error':{'code':-32601,'message':'Method not found'}}),flush=True)
  elif thread=='failure' and page==1: print(json.dumps({'id':id,'error':{'code':-32000,'message':'Failed to read rollout'}}),flush=True)
  else: reply(id,{'data':turns(page),'nextCursor':'same' if thread=='cycle' else (str(page+1) if page<34 else None)})
 else: raise AssertionError('History discovery sent a forbidden method: '+str(method))
"#;

    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        log: PathBuf,
        store: CodexNativeStore,
    }
    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("project");
            std::fs::create_dir(&root).unwrap();
            let root = std::fs::canonicalize(root).unwrap();
            let script = temp.path().join("server.py");
            std::fs::write(&script, SERVER).unwrap();
            let log = temp.path().join("calls.jsonl");
            let python = std::process::Command::new("which")
                .arg("python3")
                .output()
                .unwrap();
            let spec = BinarySpec::new(
                "codex",
                "Codex",
                vec![String::from_utf8(python.stdout).unwrap().trim().into()],
            )
            .with_arguments(vec![
                script.to_string_lossy().into_owned(),
                root.to_string_lossy().into_owned(),
                log.to_string_lossy().into_owned(),
            ]);
            let store = CodexNativeStore::from_provider(CodexProvider::with_spec(
                RuntimeDiscovery::default().without_extra_directories(),
                spec,
            ));
            Self {
                _temp: temp,
                root,
                log,
                store,
            }
        }
        fn calls(&self) -> Vec<Value> {
            std::fs::read_to_string(&self.log)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
        fn cwd(&self) -> &str {
            self.root.to_str().unwrap()
        }
        fn rollout(&self, id: &str) -> PathBuf {
            let path = self.root.join(format!("{id}.jsonl"));
            std::fs::write(
                &path,
                format!(
                    "{}\n",
                    json!({"type":"session_meta","payload":{"id":id,"cwd":self.cwd()}})
                ),
            )
            .unwrap();
            path
        }
        fn history_requests(&self) -> usize {
            self.calls()
                .iter()
                .filter(|q| q["method"] == "thread/turns/list")
                .count()
        }
    }

    #[tokio::test]
    async fn unchanged_validated_rollouts_skip_history_and_changed_rollouts_refresh() {
        use std::io::Write;
        let fixture = Fixture::new();
        let path = fixture.rollout("cached");
        let first = fixture.store.read(fixture.cwd(), "cached").await.unwrap();
        let count = fixture.history_requests();
        assert_eq!(count, 35);
        let unchanged = fixture
            .store
            .read_if_changed(fixture.cwd(), "cached", Some(&first.entry.revision))
            .await
            .unwrap();
        assert!(unchanged.is_none());
        let cached = fixture.store.read(fixture.cwd(), "cached").await.unwrap();
        assert_eq!(cached.messages, first.messages);
        assert_eq!(
            fixture.history_requests(),
            count,
            "both applied revisions and cached reads avoid paging"
        );

        writeln!(
            std::fs::OpenOptions::new().append(true).open(path).unwrap(),
            "{}",
            json!({"type":"turn_context","payload":{"model":"changed-model"}})
        )
        .unwrap();
        let changed = fixture
            .store
            .read_if_changed(fixture.cwd(), "cached", Some(&first.entry.revision))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(changed.entry.revision, first.entry.revision);
        assert_eq!(
            changed.entry.defaults.model_id.as_deref(),
            Some("changed-model")
        );
        assert_eq!(fixture.history_requests(), count * 2);
        assert_eq!(fixture.store.histories.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn timestamps_alone_and_wrong_rollout_headers_never_suppress_history_refreshes() {
        let fixture = Fixture::new();
        for id in ["thread-0", "cached-invalid"] {
            if id == "cached-invalid" {
                let path = fixture.rollout(id);
                std::fs::write(path, format!("{}\n", json!({"type":"session_meta","payload":{"id":"another-thread","cwd":fixture.cwd()}}))).unwrap();
            }
            let first = fixture.store.read(fixture.cwd(), id).await.unwrap();
            let count = fixture.history_requests();
            assert!(
                fixture
                    .store
                    .read_if_changed(fixture.cwd(), id, Some(&first.entry.revision))
                    .await
                    .unwrap()
                    .is_some()
            );
            assert_eq!(fixture.history_requests(), count + 35);
        }
        assert!(fixture.store.histories.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn history_cache_keeps_only_four_recent_threads() {
        let fixture = Fixture::new();
        for number in 0..5 {
            let id = format!("cached-{number}");
            fixture.rollout(&id);
            fixture.store.read(fixture.cwd(), &id).await.unwrap();
        }
        let cache = fixture.store.histories.lock().unwrap();
        assert_eq!(cache.len(), MAX_CACHED_HISTORIES);
        assert_eq!(
            cache
                .front()
                .unwrap()
                .thread
                .entry
                .summary
                .identity
                .session_id,
            "cached-1"
        );
        assert!(cache.iter().map(|entry| entry.bytes).sum::<usize>() <= MAX_HISTORY_BYTES);
    }

    #[tokio::test]
    async fn project_index_reads_every_active_and_archived_page_and_reuses_its_connection() {
        let fixture = Fixture::new();
        let index = fixture.store.list(fixture.cwd()).await.unwrap();
        assert_eq!(
            index.threads.len(),
            24,
            "the old 20-page cap must not lose threads"
        );
        assert_eq!(
            index
                .threads
                .iter()
                .filter(|entry| entry.summary.is_archived)
                .count(),
            2
        );
        assert!(
            index
                .threads
                .iter()
                .all(|entry| entry.summary.working_directory == fixture.cwd())
        );
        let history = fixture.store.read(fixture.cwd(), "thread-0").await.unwrap();
        assert_eq!(
            history.messages.len(),
            280,
            "the old 250-message cap must not lose history"
        );
        assert_eq!(history.messages[0].text_content(), "answer 0");
        assert_eq!(
            history.messages.last().unwrap().text_content(),
            "answer 279"
        );
        assert!(!history.history_truncated);
        let again = fixture.store.read(fixture.cwd(), "thread-0").await.unwrap();
        assert_eq!(history.messages, again.messages);
        assert_eq!(
            fixture
                .calls()
                .iter()
                .filter(|q| q["method"] == "initialize")
                .count(),
            1
        );
        assert!(fixture.calls().iter().all(|q| !matches!(
            q["method"].as_str(),
            Some("thread/resume" | "thread/start" | "turn/start")
        )));
    }

    #[tokio::test]
    async fn another_projects_history_is_rejected_before_turns_are_requested() {
        let fixture = Fixture::new();
        let error = fixture
            .store
            .read(fixture.cwd(), "foreign")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("selected project"));
        assert!(
            !fixture
                .calls()
                .iter()
                .any(|q| q["method"] == "thread/turns/list")
        );
    }

    #[tokio::test]
    async fn failed_history_does_not_return_partial_messages_or_fall_back_to_legacy() {
        let fixture = Fixture::new();
        let error = fixture
            .store
            .read(fixture.cwd(), "failure")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Failed to read rollout"));
        assert!(
            !fixture
                .calls()
                .iter()
                .any(|q| q["params"]["includeTurns"] == true)
        );
        let thread = fixture.store.read(fixture.cwd(), "legacy").await.unwrap();
        assert_eq!(thread.messages.len(), 8);
        assert_eq!(
            fixture
                .calls()
                .iter()
                .filter(|q| q["method"] == "initialize")
                .count(),
            2,
            "a failed connection must be replaced"
        );
    }

    #[test]
    fn repeated_cursors_and_invalid_pages_are_explicit_errors() {
        let mut seen = HashSet::new();
        assert!(advance_cursor(&mut seen, &Some("a".into())).unwrap());
        assert!(advance_cursor(&mut seen, &Some("b".into())).unwrap());
        assert!(advance_cursor(&mut seen, &Some("a".into())).is_err());
        assert!(page_cursor(&json!({"nextCursor":3})).is_err());
        assert!(data_array(&json!({}), "thread/list").is_err());
    }

    #[test]
    fn canonical_aliases_match_but_prefix_siblings_do_not() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let sibling = temp.path().join("project-other");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let cwd = std::fs::canonicalize(&root).unwrap();
        let mut raw = json!({"id":"thread","cwd":alias,"preview":"First line\nSecond line","updatedAt":1791486055,"recencyAt":1791485339,"model":"gpt-native","reasoningEffort":"high"});
        let entry = summary(&raw, &cwd, true).unwrap().unwrap();
        assert_eq!(entry.summary.title, "First line");
        assert_eq!(entry.summary.updated_at.timestamp(), 1791485339);
        assert!(entry.summary.is_archived);
        raw["cwd"] = json!(sibling);
        assert!(summary(&raw, &cwd, false).unwrap().is_none());
        raw["cwd"] = json!("project");
        assert!(summary(&raw, &cwd, false).unwrap().is_none());
    }

    #[test]
    fn native_items_preserve_roles_commands_results_errors_and_embedded_images() {
        // Exact field shapes from Codex 0.155.1 thread/turns/list. Values replaced
        // with test content so no user history or account data enters the repo.
        let turns = json!([{"id":"turn","startedAt":1791485000,"completedAt":1791485010,"itemsView":"full","items":[
            {"type":"userMessage","id":"user","clientId":null,"content":[{"type":"text","text":"Inspect this image"},{"type":"image","url":"data:image/png;base64,aW1hZ2U="}]},
            {"type":"agentMessage","id":"a1","text":"Checking.","phase":"commentary","memoryCitation":null,"delivery":null,"questions":null},
            {"type":"commandExecution","id":"command","command":"cargo test","cwd":"/project","status":"completed","commandActions":[],"aggregatedOutput":"all passed","exitCode":0,"durationMs":10},
            {"type":"mcpToolCall","id":"mcp","server":"docs","tool":"read","status":"failed","arguments":{"id":"doc"},"result":null,"error":{"message":"missing"}},
            {"type":"dynamicToolCall","id":"dynamic","tool":"render","arguments":{"file":"chart"},"status":"completed","success":true,"contentItems":[{"type":"text","text":"saved"}]},
            {"type":"functionCallOutput","id":"function","name":"f","output":"separate result"},
            {"type":"agentMessage","id":"a2","text":"Done.","phase":"final"}
        ]}]);
        let messages = convert_turns("thread", turns.as_array().unwrap(), &json!({})).unwrap();
        assert_eq!(messages.len(), 10);
        assert_eq!(messages[0].role, MessageRole::User);
        assert!(messages[0].contains_image());
        assert_eq!(messages[1].role, MessageRole::Assistant);
        assert_eq!(messages[3].role, MessageRole::Tool);
        assert!(
            matches!(&messages[2].content[0],MessageContent::ToolUse{input,..} if input.contains("cargo test"))
        );
        assert!(
            matches!(&messages[3].content[0],MessageContent::ToolResult{content,is_error:false,..} if content=="all passed")
        );
        assert!(
            matches!(&messages[5].content[0],MessageContent::ToolResult{content,is_error:true,..} if content.contains("missing"))
        );
        let again = convert_turns(
            "thread",
            turns.as_array().unwrap(),
            &json!({"updatedAt":1791489999}),
        )
        .unwrap();
        assert_eq!(messages, again);
        let other = convert_turns("other-thread", turns.as_array().unwrap(), &json!({})).unwrap();
        assert_ne!(messages[0].id, other[0].id);
    }

    #[test]
    fn native_command_details_match_live_items_even_with_separate_input_fields() {
        let item = json!({"type":"commandExecution", "id":"command", "command":["cat","file with spaces.txt"], "cwd":"/project", "input":{"timeout_ms":1000}, "status":"completed", "aggregatedOutput":"contents"});
        let turns = json!([{"id":"turn", "items":[item]}]);
        let messages = convert_turns("thread", turns.as_array().unwrap(), &json!({})).unwrap();
        let MessageContent::ToolUse { input, .. } = &messages[0].content[0] else {
            panic!("missing tool use");
        };
        let native_input: Value = serde_json::from_str(input).unwrap();
        assert_eq!(native_input["command"], "cat file with spaces.txt");
        assert_eq!(
            native_input["command_argv"],
            json!(["cat", "file with spaces.txt"])
        );
        assert_eq!(native_input["timeout_ms"], 1000);
        assert_eq!(native_input["cwd"], "/project");
        let events =
            super::super::session::parse_notification("item/started", &json!({"item":item}));
        let crate::ProviderEvent::ToolUse { input, .. } = &events[0] else {
            panic!("missing live tool use");
        };
        assert_eq!(native_input, serde_json::from_str::<Value>(input).unwrap());
        assert!(
            matches!(&messages[1].content[0], MessageContent::ToolResult { content, .. } if content == "contents")
        );
    }

    #[test]
    fn missing_images_remain_visible_and_large_text_is_preserved_completely() {
        let turns = json!([{"id":"turn","items":[{"id":"u","type":"userMessage","content":[{"type":"localImage","path":"/does/not/exist.png"},{"type":"image","url":"https://example.invalid/image.png"}]},{"id":"a","type":"agentMessage","text":"é".repeat(150000)}]}]);
        let messages = convert_turns("thread", turns.as_array().unwrap(), &json!({})).unwrap();
        assert_eq!(messages.len(), 2);
        assert!(messages[0].contains_image());
        assert_eq!(messages[1].text_content(), "é".repeat(150000));
        assert!(
            messages[0]
                .text_content()
                .contains("https://example.invalid/image.png")
        );
    }

    #[test]
    fn rollout_defaults_and_usage_are_validated_and_read_across_chunk_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rollout.jsonl");
        let cwd = std::fs::canonicalize(temp.path()).unwrap();
        let mut lines = vec![
            json!({"type":"session_meta","payload":{"id":"thread","cwd":cwd,"context_window":128000}}),
            json!({"type":"turn_context","payload":{"model":"old","effort":"low","approval_policy":"never","sandbox_policy":{"type":"danger-full-access"}}}),
            json!({"type":"turn_context","payload":{"model":"current","effort":"high","approval_policy":"on-request","sandbox_policy":{"type":"workspace-write"}}}),
            json!({"type":"response_item","payload":{"padding":"x".repeat(CONTEXT_CHUNK as usize+60)}}),
            json!({"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":456},"total_token_usage":{"total_tokens":99999},"model_context_window":200000}}}),
        ];
        let write = |lines: &[Value]| {
            std::fs::write(
                &path,
                lines
                    .iter()
                    .map(|line| format!("{line}\n"))
                    .collect::<String>(),
            )
            .unwrap()
        };
        write(&lines);
        let fingerprint = file_fingerprint(&path).unwrap();
        let metadata = read_rollout_metadata(&path, "thread", &cwd, fingerprint.clone());
        assert_eq!(metadata.defaults.model_id.as_deref(), Some("current"));
        assert_eq!(
            metadata.defaults.agent_access,
            Some(AgentAccess::AcceptEdits)
        );
        assert_eq!(metadata.context_tokens, Some(456));
        assert_eq!(metadata.defaults.context_window_size, Some(200000));
        let wrong = read_rollout_metadata(&path, "different-thread", &cwd, fingerprint.clone());
        assert_eq!(wrong.defaults, NativeDefaults::default());
        lines.push(json!({"type":"event_msg","payload":{"type":"changed"}}));
        write(&lines);
        assert_ne!(file_fingerprint(&path).unwrap(), fingerprint);
    }

    #[tokio::test]
    #[ignore = "reads existing installed Codex history without starting a turn"]
    async fn installed_codex_lists_and_reads_this_projects_history() {
        let cwd =
            std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap();
        let store = CodexNativeStore::new(RuntimeDiscovery::default());
        let index = store.list(cwd.to_str().unwrap()).await.unwrap();
        println!(
            "Codex native threads: {}; warnings: {}",
            index.threads.len(),
            index.warnings.len()
        );
        if let Some(entry) = index.threads.first() {
            let thread = store
                .read(cwd.to_str().unwrap(), &entry.summary.identity.session_id)
                .await
                .unwrap();
            println!(
                "Messages: {}; partial: {}; model: {:?}; effort: {:?}; access: {:?}; context: {:?}",
                thread.messages.len(),
                thread.history_truncated,
                thread.entry.defaults.model_id,
                thread.entry.defaults.effort,
                thread.entry.defaults.agent_access,
                thread.current_context_tokens
            );
            assert!(thread.messages.iter().all(|message| !message.id.is_nil()));
        }
    }
}
