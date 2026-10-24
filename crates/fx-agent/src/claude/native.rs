//! Read-only Claude Code history, scoped to one canonical workspace.
//!
//! JSONL is streamed from the beginning: a thread never silently becomes the
//! last 250 messages. Resource limits reject an oversized transcript explicitly.
//! The final incomplete append is ignored until the writer completes it.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, Metadata};
use std::io::{BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use base64::Engine;
use chrono::{DateTime, Utc};
use fx_core::{
    AgentAccess, ConversationMessage, MessageContent, MessageRole, NativeDefaults, Timestamp,
};
use serde_json::Value;
use uuid::Uuid;

use crate::native::{NativeThread, NativeThreadIndex, NativeThreadSummary};
use crate::{ProviderError, ThreadIdentity, ThreadSummary};

const MAX_TRANSCRIPT_BYTES: u64 = 128 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RECORDS: usize = 100_000;
const MAX_FILES: usize = 10_000;
const MAX_METADATA_BYTES: u64 = 1024 * 1024;
const MAX_SUMMARIES: usize = 2_000;
const MAX_CACHED_TRANSCRIPTS: usize = 4;
const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;

/// Keep one instance per history worker so unchanged files reuse their parsing.
pub struct ClaudeNativeStore {
    config_root: PathBuf,
    desktop_root: Option<PathBuf>,
    cache: Mutex<Cache>,
    maximum_transcript_bytes: u64,
}

#[derive(Default)]
struct Cache {
    summaries: HashMap<PathBuf, CachedSummary>,
    transcripts: HashMap<PathBuf, CachedTranscript>,
    desktop: HashMap<PathBuf, (Fingerprint, Option<(String, bool)>)>,
    tick: u64,
    #[cfg(test)]
    parses: usize,
}

struct CachedSummary {
    fingerprint: Fingerprint,
    canonical: PathBuf,
    parsed: ParsedSummary,
    accessed: u64,
}

struct CachedTranscript {
    fingerprint: Fingerprint,
    messages: Vec<ConversationMessage>,
    accessed: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Fingerprint {
    size: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl Fingerprint {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            size: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            identity: {
                use std::os::unix::fs::MetadataExt;
                (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                )
            },
        }
    }

    fn revision(&self, path: &Path) -> String {
        Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            format!("{}:{self:?}", path.display()).as_bytes(),
        )
        .to_string()
    }
}

#[derive(Clone, Default)]
struct ParsedSummary {
    title: Option<String>,
    first_prompt: Option<String>,
    last_prompt: Option<String>,
    defaults: NativeDefaults,
    current_context_tokens: Option<u64>,
    updated_at: Option<Timestamp>,
    history_truncated: bool,
}

#[derive(Default)]
struct DesktopState {
    sessions: HashMap<String, (bool, String, Option<SystemTime>)>,
    warnings: Vec<String>,
}

impl Default for ClaudeNativeStore {
    fn default() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let root = std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        Self::new(root).with_desktop_sessions_root(
            home.join("Library/Application Support/Claude/claude-code-sessions"),
        )
    }
}

impl ClaudeNativeStore {
    /// Isolated root, without inspecting the user's Claude desktop installation.
    pub fn new(config_root: impl Into<PathBuf>) -> Self {
        Self {
            config_root: config_root.into(),
            desktop_root: None,
            cache: Mutex::default(),
            maximum_transcript_bytes: MAX_TRANSCRIPT_BYTES,
        }
    }

    pub fn with_desktop_sessions_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.desktop_root = Some(root.into());
        self
    }

    pub fn list(&self, working_directory: &str) -> Result<NativeThreadIndex, ProviderError> {
        let (canonical, directories) = self.project_directories(working_directory)?;
        let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
        let desktop = self.desktop_state(&mut cache)?;
        let mut result = NativeThreadIndex {
            warnings: desktop.warnings.clone(),
            ..Default::default()
        };
        let mut found = HashMap::<String, NativeThreadSummary>::new();
        let mut current_paths = HashSet::new();
        for directory in &directories {
            for path in children(directory)? {
                if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(id) = path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .and_then(safe_session_id)
                else {
                    continue;
                };
                current_paths.insert(path.clone());
                let parsed = self.summary(&path, &canonical, &id, &mut cache);
                match parsed {
                    Ok((summary, fingerprint)) => {
                        let entry =
                            make_entry(&path, &id, &canonical, &summary, &fingerprint, &desktop);
                        if found
                            .get(&id)
                            .is_none_or(|old| old.summary.updated_at < entry.summary.updated_at)
                        {
                            found.insert(id, entry);
                        }
                    }
                    Err(error) => result.warnings.push(format!("Claude thread {id}: {error}")),
                }
            }
        }
        // Removed files must not retain large cached transcripts indefinitely.
        cache.summaries.retain(|path, _| {
            !directories
                .iter()
                .any(|directory| path.parent() == Some(directory))
                || current_paths.contains(path)
        });
        cache.transcripts.retain(|path, _| {
            !directories
                .iter()
                .any(|directory| path.parent() == Some(directory))
                || current_paths.contains(path)
        });
        result.threads = found.into_values().collect();
        result.threads.sort_by(|a, b| {
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
        Ok(result)
    }

    pub fn read(
        &self,
        working_directory: &str,
        session_id: &str,
    ) -> Result<NativeThread, ProviderError> {
        self.read_if_changed(working_directory, session_id, None)?
            .ok_or_else(|| protocol("Claude history was unexpectedly unchanged"))
    }

    /// Poll metadata before copying cached messages. The caller retains the
    /// revision only after successfully applying a complete native history.
    pub fn read_if_changed(
        &self,
        working_directory: &str,
        session_id: &str,
        known_revision: Option<&str>,
    ) -> Result<Option<NativeThread>, ProviderError> {
        let id =
            safe_session_id(session_id).ok_or_else(|| protocol("Invalid Claude session ID"))?;
        let (canonical, directories) = self.project_directories(working_directory)?;
        let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
        let desktop = self.desktop_state(&mut cache)?;
        let mut best = None;
        for directory in directories {
            // Native UUID filenames are normally lower-case. Match UUID values
            // rather than accepting arbitrary caller-supplied path components.
            for path in children(&directory)? {
                if path.extension().and_then(|value| value.to_str()) != Some("jsonl")
                    || path
                        .file_stem()
                        .and_then(|value| value.to_str())
                        .and_then(safe_session_id)
                        .as_deref()
                        != Some(&id)
                {
                    continue;
                }
                let (summary, fingerprint) = self.summary(&path, &canonical, &id, &mut cache)?;
                let entry = make_entry(&path, &id, &canonical, &summary, &fingerprint, &desktop);
                if best.as_ref().is_none_or(
                    |(_, old, _, _): &(
                        PathBuf,
                        NativeThreadSummary,
                        ParsedSummary,
                        Fingerprint,
                    )| old.summary.updated_at < entry.summary.updated_at,
                ) {
                    best = Some((path, entry, summary, fingerprint));
                }
            }
        }
        let Some((path, entry, summary, fingerprint)) = best else {
            return Err(protocol("Claude session was not found in this workspace"));
        };
        if known_revision == Some(entry.revision.as_str()) {
            return Ok(None);
        }
        cache.tick += 1;
        let tick = cache.tick;
        if let Some(cached) = cache.transcripts.get_mut(&path)
            && cached.fingerprint == fingerprint
        {
            cached.accessed = tick;
            return Ok(Some(NativeThread {
                entry,
                messages: cached.messages.clone(),
                current_context_tokens: summary.current_context_tokens,
                history_truncated: summary.history_truncated,
            }));
        }
        let (parsed, messages) = parse_file(
            &path,
            &canonical,
            &id,
            &fingerprint,
            self.maximum_transcript_bytes,
            true,
        )?;
        #[cfg(test)]
        {
            cache.parses += 1;
        }
        if fingerprint.size <= MAX_CACHE_BYTES {
            cache.transcripts.insert(
                path.clone(),
                CachedTranscript {
                    fingerprint: fingerprint.clone(),
                    messages: messages.clone(),
                    accessed: tick,
                },
            );
            while cache.transcripts.len() > MAX_CACHED_TRANSCRIPTS
                || cache
                    .transcripts
                    .values()
                    .map(|value| value.fingerprint.size)
                    .sum::<u64>()
                    > MAX_CACHE_BYTES
            {
                let oldest = cache
                    .transcripts
                    .iter()
                    .min_by_key(|(_, value)| value.accessed)
                    .map(|(key, _)| key.clone())
                    .unwrap();
                cache.transcripts.remove(&oldest);
            }
        }
        Ok(Some(NativeThread {
            entry: make_entry(&path, &id, &canonical, &parsed, &fingerprint, &desktop),
            messages,
            current_context_tokens: parsed.current_context_tokens,
            history_truncated: parsed.history_truncated,
        }))
    }

    fn summary(
        &self,
        path: &Path,
        canonical: &Path,
        id: &str,
        cache: &mut Cache,
    ) -> Result<(ParsedSummary, Fingerprint), ProviderError> {
        let fingerprint = regular_fingerprint(path)?;
        cache.tick += 1;
        if let Some(cached) = cache.summaries.get_mut(path)
            && cached.fingerprint == fingerprint
        {
            if cached.canonical != canonical {
                return Err(protocol("Transcript belongs to a different workspace"));
            }
            cached.accessed = cache.tick;
            return Ok((cached.parsed.clone(), fingerprint));
        }
        let (parsed, _) = parse_file(
            path,
            canonical,
            id,
            &fingerprint,
            self.maximum_transcript_bytes,
            false,
        )?;
        #[cfg(test)]
        {
            cache.parses += 1;
        }
        cache.summaries.insert(
            path.to_path_buf(),
            CachedSummary {
                fingerprint: fingerprint.clone(),
                canonical: canonical.to_path_buf(),
                parsed: parsed.clone(),
                accessed: cache.tick,
            },
        );
        while cache.summaries.len() > MAX_SUMMARIES {
            let oldest = cache
                .summaries
                .iter()
                .min_by_key(|(_, value)| value.accessed)
                .map(|(key, _)| key.clone())
                .unwrap();
            cache.summaries.remove(&oldest);
        }
        Ok((parsed, fingerprint))
    }

    fn project_directories(
        &self,
        working_directory: &str,
    ) -> Result<(PathBuf, Vec<PathBuf>), ProviderError> {
        let configured = absolute_normalized(Path::new(working_directory))?;
        let canonical = fs::canonicalize(&configured)?;
        if !canonical.is_dir() {
            return Err(protocol("Claude workspace must be an existing directory"));
        }
        let projects = self.config_root.join("projects");
        let mut directories = Vec::new();
        for root in [&configured, &canonical] {
            let path = projects.join(project_key(root));
            match fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    if !metadata.file_type().is_dir() {
                        return Err(protocol(
                            "Claude project history directory is not a regular directory",
                        ));
                    }
                    let path = fs::canonicalize(path)?;
                    if path.parent() != Some(fs::canonicalize(&projects)?.as_path()) {
                        return Err(protocol(
                            "Claude project history escaped its configured root",
                        ));
                    }
                    if !directories.contains(&path) {
                        directories.push(path);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok((canonical, directories))
    }

    fn desktop_state(&self, cache: &mut Cache) -> Result<DesktopState, ProviderError> {
        let mut state = DesktopState::default();
        let Some(root) = &self.desktop_root else {
            return Ok(state);
        };
        let mut seen = HashSet::new();
        let mut files = 0;
        for account in children(root)? {
            if !is_plain_directory(&account) {
                continue;
            }
            for organization in children(&account)? {
                if !is_plain_directory(&organization) {
                    continue;
                }
                for file in children(&organization)? {
                    let name = file
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or_default();
                    if !name.starts_with("local_") || !name.ends_with(".json") {
                        continue;
                    }
                    files += 1;
                    if files > MAX_FILES {
                        return Err(protocol(
                            "Claude desktop metadata exceeds the 10,000-file limit",
                        ));
                    }
                    seen.insert(file.clone());
                    let mut fingerprint = match regular_fingerprint(&file) {
                        Ok(value) => value,
                        Err(_) => {
                            state.warnings.push(
                                "A Claude desktop metadata file is unreadable or unsafe".into(),
                            );
                            continue;
                        }
                    };
                    let metadata = if let Some((old, parsed)) = cache.desktop.get(&file)
                        && old == &fingerprint
                    {
                        parsed.clone()
                    } else {
                        let result = read_metadata(&file, &fingerprint);
                        match result {
                            Ok(parsed) => {
                                cache
                                    .desktop
                                    .insert(file.clone(), (fingerprint.clone(), parsed.clone()));
                                parsed
                            }
                            Err(_) => {
                                state.warnings.push("A Claude desktop metadata file is invalid; archive status may be incomplete".into());
                                // Desktop writes can be interrupted too. Keep
                                // the last verified archive state until this
                                // file becomes readable instead of restoring
                                // an archived thread on a partial JSON write.
                                let Some((previous, parsed)) = cache.desktop.get(&file) else {
                                    continue;
                                };
                                fingerprint = previous.clone();
                                parsed.clone()
                            }
                        }
                    };
                    if let Some((id, archived)) = metadata {
                        let next = (archived, fingerprint.revision(&file), fingerprint.modified);
                        if state.sessions.get(&id).is_none_or(|old| old.2 <= next.2) {
                            state.sessions.insert(id, next);
                        }
                    }
                }
            }
        }
        cache.desktop.retain(|path, _| seen.contains(path));
        Ok(state)
    }
}

fn make_entry(
    path: &Path,
    id: &str,
    canonical: &Path,
    parsed: &ParsedSummary,
    fingerprint: &Fingerprint,
    desktop: &DesktopState,
) -> NativeThreadSummary {
    let desktop = desktop.sessions.get(id);
    let title = parsed
        .title
        .as_deref()
        .or(parsed.first_prompt.as_deref())
        .or(parsed.last_prompt.as_deref())
        .unwrap_or("Claude thread");
    NativeThreadSummary {
        summary: ThreadSummary {
            identity: ThreadIdentity {
                provider_id: "claude".into(),
                session_id: id.into(),
            },
            title: title
                .lines()
                .next()
                .unwrap_or("Claude thread")
                .chars()
                .take(200)
                .collect(),
            working_directory: canonical.to_string_lossy().into_owned(),
            updated_at: parsed.updated_at.unwrap_or_else(|| {
                fingerprint
                    .modified
                    .map(DateTime::<Utc>::from)
                    .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
            }),
            is_archived: desktop.is_some_and(|state| state.0),
        },
        defaults: parsed.defaults.clone(),
        revision: format!(
            "{}:{}",
            fingerprint.revision(path),
            desktop
                .map(|state| state.1.as_str())
                .unwrap_or("no-desktop-metadata")
        ),
    }
}

fn parse_file(
    path: &Path,
    canonical: &Path,
    id: &str,
    fingerprint: &Fingerprint,
    maximum_bytes: u64,
    include_messages: bool,
) -> Result<(ParsedSummary, Vec<ConversationMessage>), ProviderError> {
    if fingerprint.size > maximum_bytes {
        return Err(protocol(format!(
            "Transcript exceeds the {maximum_bytes}-byte history limit; no partial history was imported"
        )));
    }
    let file = checked_file(path, fingerprint)?;
    let mut reader = BufReader::new(file.take(fingerprint.size));
    let mut parsed = ParsedSummary {
        defaults: NativeDefaults {
            provider_id: Some("claude".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut messages = Vec::<ConversationMessage>::new();
    let mut positions = HashMap::new();
    let mut cwd_seen = false;
    // Whether the first record naming a directory has been checked.
    let mut started = false;
    let mut line = Vec::new();
    let mut line_number = 0;
    loop {
        line.clear();
        // Take is on the line read itself, so a missing newline cannot allocate
        // an unbounded buffer before the size check runs.
        let count = reader
            .by_ref()
            .take((MAX_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        line_number += 1;
        if line_number > MAX_RECORDS {
            return Err(protocol(
                "Transcript exceeds the 100,000-record history limit",
            ));
        }
        if line.len() > MAX_LINE_BYTES {
            return Err(protocol(format!(
                "Transcript record {line_number} exceeds the 16 MiB line limit"
            )));
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let object: Value = match serde_json::from_slice(&line) {
            Ok(value) => value,
            Err(error) if !line.ends_with(b"\n") && error.is_eof() => {
                parsed.history_truncated = true;
                break;
            }
            Err(_) => {
                return Err(protocol(format!(
                    "Invalid Claude history JSON at record {line_number}"
                )));
            }
        };
        if !object.is_object() {
            return Err(protocol(format!(
                "Invalid Claude history object at record {line_number}"
            )));
        }
        if object["isSidechain"].as_bool() == Some(true) {
            continue;
        }
        if let Some(session) = object["sessionId"].as_str()
            && safe_session_id(session).as_deref() != Some(id)
        {
            return Err(protocol(
                "Transcript session ID does not match its filename",
            ));
        }
        // Where a session started decides whose it is, which is also what
        // keeps a workspace whose name encodes the same from claiming it.
        // Later records may name any directory: a shell that ran `cd`, or
        // Claude desktop running a side session, moves the session's working
        // directory without moving the thread. Rejecting those hid real
        // threads behind a sync warning that retrying could never clear.
        if let Some(cwd) = object["cwd"].as_str() {
            if !started {
                let cwd = canonical_history_directory(Path::new(cwd))?;
                if cwd != canonical && !cwd.starts_with(canonical) {
                    return Err(protocol("Transcript belongs to a different workspace"));
                }
                started = true;
                cwd_seen = cwd == canonical;
            } else if !cwd_seen
                && canonical_history_directory(Path::new(cwd)).is_ok_and(|cwd| cwd == canonical)
            {
                cwd_seen = true;
            }
        }
        if let Some(timestamp) = parse_timestamp(&object["timestamp"]) {
            parsed.updated_at = Some(
                parsed
                    .updated_at
                    .map_or(timestamp, |old| old.max(timestamp)),
            );
        }
        if object.get("model").is_some() {
            parsed.defaults.model_id = nonempty(&object["model"]);
        }
        if object.get("effort").is_some() {
            parsed.defaults.effort = nonempty(&object["effort"]);
        }
        if object.get("permissionMode").is_some() {
            parsed.defaults.agent_access = access(&object["permissionMode"]);
        }
        match object["type"].as_str().unwrap_or_default() {
            "ai-title" => {
                if let Some(title) = nonempty(&object["aiTitle"]) {
                    parsed.title = Some(title);
                }
            }
            "custom-title" => {
                if let Some(title) = nonempty(&object["customTitle"])
                    .or_else(|| nonempty(&object["title"]))
                    .or_else(|| nonempty(&object["name"]))
                {
                    parsed.title = Some(title);
                }
            }
            "last-prompt" => {
                parsed.last_prompt = nonempty(&object["lastPrompt"]);
            }
            "user" | "assistant" => {
                if object["isMeta"].as_bool() == Some(true) {
                    continue;
                }
                let assistant = object["type"] == "assistant";
                let message = &object["message"];
                if !message.is_object()
                    || !(message["content"].is_string() || message["content"].is_array())
                {
                    return Err(protocol(format!(
                        "Invalid Claude message content at record {line_number}"
                    )));
                }
                if assistant {
                    if message.get("model").is_some() {
                        parsed.defaults.model_id = nonempty(&message["model"]);
                    }
                    if let Some(usage) = context_usage(&message["usage"]) {
                        parsed.current_context_tokens = Some(usage);
                    }
                } else if let Some(prompt) = prompt_text(&message["content"]) {
                    if parsed.first_prompt.is_none() {
                        parsed.first_prompt = Some(prompt.clone());
                    }
                    parsed.last_prompt = Some(prompt);
                }
                if !include_messages {
                    continue;
                }
                let content = contents(&message["content"], assistant)?;
                if content.is_empty() {
                    continue;
                }
                if let Some(patch) = object["toolUseResult"]["structuredPatch"]
                    .as_array()
                    .filter(|patch| !patch.is_empty())
                {
                    enrich_edit(&mut messages, &content, &object["parentUuid"], patch);
                }
                let role = if assistant {
                    MessageRole::Assistant
                } else if content.iter().all(MessageContent::is_tool_result) {
                    MessageRole::Tool
                } else {
                    MessageRole::User
                };
                let native_id = object["uuid"].as_str();
                let message_id = native_id
                    .and_then(|id| Uuid::parse_str(id).ok())
                    .unwrap_or_else(|| {
                        let namespace = Uuid::parse_str(id).expect("validated session ID");
                        Uuid::new_v5(&namespace, native_id.map(str::as_bytes).unwrap_or(&line))
                    });
                let message = ConversationMessage {
                    id: message_id,
                    role,
                    content,
                    timestamp: parse_timestamp(&object["timestamp"])
                        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH),
                };
                // Assistant message.id repeats across different output blocks.
                // Only the JSONL event UUID is a message identity.
                if let Some(index) = positions.get(&message_id).copied() {
                    messages[index] = message;
                } else {
                    positions.insert(message_id, messages.len());
                    messages.push(message);
                }
            }
            _ => {}
        }
    }
    if !cwd_seen {
        return Err(protocol("Transcript has no verifiable workspace identity"));
    }
    // A concurrent append, replacement, or rewrite must be retried instead of
    // publishing an older prefix as the history for a newer fingerprint.
    let after = regular_fingerprint(path)?;
    if after != *fingerprint {
        return Err(protocol(
            "Claude history changed while being read; refresh to retry",
        ));
    }
    Ok((parsed, messages))
}

fn contents(value: &Value, assistant: bool) -> Result<Vec<MessageContent>, ProviderError> {
    if let Some(text) = value.as_str() {
        return Ok(if text.trim().is_empty() {
            Vec::new()
        } else {
            vec![MessageContent::Text { text: text.into() }]
        });
    }
    let mut out = Vec::new();
    for block in value.as_array().into_iter().flatten() {
        match block["type"].as_str().unwrap_or_default() {
            "text" => {
                if let Some(text) = block["text"]
                    .as_str()
                    .filter(|text| !text.trim().is_empty())
                {
                    out.push(MessageContent::Text { text: text.into() });
                }
            }
            "tool_use" if assistant => out.push(MessageContent::ToolUse {
                id: block["id"].as_str().unwrap_or_default().into(),
                name: block["name"].as_str().unwrap_or("Tool").into(),
                input: serde_json::to_string(
                    block
                        .get("input")
                        .unwrap_or(&Value::Object(Default::default())),
                )
                .map_err(|_| protocol("Invalid tool input"))?,
            }),
            "tool_result" => out.push(MessageContent::ToolResult {
                id: block["tool_use_id"].as_str().unwrap_or_default().into(),
                content: content_string(&block["content"]),
                is_error: block["is_error"].as_bool().unwrap_or(false),
            }),
            "image" => {
                let source = &block["source"];
                if source["type"] == "base64" {
                    let encoded = source["data"]
                        .as_str()
                        .ok_or_else(|| protocol("Invalid Claude image data"))?;
                    let data = base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .map_err(|_| protocol("Invalid Claude image encoding"))?;
                    let mime_type = source["media_type"]
                        .as_str()
                        .unwrap_or("application/octet-stream")
                        .to_string();
                    out.push(MessageContent::Image { data, mime_type });
                } else {
                    out.push(MessageContent::Text {
                        text: "[Image]".into(),
                    });
                }
            }
            // Thinking and signed/redacted thinking are provider internals, not
            // visible dialogue. Unknown blocks remain explicit as JSON text.
            "thinking" | "redacted_thinking" => {}
            _ => out.push(MessageContent::Text {
                text: content_string(block),
            }),
        }
    }
    Ok(out)
}

fn enrich_edit(
    messages: &mut [ConversationMessage],
    results: &[MessageContent],
    parent: &Value,
    patch: &[Value],
) {
    let ids: HashSet<_> = results
        .iter()
        .filter_map(|content| match content {
            MessageContent::ToolResult { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    let parent = parent.as_str().and_then(|id| Uuid::parse_str(id).ok());
    if ids.is_empty() && parent.is_none() {
        return;
    }
    for message in messages.iter_mut().rev() {
        if parent.is_some_and(|parent| parent != message.id) {
            continue;
        }
        for content in message.content.iter_mut().rev() {
            if let MessageContent::ToolUse { id, name, input } = content
                && name.eq_ignore_ascii_case("Edit")
                && (ids.is_empty() || ids.contains(id.as_str()))
                && let Ok(Value::Object(mut object)) = serde_json::from_str(input)
            {
                object.insert("_flowxStructuredPatch".into(), Value::Array(patch.to_vec()));
                *input = Value::Object(object).to_string();
                return;
            }
        }
    }
}

fn content_string(value: &Value) -> String {
    if value.is_null() {
        return String::new();
    }
    if let Some(text) = value.as_str() {
        return text.into();
    }
    if let Some(blocks) = value.as_array()
        && blocks
            .iter()
            .all(|block| block["type"] == "text" && block["text"].is_string())
    {
        return blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
    }
    value.to_string()
}

fn prompt_text(value: &Value) -> Option<String> {
    let text = value.as_str().map(str::to_owned).or_else(|| {
        value.as_array().map(|blocks| {
            blocks
                .iter()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
    })?;
    let text = text.trim();
    (!text.is_empty()).then(|| {
        text.lines()
            .next()
            .unwrap_or(text)
            .chars()
            .take(200)
            .collect()
    })
}

fn context_usage(value: &Value) -> Option<u64> {
    let mut total = None::<u64>;
    for key in [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
        "output_tokens",
    ] {
        if let Some(value) = value[key].as_u64() {
            total = Some(total.unwrap_or(0).saturating_add(value));
        }
    }
    total
}

fn access(value: &Value) -> Option<AgentAccess> {
    match value.as_str()? {
        "bypassPermissions" => Some(AgentAccess::FullAccess),
        "acceptEdits" => Some(AgentAccess::AcceptEdits),
        "default" | "supervised" | "plan" => Some(AgentAccess::Supervised),
        _ => None,
    }
}

fn nonempty(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(4096).collect())
}
fn parse_timestamp(value: &Value) -> Option<Timestamp> {
    DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}
fn protocol(message: impl Into<String>) -> ProviderError {
    ProviderError::Protocol(message.into())
}
fn safe_session_id(id: &str) -> Option<String> {
    (id.len() == 36)
        .then(|| Uuid::parse_str(id).ok())
        .flatten()
        .map(|id| id.to_string())
}
fn project_key(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn absolute_normalized(path: &Path) -> Result<PathBuf, ProviderError> {
    if !path.is_absolute() {
        return Err(protocol("Claude workspace must be an absolute path"));
    }
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            component => result.push(component.as_os_str()),
        }
    }
    Ok(result)
}

/// A session can cd into a project subdirectory, including one removed since
/// that turn. Resolve existing symlinks while retaining the missing suffix;
/// accepting lexical prefixes alone would let a symlink escape the workspace.
fn canonical_history_directory(path: &Path) -> Result<PathBuf, ProviderError> {
    let mut path = absolute_normalized(path)?;
    let mut missing = Vec::new();
    loop {
        match fs::canonicalize(&path) {
            Ok(mut canonical) => {
                for part in missing.into_iter().rev() {
                    canonical.push(part);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = path.file_name().map(|value| value.to_os_string()) else {
                    return Err(error.into());
                };
                missing.push(name);
                path.pop();
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn is_plain_directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}
fn children(path: &Path) -> Result<Vec<PathBuf>, ProviderError> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut paths = Vec::new();
    for entry in entries {
        if paths.len() >= MAX_FILES {
            return Err(protocol(
                "Claude history directory exceeds the 10,000-entry limit",
            ));
        }
        paths.push(entry?.path());
    }
    paths.sort();
    Ok(paths)
}
fn regular_fingerprint(path: &Path) -> Result<Fingerprint, ProviderError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(protocol(
            "Claude history file is not a regular file (symlinks are not followed)",
        ));
    }
    Ok(Fingerprint::from_metadata(&metadata))
}
fn checked_file(path: &Path, expected: &Fingerprint) -> Result<File, ProviderError> {
    let file = File::open(path)?;
    if Fingerprint::from_metadata(&file.metadata()?) != *expected {
        return Err(protocol(
            "Claude history file changed before it could be opened",
        ));
    }
    Ok(file)
}
fn read_metadata(
    path: &Path,
    fingerprint: &Fingerprint,
) -> Result<Option<(String, bool)>, ProviderError> {
    if fingerprint.size > MAX_METADATA_BYTES {
        return Err(protocol("Claude desktop metadata exceeds the 1 MiB limit"));
    }
    let mut data = Vec::new();
    checked_file(path, fingerprint)?
        .take(MAX_METADATA_BYTES + 1)
        .read_to_end(&mut data)?;
    if data.len() as u64 > MAX_METADATA_BYTES {
        return Err(protocol(
            "Claude desktop metadata grew beyond its size limit",
        ));
    }
    let object: Value = serde_json::from_slice(&data)
        .map_err(|_| protocol("Invalid Claude desktop metadata JSON"))?;
    if regular_fingerprint(path)? != *fingerprint {
        return Err(protocol("Claude desktop metadata changed while being read"));
    }
    let Some(id) = object["cliSessionId"].as_str().and_then(safe_session_id) else {
        return Ok(None);
    };
    Ok(Some((id, object["isArchived"].as_bool().unwrap_or(false))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    const SESSION: &str = "11111111-2222-4333-8444-555555555555";

    struct Fixture {
        _temp: tempfile::TempDir,
        workspace: PathBuf,
        config: PathBuf,
        desktop: PathBuf,
        file: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let workspace = temp.path().join("Açores project_with.dots");
            fs::create_dir(&workspace).unwrap();
            let workspace = fs::canonicalize(workspace).unwrap();
            let config = temp.path().join("config");
            let project = config.join("projects").join(project_key(&workspace));
            fs::create_dir_all(&project).unwrap();
            let desktop = temp.path().join("desktop");
            fs::create_dir_all(desktop.join("account/organization")).unwrap();
            Self {
                _temp: temp,
                workspace,
                config,
                desktop,
                file: project.join(format!("{SESSION}.jsonl")),
            }
        }

        fn store(&self) -> ClaudeNativeStore {
            ClaudeNativeStore::new(&self.config).with_desktop_sessions_root(&self.desktop)
        }
        fn root(&self) -> &str {
            self.workspace.to_str().unwrap()
        }
        fn record(&self, kind: &str, text: &str, uuid: Uuid) -> Value {
            json!({"type":kind,"uuid":uuid,"sessionId":SESSION,"cwd":self.root(),"timestamp":"2026-10-12T12:00:00.125Z","message":{"id":"same-provider-message-id","content":[{"type":"text","text":text}]}})
        }
        fn write(&self, records: &[Value]) {
            let mut file = File::create(&self.file).unwrap();
            for record in records {
                writeln!(file, "{record}").unwrap();
            }
        }
        fn append(&self, record: &Value) {
            writeln!(
                fs::OpenOptions::new()
                    .append(true)
                    .open(&self.file)
                    .unwrap(),
                "{record}"
            )
            .unwrap();
        }
        fn desktop_file(&self) -> PathBuf {
            self.desktop
                .join("account/organization/local_desktop-id.json")
        }
    }

    #[test]
    fn full_history_exceeds_the_old_cap_and_unchanged_files_reuse_caches() {
        let fixture = Fixture::new();
        let records: Vec<_> = (0..700)
            .map(|index| {
                fixture.record(
                    if index % 2 == 0 { "user" } else { "assistant" },
                    &format!("Message {index}"),
                    Uuid::new_v4(),
                )
            })
            .collect();
        fixture.write(&records);
        let store = fixture.store();
        let index = store.list(fixture.root()).unwrap();
        assert!(index.warnings.is_empty());
        assert_eq!(index.threads.len(), 1);
        let thread = store.read(fixture.root(), SESSION).unwrap();
        assert!(!thread.history_truncated);
        assert_eq!(thread.messages.len(), 700);
        assert_eq!(thread.messages[0].text_content(), "Message 0");
        assert_eq!(thread.messages[699].text_content(), "Message 699");
        assert_eq!(thread.entry, index.threads[0]);
        let parses = store.cache.lock().unwrap().parses;
        assert!(
            store
                .read_if_changed(fixture.root(), SESSION, Some(&thread.entry.revision))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store.read(fixture.root(), SESSION).unwrap().messages,
            thread.messages
        );
        assert_eq!(store.list(fixture.root()).unwrap().threads, index.threads);
        assert_eq!(store.cache.lock().unwrap().parses, parses);
        fixture.append(
            &json!({"type":"custom-title","sessionId":SESSION,"customTitle":"Latest name"}),
        );
        let updated = store.list(fixture.root()).unwrap();
        assert_eq!(updated.threads[0].summary.title, "Latest name");
        assert_ne!(updated.threads[0].revision, index.threads[0].revision);
        assert!(
            store
                .read_if_changed(fixture.root(), SESSION, Some(&thread.entry.revision))
                .unwrap()
                .is_some()
        );
        assert_eq!(
            store.read(fixture.root(), SESSION).unwrap().messages.len(),
            700
        );
    }

    #[test]
    fn native_titles_defaults_usage_and_content_order_are_preserved() {
        let fixture = Fixture::new();
        let mut user = fixture.record("user", "Question", Uuid::new_v4());
        user["permissionMode"] = json!("acceptEdits");
        let mut assistant = fixture.record("assistant", "First", Uuid::new_v4());
        assistant["effort"] = json!("high");
        assistant["message"]["model"] = json!("claude-sonnet-4-6");
        assistant["message"]["usage"] = json!({"input_tokens":10,"cache_creation_input_tokens":20,"cache_read_input_tokens":30,"output_tokens":5});
        assistant["message"]["content"] = json!([
            {"type":"text","text":"Before"},
            {"type":"tool_use","id":"tool-1","name":"Edit","input":{"file_path":"file.rs"}},
            {"type":"text","text":"After"}
        ]);
        let mut tool = fixture.record("user", "", Uuid::new_v4());
        tool["message"]["content"] = json!([{"type":"tool_result","tool_use_id":"tool-1","content":[{"type":"text","text":"line1"},{"type":"text","text":"line2"}],"is_error":false}]);
        tool["toolUseResult"] =
            json!({"structuredPatch":[{"oldStart":1,"newStart":1,"lines":["-old","+new"]}]});
        fixture.write(&[
            user,
            assistant,
            tool,
            json!({"type":"ai-title","aiTitle":"AI name"}),
            json!({"type":"custom-title","customTitle":"Custom name"}),
        ]);
        let thread = fixture.store().read(fixture.root(), SESSION).unwrap();
        assert_eq!(thread.entry.summary.title, "Custom name");
        assert_eq!(
            thread.entry.defaults.model_id.as_deref(),
            Some("claude-sonnet-4-6")
        );
        assert_eq!(thread.entry.defaults.effort.as_deref(), Some("high"));
        assert_eq!(
            thread.entry.defaults.agent_access,
            Some(AgentAccess::AcceptEdits)
        );
        assert_eq!(thread.current_context_tokens, Some(65));
        assert_eq!(
            thread
                .messages
                .iter()
                .map(|message| message.role)
                .collect::<Vec<_>>(),
            [MessageRole::User, MessageRole::Assistant, MessageRole::Tool]
        );
        assert!(
            matches!(&thread.messages[1].content[1], MessageContent::ToolUse { id, input, .. } if id == "tool-1" && input.contains("file.rs"))
        );
        assert!(
            matches!(&thread.messages[1].content[1], MessageContent::ToolUse { input, .. } if input.contains("_flowxStructuredPatch"))
        );
        assert!(
            matches!(&thread.messages[2].content[0], MessageContent::ToolResult { content, .. } if content == "line1\nline2")
        );
        assert_eq!(
            thread.messages[0].timestamp.to_rfc3339(),
            "2026-10-12T12:00:00.125+00:00"
        );
    }

    #[test]
    fn sidechains_meta_and_duplicate_events_do_not_pollute_main_dialogue() {
        let fixture = Fixture::new();
        let id = Uuid::new_v4();
        let first = fixture.record("user", "Question", id);
        let mut sidechain = fixture.record("assistant", "Subagent", Uuid::new_v4());
        sidechain["isSidechain"] = json!(true);
        sidechain["cwd"] = json!("/unrelated");
        sidechain["message"]["model"] = json!("side-model");
        let mut meta = fixture.record("user", "internal context", Uuid::new_v4());
        meta["isMeta"] = json!(true);
        let revised = fixture.record("user", "Revised question", id);
        let mut missing_id = fixture.record("assistant", "Answer", Uuid::new_v4());
        missing_id.as_object_mut().unwrap().remove("uuid");
        fixture.write(&[
            first,
            sidechain,
            meta,
            revised,
            missing_id.clone(),
            missing_id,
        ]);
        let store = fixture.store();
        let thread = store.read(fixture.root(), SESSION).unwrap();
        assert_eq!(thread.messages.len(), 2);
        assert_eq!(thread.messages[0].text_content(), "Revised question");
        assert_eq!(thread.messages[0].id, id);
        assert!(thread.entry.defaults.model_id.is_none());
        let reread = fixture.store().read(fixture.root(), SESSION).unwrap();
        assert_eq!(thread.messages, reread.messages);
    }

    #[test]
    fn desktop_archive_changes_revise_the_entry_without_reparsing_history() {
        let fixture = Fixture::new();
        fixture.write(&[fixture.record("user", "Question", Uuid::new_v4())]);
        fs::write(
            fixture.desktop_file(),
            json!({"cliSessionId":SESSION,"isArchived":true}).to_string(),
        )
        .unwrap();
        let store = fixture.store();
        let archived = store.read(fixture.root(), SESSION).unwrap();
        assert!(archived.entry.summary.is_archived);
        let parses = store.cache.lock().unwrap().parses;
        fs::write(fixture.desktop_file(), "{\"cliSessionId\":").unwrap();
        let partial_metadata = store.list(fixture.root()).unwrap();
        assert!(!partial_metadata.warnings.is_empty());
        assert!(partial_metadata.threads[0].summary.is_archived);
        assert_eq!(
            partial_metadata.threads[0].revision,
            archived.entry.revision
        );
        fs::write(
            fixture.desktop_file(),
            json!({"cliSessionId":SESSION,"isArchived":false}).to_string(),
        )
        .unwrap();
        let restored = store.read(fixture.root(), SESSION).unwrap();
        assert!(!restored.entry.summary.is_archived);
        assert_ne!(restored.entry.revision, archived.entry.revision);
        assert_eq!(store.cache.lock().unwrap().parses, parses);
        assert_eq!(restored.messages, archived.messages);
        fs::remove_file(fixture.desktop_file()).unwrap();
        assert!(
            !store.list(fixture.root()).unwrap().threads[0]
                .summary
                .is_archived
        );
    }

    #[test]
    fn incomplete_last_append_is_flagged_and_corrupt_complete_records_fail() {
        let fixture = Fixture::new();
        fixture.write(&[fixture.record("user", "Keep complete prefix", Uuid::new_v4())]);
        let next = fixture
            .record("assistant", "Next message", Uuid::new_v4())
            .to_string();
        let split = next.len() / 2;
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&fixture.file)
            .unwrap();
        file.write_all(&next.as_bytes()[..split]).unwrap();
        let store = fixture.store();
        let incomplete = store.read(fixture.root(), SESSION).unwrap();
        assert!(incomplete.history_truncated);
        assert_eq!(incomplete.messages.len(), 1);
        file.write_all(&next.as_bytes()[split..]).unwrap();
        let completed = store.read(fixture.root(), SESSION).unwrap();
        assert!(!completed.history_truncated);
        assert_eq!(completed.messages.len(), 2);
        file.write_all(b"\n{garbage}\n").unwrap();
        assert!(store.read(fixture.root(), SESSION).is_err());
        let index = store.list(fixture.root()).unwrap();
        assert!(index.threads.is_empty());
        assert_eq!(index.warnings.len(), 1);
    }

    #[test]
    fn image_and_structured_tool_result_payloads_survive_import() {
        let fixture = Fixture::new();
        let mut record = fixture.record("user", "", Uuid::new_v4());
        record["message"]["content"] = json!([
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AQID"}},
            {"type":"tool_result","tool_use_id":"call","content":{"result":"value","details":[1,2]},"is_error":true}
        ]);
        fixture.write(&[record]);
        let thread = fixture.store().read(fixture.root(), SESSION).unwrap();
        assert!(
            matches!(&thread.messages[0].content[0], MessageContent::Image { data, mime_type } if data == &[1,2,3] && mime_type == "image/png")
        );
        assert!(
            matches!(&thread.messages[0].content[1], MessageContent::ToolResult { content, is_error: true, .. } if serde_json::from_str::<Value>(content).unwrap()["details"] == json!([1,2]))
        );
    }

    #[test]
    fn limits_are_explicit_and_do_not_import_partial_history() {
        let fixture = Fixture::new();
        fixture.write(&[fixture.record("user", "Message", Uuid::new_v4())]);
        let mut store = fixture.store();
        store.maximum_transcript_bytes = 64;
        assert!(
            store
                .read(fixture.root(), SESSION)
                .unwrap_err()
                .to_string()
                .contains("64-byte history limit")
        );
        let index = store.list(fixture.root()).unwrap();
        assert!(index.threads.is_empty());
        assert_eq!(index.warnings.len(), 1);
        assert_eq!(store.cache.lock().unwrap().transcripts.len(), 0);
    }

    #[test]
    fn missing_config_is_empty_but_missing_workspace_is_an_error() {
        let fixture = Fixture::new();
        let store = ClaudeNativeStore::new(fixture.config.join("absent"));
        assert!(store.list(fixture.root()).unwrap().threads.is_empty());
        assert!(store.list("relative/path").is_err());
        assert!(
            store
                .list(fixture.workspace.join("absent").to_str().unwrap())
                .is_err()
        );
        for id in [
            "../session",
            "",
            "../../etc/passwd",
            "11111111222243338444555555555555",
        ] {
            assert!(store.read(fixture.root(), id).is_err());
        }
    }

    #[test]
    fn path_encoding_collisions_never_reuse_another_workspaces_cached_summary() {
        let fixture = Fixture::new();
        fixture.write(&[fixture.record("user", "Private to original", Uuid::new_v4())]);
        let collision = fixture.workspace.with_file_name("Açores-project-with-dots");
        fs::create_dir(&collision).unwrap();
        assert_eq!(project_key(&fixture.workspace), project_key(&collision));
        let store = fixture.store();
        assert_eq!(store.list(fixture.root()).unwrap().threads.len(), 1);
        let index = store.list(collision.to_str().unwrap()).unwrap();
        assert!(index.threads.is_empty());
        assert!(store.read(collision.to_str().unwrap(), SESSION).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn canonical_aliases_are_discovered_but_transcript_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        fixture.write(&[fixture.record("user", "Canonical session", Uuid::new_v4())]);
        let alias = fixture.workspace.with_file_name("alias");
        symlink(&fixture.workspace, &alias).unwrap();
        let store = fixture.store();
        assert_eq!(
            store.list(alias.to_str().unwrap()).unwrap().threads[0]
                .summary
                .working_directory,
            fixture.root()
        );
        let configured_project = fixture.config.join("projects").join(project_key(&alias));
        fs::create_dir(&configured_project).unwrap();
        fs::rename(
            &fixture.file,
            configured_project.join(format!("{SESSION}.jsonl")),
        )
        .unwrap();
        assert_eq!(
            store
                .read(alias.to_str().unwrap(), SESSION)
                .unwrap()
                .messages
                .len(),
            1
        );
        symlink(
            configured_project.join(format!("{SESSION}.jsonl")),
            &fixture.file,
        )
        .unwrap();
        assert!(store.read(fixture.root(), SESSION).is_err());
        assert!(store.list(fixture.root()).unwrap().threads.is_empty());
    }

    #[test]
    fn mismatched_session_or_workspace_records_are_never_imported() {
        let fixture = Fixture::new();
        let mut record = fixture.record("user", "Question", Uuid::new_v4());
        record["sessionId"] = json!(Uuid::new_v4());
        fixture.write(&[record]);
        assert!(fixture.store().read(fixture.root(), SESSION).is_err());
        fixture.write(&[json!({"type":"user","message":{"content":"No cwd"}})]);
        assert!(fixture.store().read(fixture.root(), SESSION).is_err());
    }

    #[test]
    fn a_session_is_the_workspace_it_started_in_wherever_it_went_later() {
        // A real thread: Claude desktop ran a side session from inside it, so
        // some records name its notes folder. The whole thread was refused.
        let fixture = Fixture::new();
        let first = fixture.record("user", "At project root", Uuid::new_v4());
        let mut side = fixture.record("assistant", "In a side session", Uuid::new_v4());
        side["cwd"] = json!(
            fixture
                .workspace
                .parent()
                .unwrap()
                .join("side-session-notes/gone")
        );
        let back = fixture.record("user", "Back at the root", Uuid::new_v4());
        fixture.write(&[first.clone(), side.clone(), back]);
        assert_eq!(
            fixture
                .store()
                .read(fixture.root(), SESSION)
                .unwrap()
                .messages
                .len(),
            3
        );
        // A session that started elsewhere is still not this workspace's,
        // even if it visits it later.
        fixture.write(&[side, first]);
        assert!(fixture.store().read(fixture.root(), SESSION).is_err());
    }

    #[test]
    fn main_session_may_visit_descendants_but_needs_exact_workspace_identity() {
        let fixture = Fixture::new();
        let first = fixture.record("user", "At project root", Uuid::new_v4());
        let mut nested = fixture.record("assistant", "Inside source folder", Uuid::new_v4());
        // The historical subdirectory can have been deleted since this turn.
        nested["cwd"] = json!(fixture.workspace.join("old-source/nested"));
        fixture.write(&[first.clone(), nested.clone()]);
        assert_eq!(
            fixture
                .store()
                .read(fixture.root(), SESSION)
                .unwrap()
                .messages
                .len(),
            2
        );
        fixture.write(&[nested]);
        assert!(fixture.store().read(fixture.root(), SESSION).is_err());
        let mut unrelated = fixture.record("assistant", "Elsewhere", Uuid::new_v4());
        unrelated["cwd"] = json!(fixture.workspace.parent().unwrap());
        fixture.write(&[unrelated, first]);
        assert!(fixture.store().read(fixture.root(), SESSION).is_err());
    }
}

/// Refuse to resume a provider transcript that another Claude process still
/// owns, even if that process is idle or waiting for approval. Hold FlowX's
/// cross-process session lease around this check and the subsequent turn.
///
/// Older Claude versions without this optional metadata command retain the
/// lease-only fallback. A supported command failing cannot prove inactivity.
pub async fn ensure_session_inactive(
    launch: &fx_core::services::runtime::RuntimeLaunch,
    prefix_arguments: &[String],
    session_id: &str,
) -> Result<(), ProviderError> {
    let id = safe_session_id(session_id).ok_or_else(|| protocol("Invalid Claude session ID"))?;
    if let Ok(result) = activity_command(launch, prefix_arguments, &["agents", "--json"]).await
        && result.success
        && !result.limited
        && let Ok(active) = active_session_ids(&result.stdout)
    {
        if active.contains(&id) {
            return Err(protocol(
                "This Claude session is still open in another Claude Code process. Continue there, or stop and exit that session before resuming it in FlowX.",
            ));
        }
        return Ok(());
    }
    let version_supported = activity_command(launch, prefix_arguments, &["--version"])
        .await
        .is_ok_and(|result| {
            result.success
                && !result.limited
                && activity_version_supported(&String::from_utf8_lossy(&result.stdout))
        });
    let help_supported = if version_supported {
        false
    } else {
        activity_command(launch, prefix_arguments, &["agents", "--help"])
            .await
            .is_ok_and(|result| {
                let help = format!(
                    "{}{}",
                    String::from_utf8_lossy(&result.stdout),
                    String::from_utf8_lossy(&result.stderr)
                )
                .to_ascii_lowercase();
                result.success
                    && !result.limited
                    && help.contains("--json")
                    && help.contains("active sessions")
            })
    };
    if version_supported || help_supported {
        Err(protocol(
            "FlowX could not verify whether this Claude session is active elsewhere. Retry before resuming it.",
        ))
    } else {
        Ok(())
    }
}

fn active_session_ids(data: &[u8]) -> Result<HashSet<String>, ProviderError> {
    let value: Value =
        serde_json::from_slice(data).map_err(|_| protocol("Invalid Claude activity response"))?;
    let rows = value
        .as_array()
        .ok_or_else(|| protocol("Claude activity response was not an array"))?;
    rows.iter()
        .map(|row| {
            row["sessionId"]
                .as_str()
                .and_then(|id| safe_session_id(id.trim()))
                .ok_or_else(|| protocol("Claude activity entry has no valid session ID"))
        })
        .collect()
}

fn activity_version_supported(version: &str) -> bool {
    version
        .split(|character: char| !character.is_ascii_digit() && character != '.')
        .find_map(|part| {
            let parts: Vec<_> = part.split('.').take(3).map(str::parse::<u64>).collect();
            (parts.len() == 3 && parts.iter().all(Result::is_ok)).then(|| {
                (
                    parts[0].as_ref().copied().unwrap(),
                    parts[1].as_ref().copied().unwrap(),
                    parts[2].as_ref().copied().unwrap(),
                )
            })
        })
        .is_some_and(|version| version >= (2, 1, 145))
}

struct ActivityOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    success: bool,
    limited: bool,
}

async fn activity_command(
    launch: &fx_core::services::runtime::RuntimeLaunch,
    prefix_arguments: &[String],
    arguments: &[&str],
) -> Result<ActivityOutput, ProviderError> {
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    const OUTPUT_LIMIT: usize = 1024 * 1024;
    let mut child = tokio::process::Command::new(&launch.executable)
        .args(prefix_arguments)
        .args(arguments)
        .env_clear()
        .envs(&launch.environment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let read = async {
        let out = async {
            let mut bytes = Vec::new();
            stdout
                .take((OUTPUT_LIMIT + 1) as u64)
                .read_to_end(&mut bytes)
                .await?;
            Ok::<_, std::io::Error>(bytes)
        };
        let err = async {
            let mut bytes = Vec::new();
            stderr
                .take((OUTPUT_LIMIT + 1) as u64)
                .read_to_end(&mut bytes)
                .await?;
            Ok::<_, std::io::Error>(bytes)
        };
        let (stdout, stderr, status) = tokio::try_join!(out, err, child.wait())?;
        Ok::<_, std::io::Error>(ActivityOutput {
            limited: stdout.len() > OUTPUT_LIMIT || stderr.len() > OUTPUT_LIMIT,
            stdout,
            stderr,
            success: status.success(),
        })
    };
    match tokio::time::timeout(Duration::from_secs(3), read).await {
        Ok(result) => result.map_err(ProviderError::Io),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(protocol("Claude activity query timed out"))
        }
    }
}

#[cfg(test)]
mod activity_tests {
    use super::*;
    use fx_core::services::runtime::RuntimeLaunch;
    use serde_json::json;

    const ID: &str = "11111111-2222-4333-8444-555555555555";

    fn runtime(
        temp: &tempfile::TempDir,
        status: &str,
        version: &str,
        help: &str,
    ) -> (RuntimeLaunch, Vec<String>) {
        let script = temp.path().join("activity.py");
        fs::write(&script, format!("import sys\nargs=sys.argv[1:]\nif args==['agents','--json']: print({status:?})\nelif args==['--version']: print({version:?})\nelif args==['agents','--help']: print({help:?})\nelse: sys.exit(2)\n")).unwrap();
        (
            RuntimeLaunch {
                executable: PathBuf::from("/usr/bin/python3"),
                environment: Default::default(),
            },
            vec![script.to_string_lossy().into_owned()],
        )
    }

    #[tokio::test]
    async fn active_idle_and_blocked_native_sessions_all_refuse_a_second_writer() {
        let temp = tempfile::tempdir().unwrap();
        for status in ["running", "idle", "blocked"] {
            let payload = json!([{"cwd":"/fixture","kind":"foreground","name":"fixture","pid":1,"sessionId":ID,"startedAt":1,"status":status}]).to_string();
            let (launch, args) = runtime(&temp, &payload, "2.1.145 (Claude Code)", "");
            let error = ensure_session_inactive(&launch, &args, ID)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("still open"));
        }
        let (launch, args) = runtime(&temp, "[]", "2.1.145 (Claude Code)", "");
        ensure_session_inactive(&launch, &args, ID).await.unwrap();
    }

    #[tokio::test]
    async fn supported_but_unreadable_activity_fails_closed_while_old_versions_work() {
        let temp = tempfile::tempdir().unwrap();
        let (launch, args) = runtime(&temp, "not JSON", "2.1.145 (Claude Code)", "");
        assert!(
            ensure_session_inactive(&launch, &args, ID)
                .await
                .unwrap_err()
                .to_string()
                .contains("could not verify")
        );
        let (launch, args) = runtime(&temp, "{}", "unknown", "List active sessions --json");
        assert!(ensure_session_inactive(&launch, &args, ID).await.is_err());
        let (launch, args) = runtime(
            &temp,
            "unknown command",
            "2.1.144 (Claude Code)",
            "unknown command",
        );
        ensure_session_inactive(&launch, &args, ID).await.unwrap();
    }

    #[test]
    fn activity_ids_are_exact_and_version_detection_uses_numeric_components() {
        let ids = active_session_ids(
            json!([{"sessionId":ID.to_uppercase(),"status":"idle"}])
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert!(ids.contains(ID));
        assert!(!ids.contains(&ID[..8]));
        assert!(active_session_ids(br#"[{"sessionID":"wrong key"}]"#).is_err());
        assert!(!activity_version_supported("2.1.9"));
        assert!(!activity_version_supported("2.1.144"));
        assert!(activity_version_supported("2.1.145 (Claude Code)"));
        assert!(activity_version_supported("2.2.0"));
    }
}
